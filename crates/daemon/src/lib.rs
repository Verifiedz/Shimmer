//! `shimmer-daemon`: the one long-lived process that owns all state (CLAUDE.md §2).
//!
//! Core services live here: module registry, event bus, queue, IPC server, indexer.
//! Modules are async tasks inside this process, reached only through `Ctx`.
//!
//! Not built yet: the HTTP gateway; `ctx.http` fails closed.

mod backend;
mod bus;
mod config;
mod core;
mod indexer;
mod ipc;
// Linux/macOS only for now (ADR 0010 §7); the module itself documents the scoping.
#[cfg(unix)]
mod launcher;
mod queue;
mod registry;
mod scheduler;

use std::fs::{self, File};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use shimmer_core::{Clock, Emitter, Error, Module, ModuleId, NamespacedStore, Result};
use shimmer_store::Store;
use tokio::net::UnixListener;
use tokio::sync::broadcast::error::RecvError;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::backend::Backend;
use crate::bus::Bus;
use crate::core::Core;
use crate::registry::Registry;
use crate::scheduler::Scheduler;

/// In-flight connections and tasks get this long to finish after shutdown is requested.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

pub struct DaemonConfig {
    /// `$SHIMMER_HOME`.
    pub home: PathBuf,
    pub socket: PathBuf,
    pub clock: Clock,
    /// How often the scheduler looks at the clock. Firing precision is one tick.
    pub scheduler_tick: Duration,
}

impl DaemonConfig {
    /// Paths from the environment, per `docs/protocol.md`.
    pub fn from_env() -> Self {
        Self {
            home: shimmer_proto::paths::shimmer_home(),
            socket: shimmer_proto::paths::socket_path(),
            clock: Clock::system(),
            scheduler_tick: Duration::from_secs(1),
        }
    }
}

#[derive(Clone)]
pub struct ShutdownHandle(CancellationToken);

impl ShutdownHandle {
    pub fn shutdown(&self) {
        self.0.cancel();
    }
}

/// A started daemon. Serving happens on background tasks; [`wait`](Self::wait) blocks until
/// shutdown is requested (by [`shutdown`](Self::shutdown) or a `core.shutdown` request).
pub struct Daemon {
    core: Arc<Core>,
    socket: PathBuf,
    tracker: TaskTracker,
    _lock: File,
}

