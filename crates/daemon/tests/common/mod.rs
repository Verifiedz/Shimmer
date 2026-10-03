//! Shared harness for the socket tests: a demo module, a raw protocol client, and a daemon
//! environment. Speaks only `shimmer-proto` frames, exactly what any client sees.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use serde_json::{json, Value};
use shimmer_core::{
    Clock, CommandSpec, Ctx, Error, ErrorCode, Event, Execution, LaneConfig, Manifest, Module, Result, TriggerSpec,
};
use shimmer_daemon::{Daemon, DaemonConfig};
use shimmer_proto::{decode_server, ServerFrame};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

// ---------------------------------------------------------------- a module to drive

#[derive(Default)]
pub struct Demo {
    pub triggers: Vec<TriggerSpec>,
}

pub fn spec(op: &str, execution: Execution) -> CommandSpec {
    CommandSpec { op: op.into(), summary: op.into(), params_schema: json!({}), execution }
}

#[async_trait]
impl Module for Demo {
    fn manifest(&self) -> Manifest {
        Manifest {
            id: "demo".into(),
            version: "0.0.1".into(),
            namespace: "demo".into(),
            topics: vec!["demo.item.saved".into()],
            capabilities: vec![],
        }
    }

    async fn init(&self, _ctx: &Ctx) -> Result<()> {
        Ok(())
    }

    fn lanes(&self) -> Vec<LaneConfig> {
        vec![LaneConfig::new("slow", 1)]
    }

    fn triggers(&self) -> Vec<TriggerSpec> {
        self.triggers.clone()
    }

    fn commands(&self) -> Vec<CommandSpec> {
        let slow = || Execution::Queued { lane: "slow".into() };
        vec![
            spec("demo.echo", Execution::Inline),
            spec("demo.put", Execution::Inline),
            spec("demo.get", Execution::Inline),
            spec("demo.panic", Execution::Inline),
            spec("demo.quick", slow()),
            spec("demo.fail", slow()),
            spec("demo.slow_fail", slow()),
            spec("demo.bad", slow()),
            spec("demo.dirty", slow()),
            spec("demo.spin", slow()),
        ]
    }

    async fn handle(&self, op: &str, params: Value, ctx: &Ctx) -> Result<Value> {
        match op {
            "demo.echo" => Ok(params),
            "demo.put" => {
                let (key, val) = (params["key"].as_str().unwrap_or("k"), params["val"].as_str().unwrap_or(""));
                ctx.store.transaction(|tx| {
                    tx.put(&format!("items/{key}.txt"), val)?;
                    tx.emit("demo.item.saved", json!({"key": key}))
                })?;
                Ok(json!({"saved": key}))
            }
            "demo.get" => {
                let key = params["key"].as_str().unwrap_or("k");
                Ok(json!({"val": ctx.store.read_string(&format!("items/{key}.txt"))?}))
            }
            "demo.panic" => panic!("module bug"),
            "demo.quick" => Ok(json!("quick done")),
            "demo.fail" => Err(Error::unavailable("upstream down")),
            // Runs and reports a problem of its own: not a structural failure (§11.3).
            "demo.bad" => Err(Error::module_error("ran, and found a problem")),
            // What `workspaces.activate` will return for a half-configured workspace; the detail
            // is the shape `docs/protocol.md` specifies for `workspace_dirty`.
            "demo.dirty" => {
                Err(Error::new(ErrorCode::WorkspaceDirty, "workspace 'deep-work' failed setup and was not cleaned up")
                    .with_detail(json!({
                        "workspace": "deep-work", "failed_step": "3/7 tmux-session",
                        "failed_at": "2026-09-16T09:12:44Z", "log": "logs/deep-work-01JD2T.log",
                        "has_cleanup_script": true
                    })))
            }
            // Long enough that a task enqueued right after it is still waiting when it fails.
            "demo.slow_fail" => {
                tokio::time::sleep(Duration::from_millis(100)).await;
                Err(Error::unavailable("upstream down, eventually"))
            }
            "demo.spin" => {
                for i in 0..1000 {
                    if ctx.cancel.is_cancelled() {
                        return Err(Error::module_error("stopped"));
                    }
                    ctx.progress(i as f32 / 1000.0, "spinning");
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                Ok(json!("spun"))
            }
            _ => Err(Error::unknown_op(op)),
        }
    }
}

/// Stands in for the `notify` module (M6): a `notify` lane and two ops. `notify.send` echoes
/// its params, so a test sees exactly what the queue handed the fallback; `notify.fail` fails
/// structurally, to prove a failing fallback ends there.
pub struct Notify;

#[async_trait]
impl Module for Notify {
    fn manifest(&self) -> Manifest {
        Manifest {
            id: "notify".into(),
            version: "0.0.1".into(),
            namespace: "notify".into(),
            topics: vec![],
            capabilities: vec![],
        }
    }

