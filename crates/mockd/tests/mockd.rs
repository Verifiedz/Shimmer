//! The mock daemon over a real socket, driven the way a client would drive it, using the
//! fixtures that ship in `crates/mockd/fixtures`: the ones Dev C builds the TUI against.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};
use shimmer_core::{Error, ErrorCode, Event};
use shimmer_mockd::{Mockd, Options};
use shimmer_proto::{decode_server, ManifestData, QueuedHandle, ServerFrame};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

const T: Duration = Duration::from_secs(5);

fn shipped() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

struct Env {
    dir: TempDir,
}

impl Env {
    fn new() -> Self {
        Self { dir: TempDir::new().unwrap() }
    }

    fn sock(&self) -> PathBuf {
        self.dir.path().join("mock.sock")
    }

    async fn start(&self) -> Mockd {
        self.start_with(shipped()).await
    }

    async fn start_with(&self, fixtures: PathBuf) -> Mockd {
        Mockd::start(Options { fixtures, socket: self.sock() }).await.unwrap()
    }
}

async fn stop(m: Mockd) {
    m.shutdown();
    m.wait().await;
}

struct Client {
    rd: BufReader<OwnedReadHalf>,
    wr: OwnedWriteHalf,
    n: u32,
    events: VecDeque<Event>,
}

impl Client {
    async fn raw(sock: &Path) -> Self {
        let (rd, wr) = UnixStream::connect(sock).await.unwrap().into_split();
        Self { rd: BufReader::new(rd), wr, n: 0, events: VecDeque::new() }
    }

    async fn connect(sock: &Path) -> Self {
        let mut c = Self::raw(sock).await;
        c.send(json!({"v":2,"kind":"hello","client":"test","client_version":"0"})).await;
        assert!(matches!(c.recv().await, Some(ServerFrame::Welcome { v: 2, .. })));
        c
    }

    async fn send(&mut self, v: Value) {
        self.wr.write_all(format!("{v}\n").as_bytes()).await.unwrap();
    }

    async fn recv(&mut self) -> Option<ServerFrame> {
        let mut line = String::new();
        let n = tokio::time::timeout(T, self.rd.read_line(&mut line)).await.expect("timed out").unwrap();
        (n > 0).then(|| decode_server(&line).unwrap())
    }

    async fn call(&mut self, op: &str, params: Value) -> Result<Value, Error> {
        self.call_with(op, params, None).await
    }

    async fn call_with(&mut self, op: &str, params: Value, queue: Option<Value>) -> Result<Value, Error> {
        self.n += 1;
        let id = format!("r{}", self.n);
        let mut req = json!({"v":2,"kind":"request","id":id,"op":op,"params":params});
        if let Some(q) = queue {
            req["queue"] = q;
        }
        self.send(req).await;
        self.response(&id).await
    }

    async fn response(&mut self, want: &str) -> Result<Value, Error> {
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

    async fn subscribe(&mut self, topics: &[&str]) {
        self.n += 1;
        let id = format!("s{}", self.n);
        self.send(json!({"v":2,"kind":"subscribe","id":id,"topics":topics})).await;
        self.response(&id).await.unwrap();
    }

    /// The next `n` events, in stream order.
    async fn events(&mut self, n: usize) -> Vec<Event> {
        let mut out = Vec::new();
        while out.len() < n {
            if let Some(e) = self.events.pop_front() {
                out.push(e);
                continue;
            }
            match self.recv().await.expect("connection closed") {
                ServerFrame::Event { event, .. } => out.push(event),
                other => panic!("unexpected frame {other:?}"),
            }
        }
        out
    }
}

fn topics(events: &[Event]) -> Vec<&str> {
    events.iter().map(|e| e.topic.as_str()).collect()
}

// ------------------------------------------------------------------ the basics

#[tokio::test]
async fn handshake_ping_and_a_manifest_that_parses_as_the_wire_type() {
    let env = Env::new();
    let m = env.start().await;
    let mut c = Client::connect(&env.sock()).await;

    assert_eq!(c.call("core.ping", json!({})).await.unwrap()["pong"], true, "built in, needs no fixture");

    let manifest = c.call("core.manifest", json!({})).await.unwrap();
    let parsed: ManifestData = serde_json::from_value(manifest).expect("manifest matches shimmer_proto::ManifestData");
    assert_eq!(parsed.protocol, shimmer_proto::PROTOCOL_VERSION);
    let ids: Vec<_> = parsed.modules.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["records", "fetchers", "workspaces", "notify"]);
    let lanes: Vec<_> = parsed.lanes.iter().map(|l| (l.id.as_str(), l.max_concurrent)).collect();
    assert!(lanes.contains(&("workspaces", 1)) && lanes.contains(&("fetchers", 8)), "{lanes:?}");
    stop(m).await;
}