impl Daemon {
    pub async fn start(config: DaemonConfig, modules: Vec<Arc<dyn Module>>) -> Result<Self> {
        let store = Arc::new(Store::open(&config.home)?);
        // One writer: a second daemon on the same home would corrupt the ordering guarantees.
        let lock = lock_home(store.home())?;
        let recovered = store.recover()?;
        if recovered != Default::default() {
            tracing::info!(?recovered, "recovered interrupted transactions");
        }

        let cfg = config::Config::load(store.home())?;
        let registry = Registry::build(modules)?;
        let lanes = registry.lanes(&cfg)?;
        let declared: Vec<_> = registry
            .entries
            .iter()
            .flat_map(|e| e.module.triggers().into_iter().map(|t| (e.manifest.id.clone(), t)))
            .collect();
        let known_ops = registry.entries.iter().flat_map(|e| &e.commands).map(|c| c.op.clone()).collect();
        let lane_ids = lanes.iter().map(|l| l.id.clone()).collect();

        let shutdown = CancellationToken::new();
        let bus = Bus::new();
        let backend = Arc::new(Backend { store: store.clone(), bus: bus.clone() });
        let core = Core::new(registry, lanes, &cfg, backend, config.clock.clone(), shutdown.clone(), &config.socket);
        let tracker = TaskTracker::new();

        // Subscribe before init so events a module emits while initialising are not missed.
        let receivers: Vec<_> = core.registry.entries.iter().map(|_| bus.subscribe()).collect();
        let index = indexer::open(store.clone()).await?;
        tracker.spawn(indexer::run(index, store.clone(), bus.subscribe(), shutdown.clone()));

        for (entry, mut rx) in core.registry.entries.iter().zip(receivers) {
            let id = entry.manifest.id.clone();
            let ctx = core.ctxs[&id].clone();
            match entry.module.init(&ctx).await {
                Ok(()) => entry.ready.store(true, Ordering::Release),
                Err(e) => {
                    // One broken module must not take the daemon down: its ops answer
                    // `unavailable` and everything else keeps working.
                    tracing::error!(module = %id, error = %e, "module init failed; ops will answer unavailable");
                    continue;
                }
            }
            let (module, shutdown) = (entry.module.clone(), shutdown.clone());
            tracker.spawn(async move {
                loop {
                    let ev = tokio::select! {
                        _ = shutdown.cancelled() => return,
                        ev = rx.recv() => ev,
                    };
                    match ev {
                        Ok(ev) => {
                            if let Err(e) = module.on_event(&ev, &ctx).await {
                                tracing::warn!(module = %id, topic = %ev.topic, error = %e, "on_event failed");
                            }
                        }
                        Err(RecvError::Lagged(n)) => tracing::warn!(module = %id, dropped = n, "module lagged the bus"),
                        Err(RecvError::Closed) => return,
                    }
                }
            });
        }

        // Modules are initialised, so anything that fires now can run. The first tick catches up
        // whatever came due while the daemon was down.
        let sched_id = ModuleId::new("scheduler");
        let scheduler = Arc::new(Scheduler::new(
            NamespacedStore::new("scheduler", sched_id.clone(), core.backend.clone(), config.clock.clone()),
            Emitter::new(sched_id, core.backend.clone(), config.clock.clone()),
            config.clock.clone(),
            core.queue.clone(),
            known_ops,
            lane_ids,
        ));
        scheduler.load()?;
        scheduler.register_module_triggers(declared)?;
        let _ = core.scheduler.set(scheduler.clone());
        tracker.spawn(scheduler.run(config.scheduler_tick, shutdown.clone()));

        let listener = bind(&config.socket)?;
        tracker.spawn(ipc::serve(core.clone(), listener, tracker.clone()));
        tracing::info!(socket = %config.socket.display(), home = %store.home().display(), "daemon ready");
        Ok(Self { core, socket: config.socket, tracker, _lock: lock })
    }

    /// A handle a signal handler can hold after `wait` has taken the daemon.
    pub fn shutdown_handle(&self) -> ShutdownHandle {
        ShutdownHandle(self.core.shutdown.clone())
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket
    }

    /// Ask the daemon to stop. Returns immediately; use [`wait`](Self::wait) to join.
    pub fn shutdown(&self) {
        self.core.shutdown.cancel();
    }

    /// Resolves after shutdown is requested and connections and tasks have drained (or the
    /// grace period expired). Removes the socket file.
    pub async fn wait(self) {
        self.core.shutdown.cancelled().await;
        self.tracker.close();
        self.core.queue.tracker.close();
        let drain = async {
            self.tracker.wait().await;
            self.core.queue.tracker.wait().await;
        };
        if tokio::time::timeout(SHUTDOWN_GRACE, drain).await.is_err() {
            tracing::warn!("shutdown grace period expired with work still running");
        }
        let _ = fs::remove_file(&self.socket);
    }
}

/// Exclusive, advisory, released by the OS when the process dies.
fn lock_home(home: &Path) -> Result<File> {
    let file = File::create(home.join(".daemon.lock"))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(fs::TryLockError::WouldBlock) => {
            Err(Error::conflict(format!("another daemon already owns {}", home.display())))
        }
        Err(fs::TryLockError::Error(e)) => Err(e.into()),
    }
}

/// Bind the socket, clearing a stale one left by a daemon that died without cleanup.
fn bind(path: &Path) -> Result<UnixListener> {
    if let Some(parent) = path.parent().filter(|p| !p.exists()) {
        fs::DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
    }
    if path.exists() {
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            return Err(Error::conflict(format!("a daemon is already listening on {}", path.display())));
        }
        fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}