    async fn init(&self, _ctx: &Ctx) -> Result<()> {
        Ok(())
    }

    fn lanes(&self) -> Vec<LaneConfig> {
        vec![LaneConfig::new("notify", 2)]
    }

    fn commands(&self) -> Vec<CommandSpec> {
        let lane = || Execution::Queued { lane: "notify".into() };
        vec![spec("notify.send", lane()), spec("notify.fail", lane())]
    }

    async fn handle(&self, op: &str, params: Value, _ctx: &Ctx) -> Result<Value> {
        match op {
            "notify.send" => Ok(params),
            "notify.fail" => Err(Error::unavailable("no sink reachable")),
            _ => Err(Error::unknown_op(op)),
        }
    }
}

/// Its manifest's `capabilities` is configurable so a test can start the *same* op logic
/// with and without `"process"` declared, to prove `Core::new`'s capability-scoped
/// `ctx.launcher` wiring (ADR 0010 §4) rather than anything about spawn behaviour itself
/// (covered by `crates/daemon/src/launcher.rs`'s own tests).
pub struct ProcessUser {
    pub declares_capability: bool,
}

#[async_trait]
impl Module for ProcessUser {
    fn manifest(&self) -> Manifest {
        Manifest {
            id: "procuser".into(),
            version: "0.0.1".into(),
            namespace: "procuser".into(),
            topics: vec![],
            capabilities: if self.declares_capability { vec!["process".into()] } else { vec![] },
        }
    }

    async fn init(&self, _ctx: &Ctx) -> Result<()> {
        Ok(())
    }

    fn commands(&self) -> Vec<CommandSpec> {
        vec![spec("procuser.launch", Execution::Inline)]
    }

    async fn handle(&self, op: &str, params: Value, ctx: &Ctx) -> Result<Value> {
        match op {
            "procuser.launch" => {
                let workspace_dir = params["workspace_dir"].as_str().unwrap_or("deep-work").to_string();
                let step = shimmer_core::LaunchStep {
                    workspace_id: workspace_dir.clone(),
                    workspace_dir,
                    step: shimmer_core::Step::Cleanup,
                    mode: shimmer_core::SpawnMode::Detached,
                    user_env: vec![],
                };
                let outcome = ctx.launcher.run(&step, &ctx.cancel).await?;
                Ok(json!({"session_id_is_empty": outcome.session_id.is_empty()}))
            }
            _ => Err(Error::unknown_op(op)),
        }
    }
}

// ---------------------------------------------------------------- a raw client

pub struct Client {
    pub rd: BufReader<OwnedReadHalf>,
    pub wr: OwnedWriteHalf,
    pub n: u32,
    /// Events that arrived while waiting for something else.
    pub events: VecDeque<Event>,
}

pub const T: Duration = Duration::from_secs(5);

impl Client {
    pub async fn raw(sock: &Path) -> Self {
        let (rd, wr) = UnixStream::connect(sock).await.unwrap().into_split();
        Self { rd: BufReader::new(rd), wr, n: 0, events: VecDeque::new() }
    }

    pub async fn connect(sock: &Path) -> Self {
        let mut c = Self::raw(sock).await;
        c.send_json(json!({"v":1,"kind":"hello","client":"test","client_version":"0"})).await;
        assert!(matches!(c.recv().await, Some(ServerFrame::Welcome { v: 1, .. })));
        c
    }