#[tokio::test]
async fn it_behaves_like_the_daemon_at_the_edges() {
    let env = Env::new();
    let m = env.start().await;

    // A wrong version is refused with the same error, and the connection closes.
    let mut c = Client::raw(&env.sock()).await;
    c.send(json!({"v":9,"kind":"hello","client":"t","client_version":"0"})).await;
    let Some(ServerFrame::Error { error, .. }) = c.recv().await else { panic!("expected an error frame") };
    assert_eq!(error.code, ErrorCode::UnsupportedVersion);
    assert_eq!(error.detail.unwrap()["supported"], json!([shimmer_proto::PROTOCOL_VERSION]));
    assert!(c.recv().await.is_none());

    // Anything before hello is refused.
    let mut c = Client::raw(&env.sock()).await;
    c.send(json!({"v":2,"kind":"request","id":"1","op":"core.ping","params":{}})).await;
    assert!(matches!(c.recv().await, Some(ServerFrame::Error { .. })));

    // A malformed line is reported and the connection survives.
    let mut c = Client::connect(&env.sock()).await;
    c.wr.write_all(b"this is not json\n").await.unwrap();
    let Some(ServerFrame::Error { error, .. }) = c.recv().await else { panic!("expected an error frame") };
    assert_eq!(error.code, ErrorCode::BadRequest);
    assert_eq!(c.call("core.ping", json!({})).await.unwrap()["pong"], true);

    // Aliases never appear on the wire; unknown ops and gaps in fixtures are told apart.
    assert_eq!(c.call("bankai", json!({})).await.unwrap_err().code, ErrorCode::UnknownOp);
    let gap = c.call("records.list", json!({"collection": "unheard-of"})).await.unwrap_err();
    assert_eq!(gap.code, ErrorCode::InvalidParams);
    assert!(gap.message.contains("none match"), "{}", gap.message);

    // Bad subscribe patterns are refused as they are by the daemon.
    c.send(json!({"v":2,"kind":"subscribe","id":"bad","topics":["rec*"]})).await;
    assert_eq!(c.response("bad").await.unwrap_err().code, ErrorCode::InvalidParams);
    stop(m).await;
}

#[tokio::test]
async fn requests_are_pipelined_and_matched_by_id() {
    let env = Env::new();
    let m = env.start().await;
    let mut c = Client::connect(&env.sock()).await;
    for i in 0..5 {
        c.send(json!({"v":2,"kind":"request","id":format!("p{i}"),"op":"core.ping","params":{}})).await;
    }
    for i in 0..5 {
        assert_eq!(c.response(&format!("p{i}")).await.unwrap()["pong"], true);
    }
    stop(m).await;
}

#[tokio::test]
async fn core_shutdown_replies_then_stops_and_removes_the_socket() {
    let env = Env::new();
    let m = env.start().await;
    let mut c = Client::connect(&env.sock()).await;
    assert_eq!(c.call("core.shutdown", json!({})).await.unwrap()["ok"], true);
    tokio::time::timeout(T, m.wait()).await.expect("mockd did not stop");
    assert!(!env.sock().exists());
}

#[tokio::test]
async fn it_will_not_take_over_a_live_socket_but_replaces_a_stale_one() {
    let env = Env::new();
    let first = env.start().await;
    let err = Mockd::start(Options { fixtures: shipped(), socket: env.sock() }).await.err().expect("must refuse");
    assert_eq!(err.code, ErrorCode::Conflict);
    stop(first).await;

    // A crash leaves the socket file behind with nobody listening; that is replaced.
    let stale = env.dir.path().join("stale.sock");
    drop(std::os::unix::net::UnixListener::bind(&stale).unwrap());
    assert!(stale.exists());
    let second = Mockd::start(Options { fixtures: shipped(), socket: stale }).await.unwrap();
    stop(second).await;
}

