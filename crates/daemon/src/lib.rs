//! `shimmer-daemon`: the one long-lived process that owns all state (CLAUDE.md §2).
//!
//! Core services live here: module registry, event bus, queue, IPC server, indexer.
//! Modules are async tasks inside this process, reached only through `Ctx`.
//!
//! `ctx.http` is the real gateway (`http_backend`, ADR 0027) for a module whose manifest
//! declares `"network"`; every other module keeps `Ctx::new`'s default, `DisabledHttp`.

mod backend;
mod bus;
mod config;
mod core;
mod delivery;
mod desktop;
mod http_backend;
mod indexer;
mod ipc;
// Linux/macOS only for now (ADR 0010 §7); the module itself documents the scoping.
#[cfg(unix)]
mod launcher;
mod queue;
mod registry;
mod scheduler;
mod secrets;

use std::fs::{self, File};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use shimmer_core::{Clock, Emitter, Error, Module, ModuleId, NamespacedStore, Result};
use shimmer_store::Store;
use tokio::net::UnixListener;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::backend::Backend;
use crate::bus::Bus;
use crate::core::Core;
use crate::registry::Registry;
use crate::scheduler::Scheduler;

/// In-flight connections and tasks get this long to finish after shutdown is requested.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
/// How long a starting daemon waits for a busy `.daemon.lock`. Covers the last instants of a
/// daemon that's just shut down (#243); a daemon that's really running is still refused.
const LOCK_WAIT: Duration = Duration::from_secs(2);
const LOCK_POLL: Duration = Duration::from_millis(20);

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
        let lock = lock_home(store.home()).await?;
        let recovered = store.recover()?;
        if recovered != Default::default() {
            tracing::info!(?recovered, "recovered interrupted transactions");
        }

        let cfg = config::Config::load(store.home())?;
        let (secrets, secrets_problem) = secrets::load_chain();
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
        // Opened before `Core::new` so `core.events.list` (`crates/daemon/src/core.rs`) has a
        // handle to query from the moment the daemon starts answering requests.
        let index = indexer::open(store.clone()).await?;
        let core = Core::new(
            registry,
            lanes,
            &cfg,
            backend,
            config.clock.clone(),
            shutdown.clone(),
            &config.socket,
            secrets,
            secrets_problem,
            index.clone(),
        );
        let tracker = TaskTracker::new();

        // Subscribe before init so events a module emits while initialising are not missed.
        // Taken before subscribing: a module's catch-up after a lag never reaches before it (#310).
        let listening_since = config.clock.now();
        let receivers: Vec<_> = core.registry.entries.iter().map(|_| bus.subscribe()).collect();
        let recovery_index = index.clone();
        tracker.spawn(indexer::run(index, store.clone(), bus.subscribe(), shutdown.clone()));

        for (entry, rx) in core.registry.entries.iter().zip(receivers) {
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
            // Every event, in order, a lag caught up from the log and a panic kept to its event
            // (#261 findings 1, 2).
            tracker.spawn(delivery::run(
                id,
                entry.module.clone(),
                ctx,
                rx,
                store.clone(),
                shutdown.clone(),
                listening_since,
                tracker.clone(),
            ));
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
            cfg.local_timezone,
        ));
        scheduler.load()?;
        scheduler.register_module_triggers(declared)?;
        // Firings a stop left unfinished: queued again if they never started (#263 finding 1).
        // A failure here is logged, not fatal: the firings stay on disk for the next start.
        let events = recovery_index.clone();
        if let Err(e) = scheduler.recover(|pattern, since| {
            events.lock().unwrap_or_else(|e| e.into_inner()).events_matching_since(pattern, since)
        }) {
            tracing::error!(error = %e, "couldn't check unfinished scheduled tasks; checked again next start");
        }
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
    /// grace period expired). Removes the socket file, then releases `.daemon.lock`.
    ///
    /// The order is what makes a restart safe (#243). Everything is torn down first (the index
    /// closes when `core` drops), so the socket going away means the daemon really is done. The
    /// lock is released right after, not whenever the process exits: anything that sees the
    /// socket gone and starts a new daemon finds the lock free. Releasing it *before* removing
    /// the socket would let a new daemon bind first and have its socket deleted by this one.
    pub async fn wait(self) {
        let Daemon { core, socket, tracker, _lock: lock } = self;
        core.shutdown.cancelled().await;
        tracker.close();
        core.queue.tracker.close();
        let drain = async {
            tracker.wait().await;
            core.queue.tracker.wait().await;
        };
        if tokio::time::timeout(SHUTDOWN_GRACE, drain).await.is_err() {
            tracing::warn!("shutdown grace period expired with work still running");
        }
        drop(core);
        let _ = fs::remove_file(&socket);
        let _ = lock.unlock();
    }
}

/// Exclusive and advisory. `Daemon::wait` releases it on shutdown, and the OS when the process
/// dies. A busy lock is retried for [`LOCK_WAIT`]: a daemon that has just removed its socket
/// may not have released the lock yet (#243), and refusing then fails a quick restart.
async fn lock_home(home: &Path) -> Result<File> {
    let file = File::create(home.join(".daemon.lock"))?;
    let deadline = tokio::time::Instant::now() + LOCK_WAIT;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(fs::TryLockError::WouldBlock) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(LOCK_POLL).await;
            }
            Err(fs::TryLockError::WouldBlock) => {
                return Err(Error::conflict(format!("another daemon already owns {}", home.display())));
            }
            Err(fs::TryLockError::Error(e)) => return Err(e.into()),
        }
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