    pub async fn send_json(&mut self, v: Value) {
        self.wr.write_all(format!("{v}\n").as_bytes()).await.unwrap();
    }

    pub async fn recv(&mut self) -> Option<ServerFrame> {
        let mut line = String::new();
        let n = tokio::time::timeout(T, self.rd.read_line(&mut line)).await.expect("timed out").unwrap();
        (n > 0).then(|| decode_server(&line).unwrap())
    }

    /// Send a request and return its response; events seen on the way are kept.
    pub async fn call(&mut self, op: &str, params: Value) -> std::result::Result<Value, Error> {
        self.call_with(op, params, None).await
    }

    pub async fn call_with(
        &mut self,
        op: &str,
        params: Value,
        queue: Option<Value>,
    ) -> std::result::Result<Value, Error> {
        self.n += 1;
        let id = format!("r{}", self.n);
        let mut req = json!({"v":1,"kind":"request","id":id,"op":op,"params":params});
        if let Some(q) = queue {
            req["queue"] = q;
        }
        self.send_json(req).await;
        self.response(&id).await
    }

    pub async fn response(&mut self, want: &str) -> std::result::Result<Value, Error> {
        loop {
            match self.recv().await.expect("connection closed") {
                ServerFrame::Response { id, ok, data, error, .. } if id == want => {
                    return if ok { Ok(data.unwrap()) } else { Err(error.unwrap()) };
                }
                ServerFrame::Event { event, .. } => self.events.push_back(event),
                other => panic!("unexpected frame {other:?}"),
            }
        }
    }

    pub async fn subscribe(&mut self, topics: &[&str]) {
        self.n += 1;
        let id = format!("s{}", self.n);
        self.send_json(json!({"v":1,"kind":"subscribe","id":id,"topics":topics})).await;
        self.response(&id).await.unwrap();
    }

    /// Next event with this topic, whatever else arrives first.
    pub async fn event(&mut self, topic: &str) -> Event {
        loop {
            if let Some(i) = self.events.iter().position(|e| e.topic == topic) {
                return self.events.remove(i).unwrap();
            }
            match self.recv().await.expect("connection closed") {
                ServerFrame::Event { event, .. } => self.events.push_back(event),
                other => panic!("unexpected frame {other:?}"),
            }
        }
    }
}

// ---------------------------------------------------------------- harness

pub struct Env {
    pub home: TempDir,
    pub sock: PathBuf,
    pub clock: Clock,
}

impl Env {
    pub fn new() -> Self {
        Self::with_clock(Clock::system())
    }

    /// Starts at a known instant and only moves when the test advances it.
    pub fn fake_clock() -> Self {
        Self::with_clock(Clock::fake(t0()))
    }

    fn with_clock(clock: Clock) -> Self {
        let home = TempDir::new().unwrap();
        let sock = home.path().join("daemon.sock");
        Self { home, sock, clock }
    }

    pub async fn start(&self) -> Daemon {
        self.start_with(Demo::default()).await
    }

    /// `demo` plus a `notify` module, for fallbacks.
    pub async fn start_with_notify(&self) -> Daemon {
        self.try_start(vec![Arc::new(Demo::default()), Arc::new(Notify)]).await.unwrap()
    }

    pub async fn start_with(&self, demo: Demo) -> Daemon {
        self.try_start(vec![Arc::new(demo)]).await.unwrap()
    }

    pub async fn try_start(&self, modules: Vec<Arc<dyn Module>>) -> Result<Daemon> {
        Daemon::start(
            DaemonConfig {
                home: self.home.path().to_owned(),
                socket: self.sock.clone(),
                clock: self.clock.clone(),
                scheduler_tick: Duration::from_millis(10),
            },
            modules,
        )
        .await
    }
}

pub async fn stop(d: Daemon) {
    d.shutdown();
    d.wait().await;
}

pub fn task_id(v: &Value) -> String {
    assert_eq!(v["queued"], true, "{v}");
    v["task_id"].as_str().unwrap().to_owned()
}

pub fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 21, 8, 0, 0).unwrap()
}
