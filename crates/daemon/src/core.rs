//! The daemon's shared state and request routing. IPC frames arrive here as
//! `(op, params, queue control)` and leave as a `Result<Value>`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock, Weak};
use std::time::Instant;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use shimmer_core::{
    Clock, Ctx, Emitter, Error, Execution, HttpGateway, LaneConfig, LaneId, ModuleConfig, ModuleId, NamespacedStore,
    Origin, Priority, ProgressFn, QueueHandle, Result,
};
use shimmer_core::{EnqueueRequest, TaskId, TaskSubmitter};
#[cfg(unix)]
use shimmer_core::{LaunchBackend, Launcher};
use shimmer_proto::{ops, ManifestData, ModuleInfo, PingData, QueueControl, QueuePriority, QueuedHandle};
use tokio_util::sync::CancellationToken;

use crate::backend::{Backend, DisabledHttp};
use crate::config::Config;
use crate::queue::{OpRunner, Promotion, QueueFront, QueueInner};
use crate::registry::Registry;
use crate::scheduler::Scheduler;

pub struct Core {
    pub registry: Registry,
    pub ctxs: HashMap<ModuleId, Ctx>,
    pub queue: Arc<QueueInner>,
    pub backend: Arc<Backend>,
    pub lanes: Vec<LaneConfig>,
    pub started: Instant,
    pub shutdown: CancellationToken,
    /// Set once during startup, after modules are initialised.
    pub scheduler: OnceLock<Arc<Scheduler>>,
}

impl Core {
    /// Wire every module to its capabilities. Infallible: all validation happened when the
    /// registry and lanes were built.
    pub fn new(
        registry: Registry,
        lanes: Vec<LaneConfig>,
        config: &Config,
        backend: Arc<Backend>,
        clock: Clock,
        shutdown: CancellationToken,
        socket: &Path,
    ) -> Arc<Self> {
        // A module gets a populated, usable `Launcher` only if its manifest declares the
        // `"process"` capability (ADR 0010 §4); every other module keeps `Ctx::new`'s
        // default `Launcher::unavailable`, the same fails-closed posture `ctx.http` already
        // has. One real backend, shared across every such module: `instance_id` (ADR 0010
        // §9's amendment) only needs to be collision-resistant per daemon process, not per
        // module, and sharing it means every minted session id in one daemon run draws from
        // the same sequence, not a separate one per module.
        #[cfg(unix)]
        let real_launcher: Arc<dyn LaunchBackend> = Arc::new(crate::launcher::RealLaunchBackend::new(
            backend.store.home().to_path_buf(),
            socket.to_path_buf(),
            clock.clone(),
            rand::random(),
        ));

        Arc::new_cyclic(|weak: &Weak<Core>| {
            let ops = registry
                .entries
                .iter()
                .flat_map(|e| &e.commands)
                .map(|c| {
                    let lane = match &c.execution {
                        Execution::Queued { lane } => Some(lane.clone()),
                        Execution::Inline => None,
                    };
                    (c.op.clone(), lane)
                })
                .collect();
            let runner: Weak<dyn OpRunner> = weak.clone();
            let queue = QueueInner::new(&lanes, ops, backend.clone(), clock.clone(), runner, shutdown.clone());
            let submitter: Arc<dyn TaskSubmitter> = Arc::new(QueueFront::new(&queue));

            let ctxs = registry
                .entries
                .iter()
                .map(|e| {
                    let id = e.manifest.id.clone();
                    #[allow(unused_mut)]
                    let mut ctx = Ctx::new(
                        NamespacedStore::new(e.manifest.namespace.clone(), id.clone(), backend.clone(), clock.clone()),
                        HttpGateway::new(id.clone(), Arc::new(DisabledHttp)),
                        Emitter::new(id.clone(), backend.clone(), clock.clone()),
                        QueueHandle::new(id.clone(), submitter.clone()),
                        clock.clone(),
                        config.local_timezone,
                        shutdown.child_token(),
                        ModuleConfig::new(config.modules.get(id.as_str()).cloned().unwrap_or_else(|| json!({}))),
                        id.clone(),
                    );
                    #[cfg(unix)]
                    if e.manifest.capabilities.iter().any(|c| c == "process") {
                        ctx.launcher = Launcher::new(real_launcher.clone());
                    }
                    (id, ctx)
                })
                .collect();

            Core {
                registry,
                ctxs,
                queue,
                backend,
                lanes,
                started: Instant::now(),
                shutdown,
                scheduler: OnceLock::new(),
            }
        })
    }

