//! One connection to the daemon, speaking `docs/protocol.md`: `hello` first, then requests
//! matched to responses by id, and optionally a subscription to pushed events. Every failure,
//! including a dead socket, is a `shimmer_core::Error` so the caller handles one error type and
//! branches on `code`.

use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use shimmer_core::{Error, Event, Result};
use shimmer_proto::{decode_server, encode, ClientFrame, QueueControl, ServerFrame, PROTOCOL_VERSION};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// Inline ops are fast and queued ops answer with a task handle at once, so a response
/// slower than this means the daemon is wedged, not busy.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Client {
    rd: BufReader<OwnedReadHalf>,
    wr: OwnedWriteHalf,
    next_id: u64,
    /// True after [`subscribe`](Self::subscribe): events are kept, not skipped.
    subscribed: bool,
    /// Events that arrived while waiting for a response, oldest first.
    events: VecDeque<Event>,
    /// What the daemon's `welcome` said it is.
    daemon_version: String,
}

impl Client {
    /// Connect and complete the handshake. The raw `io::Error` from connecting is kept apart
    /// so the caller can tell "nobody is listening" (start a daemon) from everything else.
    pub async fn connect(socket: &Path) -> std::result::Result<Self, ConnectError> {
        Self::connect_for(socket, &shimmer_proto::paths::shimmer_home()).await
    }

    /// [`connect`](Self::connect), refusing a daemon that serves a folder other than `home`
    /// (ADR 0039 §3). A daemon that names no folder (older, or `shimmer mockd`) is accepted.
    pub async fn connect_for(socket: &Path, home: &Path) -> std::result::Result<Self, ConnectError> {
        if let Some(why) = shimmer_proto::paths::too_long(socket) {
            return Err(ConnectError::Protocol(Error::invalid_params(why)));
        }
        // A socket in the shared `/tmp` folder is only trusted if that folder is ours (ADR 0039 §5).
        let uid = shimmer_proto::paths::owner_uid();
        if let Some(dir) = socket.parent().filter(|_| shimmer_proto::paths::in_short_socket_dir(socket, uid)) {
            if dir.exists() {
                shimmer_proto::paths::check_private_dir(dir, uid)
                    .map_err(|why| ConnectError::Protocol(Error::unavailable(why)))?;
            }
        }
        let stream = UnixStream::connect(socket).await.map_err(ConnectError::Io)?;
        let (rd, wr) = stream.into_split();
        let mut client = Self {
            rd: BufReader::new(rd),
            wr,
            next_id: 0,
            subscribed: false,
            events: VecDeque::new(),
            daemon_version: String::new(),
        };
        let served = client.handshake().await.map_err(ConnectError::Protocol)?;
        if let Some(served) = served {
            let mine = shimmer_proto::paths::resolve(home);
            if Path::new(&served) != mine {
                return Err(ConnectError::OtherHome { served, mine });
            }
        }
        Ok(client)
    }

    /// The folder the daemon said it serves, if it said.
    async fn handshake(&mut self) -> Result<Option<String>> {
        let hello = ClientFrame::Hello {
            v: PROTOCOL_VERSION,
            client: "cli".into(),
            client_version: env!("CARGO_PKG_VERSION").into(),
        };
        self.send(&hello).await?;
        let frame = tokio::time::timeout(HANDSHAKE_TIMEOUT, self.recv())
            .await
            .map_err(|_| Error::unavailable("daemon did not answer the handshake"))??;
        match frame {
            ServerFrame::Welcome { daemon_version, home, .. } => {
                self.daemon_version = daemon_version;
                Ok(home)
            }
            ServerFrame::Error { error, .. } => Err(error),
            other => Err(Error::bad_request(format!("expected welcome, got {}", kind(&other)))),
        }
    }

    /// The version the daemon gave in its `welcome`.
    pub fn daemon_version(&self) -> &str {
        &self.daemon_version
    }

    /// Send one request and wait for its response. Events arriving meanwhile are kept for
    /// [`next_event`](Self::next_event) once subscribed, and skipped before that.
    pub async fn call(&mut self, op: &str, params: Value) -> Result<Value> {
        self.call_queued(op, params, None).await
    }