#[tokio::test]
async fn a_bad_fixture_fails_startup_naming_the_file() {
    let env = Env::new();
    let fixtures = TempDir::new().unwrap();
    std::fs::write(fixtures.path().join("broken.json"), r#"{"responses":[{"op":"a.b"}]}"#).unwrap();
    let err = Mockd::start(Options { fixtures: fixtures.path().into(), socket: env.sock() }).await.err().unwrap();
    assert!(err.message.contains("broken.json"), "{}", err.message);
    assert!(!env.sock().exists(), "and nothing is left listening");
}

// ------------------------------------------------------------------ the four hard paths

/// protocol.md, "Priority promotion": refused with what it would cost, then accepted once
/// the client echoes the queue_version it was shown.
#[tokio::test]
async fn confirmation_round_trip() {
    let env = Env::new();
    let m = env.start().await;
    let mut c = Client::connect(&env.sock()).await;
    c.subscribe(&["queue.*"]).await;
    let params = json!({"source": "linkedin"});

    let refused = c.call_with("fetchers.run", params.clone(), Some(json!({"priority": "override"}))).await;
    let refused = refused.unwrap_err();
    assert_eq!(refused.code, ErrorCode::ConfirmationRequired);
    let detail = refused.detail.unwrap();
    assert_eq!(detail["lane"], "fetchers");
    assert_eq!(detail["queue_version"], 41);
    assert_eq!(detail["would_displace"].as_array().unwrap().len(), 3);
    assert_eq!(detail["would_displace"][0]["priority"], "scheduled");

    // Confirming a version the user was not shown is refused again, as the daemon would.
    let stale = json!({"priority": "override", "confirm": true, "queue_version": 40});
    let again = c.call_with("fetchers.run", params.clone(), Some(stale)).await.unwrap_err();
    assert_eq!(again.code, ErrorCode::ConfirmationRequired);

    let confirm = json!({"priority": "override", "confirm": true, "queue_version": 41});
    let granted = c.call_with("fetchers.run", params.clone(), Some(confirm)).await.unwrap();
    let handle: QueuedHandle = serde_json::from_value(granted).expect("a queued handle");
    assert_eq!((handle.lane.as_str(), handle.position), ("fetchers", 0));
    let promoted = c.events(1).await.remove(0);
    assert_eq!(promoted.topic, "queue.task.promoted");
    assert_eq!(promoted.payload["task_id"], handle.task_id.as_str());
    assert_eq!(promoted.payload["displaced"].as_array().unwrap().len(), 3);

    // An ordinary request just joins the lane.
    let plain = c.call("fetchers.run", params).await.unwrap();
    assert_eq!(plain["position"], 4);
    stop(m).await;
}

#[tokio::test]
async fn workspace_dirty_refusal_and_a_failing_launch() {
    let env = Env::new();
    let m = env.start().await;
    let mut c = Client::connect(&env.sock()).await;
    c.subscribe(&["queue.task.*", "workspaces.*"]).await;

    let dirty = c.call("workspaces.activate", json!({"id": "deep-work"})).await.unwrap_err();
    assert_eq!(dirty.code, ErrorCode::WorkspaceDirty);
    let d = dirty.detail.unwrap();
    assert_eq!(d["failed_step"], "3/7 tmux-session");
    assert_eq!(d["log"], "logs/deep-work-01JD2T.log");
    assert_eq!(d["has_cleanup_script"], true);

    // A healthy workspace accepts the launch, and then the task fails: a distinct path.
    let handle = c.call("workspaces.activate", json!({"id": "focus"})).await.unwrap();
    assert_eq!(handle["lane"], "workspaces");
    let seen = c.events(3).await;
    assert_eq!(topics(&seen), ["queue.task.started", "workspaces.session.dirty", "queue.task.failed"]);
    assert_eq!(seen[2].payload["code"], "workspace_dirty");
    assert_eq!(seen[2].payload["task_id"], handle["task_id"]);
    stop(m).await;
}

#[tokio::test]
async fn a_long_queued_task_reports_progress_in_order() {
    let env = Env::new();
    let m = env.start().await;
    let mut c = Client::connect(&env.sock()).await;
    c.subscribe(&["queue.*", "fetchers.*"]).await;

    let handle = c.call("fetchers.run", json!({"source": "hn-whoishiring"})).await.unwrap();
    let task = handle["task_id"].as_str().unwrap().to_owned();
    assert_eq!(handle["queued"], true, "the result arrives later, as events");

    let seen = c.events(9).await;
    assert_eq!(seen.first().unwrap().topic, "queue.task.started");
    assert_eq!(seen.last().unwrap().topic, "queue.task.finished");
    let fractions: Vec<f64> = seen
        .iter()
        .filter(|e| e.topic == "queue.task.progress")
        .map(|e| {
            assert_eq!(e.payload["task_id"], task.as_str());
            e.payload["fraction"].as_f64().unwrap()
        })
        .collect();
    assert_eq!(fractions, [0.2, 0.4, 0.6, 0.8, 1.0]);
    assert!(topics(&seen).contains(&"fetchers.item.found"));
    assert_eq!(seen.last().unwrap().payload["result"]["new_items"], 12);
    assert!(seen.windows(2).all(|w| w[0].at <= w[1].at), "stream order is time order");
    stop(m).await;
}

#[tokio::test]
async fn the_timeline_replays_a_lagged_stream_only_to_subscribers() {
    let env = Env::new();
    let m = env.start().await;

    let mut c = Client::connect(&env.sock()).await;
    c.subscribe(&["records.*", "core.*"]).await;
    let seen = c.events(4).await;
    assert_eq!(
        topics(&seen),
        ["records.item.created", "records.item.updated", "core.stream.lagged", "records.item.completed"]
    );
    assert_eq!(seen[2].payload["dropped"], 37);
    assert_eq!(seen[2].source.as_str(), "core");

    // Filtered like the real stream: a subscriber to `core.*` sees only the lag notice, and one
    // who never subscribed sees nothing at all.
    let mut narrow = Client::connect(&env.sock()).await;
    narrow.subscribe(&["core.*"]).await;
    assert_eq!(topics(&narrow.events(1).await), ["core.stream.lagged"]);
    let mut silent = Client::connect(&env.sock()).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(silent.call("core.ping", json!({})).await.unwrap()["pong"], true);
    assert!(silent.events.is_empty());
    stop(m).await;
}