    pub async fn handle_request(&self, op: &str, params: Value, ctl: Option<QueueControl>) -> Result<Value> {
        match op {
            ops::CORE_PING => {
                Ok(serde_json::to_value(PingData { pong: true, uptime_s: self.started.elapsed().as_secs() })
                    .unwrap_or_default())
            }
            ops::CORE_MANIFEST => Ok(serde_json::to_value(self.manifest()).unwrap_or_default()),
            // The IPC layer cancels `shutdown` after this response is on the wire.
            ops::CORE_SHUTDOWN => Ok(json!({"ok": true})),
            ops::QUEUE_LIST => {
                #[derive(Deserialize, Default)]
                #[serde(default)]
                struct P {
                    lane: Option<LaneId>,
                }
                let p: P = parse(params)?;
                self.queue.list(p.lane.as_ref())
            }
            ops::QUEUE_TASK => self.queue.task_view(task_id(params)?),
            ops::QUEUE_CANCEL => self.queue.cancel(task_id(params)?),
            ops::QUEUE_REORDER => {
                #[derive(Deserialize)]
                struct P {
                    lane: LaneId,
                    task_id: String,
                    before: Option<String>,
                    queue_version: u64,
                }
                let p: P = parse(params)?;
                let id = p.task_id.parse()?;
                let before = p.before.map(|s| s.parse()).transpose()?;
                self.queue.reorder(&p.lane, id, before, p.queue_version)
            }
            _ if op.starts_with("scheduler.") => match self.scheduler.get() {
                Some(s) => s.handle(op, params),
                None => Err(Error::unavailable("scheduler is not running")),
            },
            _ => self.module_op(op, params, ctl).await,
        }
    }

    async fn module_op(&self, op: &str, params: Value, ctl: Option<QueueControl>) -> Result<Value> {
        let (entry, spec) =
            self.registry.command(op).ok_or_else(|| Error::unknown_op(format!("no module registered op '{op}'")))?;
        if !entry.ready.load(Ordering::Acquire) {
            return Err(Error::unavailable(format!("module '{}' is not initialised", entry.manifest.id)));
        }
        match &spec.execution {
            Execution::Inline => self.run_inline(op, params, &entry.manifest.id).await,
            Execution::Queued { .. } => {
                let ctl = ctl.unwrap_or_default();
                let request = EnqueueRequest {
                    op: op.to_owned(),
                    params,
                    lane: None,
                    priority: Priority::Normal,
                    origin: Origin::User,
                    fallback: None,
                };
                let handle = match ctl.priority {
                    QueuePriority::Override => self.queue.submit_promoted(
                        request,
                        Promotion { confirm: ctl.confirm, queue_version: ctl.queue_version },
                    )?,
                    _ => self.queue.submit_request(request)?,
                };
                Ok(serde_json::to_value(QueuedHandle {
                    queued: true,
                    task_id: handle.task_id.to_string(),
                    lane: handle.lane.to_string(),
                    position: handle.position,
                })
                .unwrap_or_default())
            }
        }
    }

    async fn run_inline(&self, op: &str, params: Value, module: &ModuleId) -> Result<Value> {
        let entry = self.registry.entry(module).ok_or_else(|| Error::internal("module vanished"))?;
        let ctx = self.ctxs.get(module).cloned().ok_or_else(|| Error::internal("module has no ctx"))?;
        let (module, op_owned) = (entry.module.clone(), op.to_owned());
        // Own task: a panicking handler becomes an `internal` error, not a dead connection.
        tokio::spawn(async move { module.handle(&op_owned, params, &ctx).await }).await.unwrap_or_else(|e| {
            tracing::error!(op, error = %e, backtrace = %std::backtrace::Backtrace::capture(), "handler panicked");
            Err(Error::internal(format!("handler for '{op}' panicked")))
        })
    }

    pub fn manifest(&self) -> ManifestData {
        ManifestData {
            protocol: shimmer_proto::PROTOCOL_VERSION,
            lanes: self.lanes.clone(),
            modules: self
                .registry
                .entries
                .iter()
                .map(|e| ModuleInfo {
                    id: e.manifest.id.to_string(),
                    version: e.manifest.version.clone(),
                    namespace: e.manifest.namespace.clone(),
                    commands: e.commands.clone(),
                    topics: e.manifest.topics.clone(),
                })
                .collect(),
        }
    }
}

#[async_trait]
impl OpRunner for Core {
    async fn run_op(&self, op: &str, params: Value, cancel: CancellationToken, progress: ProgressFn) -> Result<Value> {
        let (entry, _) =
            self.registry.command(op).ok_or_else(|| Error::unknown_op(format!("no module registered op '{op}'")))?;
        if !entry.ready.load(Ordering::Acquire) {
            return Err(Error::unavailable(format!("module '{}' is not initialised", entry.manifest.id)));
        }
        let ctx = self
            .ctxs
            .get(&entry.manifest.id)
            .ok_or_else(|| Error::internal("module has no ctx"))?
            .for_task(cancel, progress);
        entry.module.handle(op, params, &ctx).await
    }
}

fn parse<T: for<'de> Deserialize<'de>>(params: Value) -> Result<T> {
    // `params` may be absent on the wire, which decodes as null.
    let params = if params.is_null() { json!({}) } else { params };
    serde_json::from_value(params).map_err(|e| Error::invalid_params(e.to_string()))
}

fn task_id(params: Value) -> Result<TaskId> {
    #[derive(Deserialize)]
    struct P {
        task_id: String,
    }
    parse::<P>(params)?.task_id.parse()
}