    /// [`call`](Self::call) with the request frame's `queue` block (docs/protocol.md, "Queue
    /// control on the request frame"): only `--first` sends one (#222).
    pub async fn call_queued(&mut self, op: &str, params: Value, queue: Option<QueueControl>) -> Result<Value> {
        let id = self.fresh_id();
        let request = ClientFrame::Request { v: PROTOCOL_VERSION, id: id.clone(), op: op.to_owned(), params, queue };
        self.send(&request).await?;
        tokio::time::timeout(REQUEST_TIMEOUT, self.response(&id))
            .await
            .map_err(|_| Error::unavailable(format!("no response to '{op}' after {}s", REQUEST_TIMEOUT.as_secs())))?
    }

    /// Ask the daemon to push events matching `topics` (docs/protocol.md, `subscribe`). Call
    /// it before the request whose events you want, so none can be missed.
    pub async fn subscribe(&mut self, topics: &[&str]) -> Result<()> {
        let id = self.fresh_id();
        let frame = ClientFrame::Subscribe {
            v: PROTOCOL_VERSION,
            id: id.clone(),
            topics: topics.iter().map(|t| t.to_string()).collect(),
        };
        self.send(&frame).await?;
        self.subscribed = true;
        tokio::time::timeout(REQUEST_TIMEOUT, self.response(&id))
            .await
            .map_err(|_| Error::unavailable("daemon did not acknowledge the subscription"))?
            .map(drop)
    }

    /// The next pushed event, waiting as long as it takes: a launch can run for minutes. Only
    /// after [`subscribe`](Self::subscribe).
    pub async fn next_event(&mut self) -> Result<Event> {
        if let Some(event) = self.events.pop_front() {
            return Ok(event);
        }
        loop {
            match self.recv().await? {
                ServerFrame::Event { event, .. } => return Ok(event),
                ServerFrame::Error { error, .. } => return Err(error),
                // A late response to something else: not what we're waiting for.
                ServerFrame::Welcome { .. } | ServerFrame::Response { .. } => continue,
            }
        }
    }

    fn fresh_id(&mut self) -> String {
        self.next_id += 1;
        self.next_id.to_string()
    }

    async fn response(&mut self, id: &str) -> Result<Value> {
        loop {
            match self.recv().await? {
                ServerFrame::Response { id: got, ok, data, error, .. } if got == id => {
                    return match (ok, error) {
                        (true, _) => Ok(data.unwrap_or(Value::Null)),
                        (false, Some(e)) => Err(e),
                        (false, None) => Err(Error::internal("daemon sent ok:false with no error")),
                    };
                }
                ServerFrame::Error { error, .. } => return Err(error),
                ServerFrame::Event { event, .. } if self.subscribed => self.events.push_back(event),
                _ => continue,
            }
        }
    }

    async fn send(&mut self, frame: &ClientFrame) -> Result<()> {
        let line = encode(frame)?;
        self.wr.write_all(line.as_bytes()).await.map_err(lost)?;
        self.wr.flush().await.map_err(lost)
    }

    async fn recv(&mut self) -> Result<ServerFrame> {
        let mut line = String::new();
        match self.rd.read_line(&mut line).await.map_err(lost)? {
            0 => Err(Error::unavailable("daemon closed the connection")),
            _ => decode_server(&line),
        }
    }
}

#[derive(Debug)]
pub enum ConnectError {
    /// The socket could not be opened at all.
    Io(io::Error),
    /// Something answered but the handshake failed (wrong version, not a daemon, timeout).
    Protocol(Error),
    /// A daemon answered, but for another Shimmer folder (ADR 0039 §3).
    OtherHome { served: String, mine: PathBuf },
}

impl ConnectError {
    /// No daemon behind this path: no socket file, or a stale one left by a crash.
    pub fn nobody_listening(&self) -> bool {
        matches!(self, Self::Io(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused))
    }

    pub fn into_error(self, socket: &Path) -> Error {
        match self {
            Self::Io(e) => Error::unavailable(format!("cannot connect to {}: {e}", socket.display())),
            Self::Protocol(e) => e,
            Self::OtherHome { served, mine } => Error::unavailable(format!(
                "the daemon on {} serves {served}, not {}; set SHIMMER_SOCKET to that folder's daemon, or unset it",
                socket.display(),
                mine.display()
            )),
        }
    }
}

fn lost(e: io::Error) -> Error {
    Error::unavailable(format!("lost connection to daemon: {e}"))
}

fn kind(frame: &ServerFrame) -> &'static str {
    match frame {
        ServerFrame::Welcome { .. } => "welcome",
        ServerFrame::Response { .. } => "response",
        ServerFrame::Event { .. } => "event",
        ServerFrame::Error { .. } => "error",
    }
}
