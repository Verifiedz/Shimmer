//! The Unix-socket server: newline-delimited JSON, handshake, pipelined requests, and a
//! subscription stream. Everything on the wire is defined by `docs/protocol.md`.

use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use shimmer_core::{Error, Event, ModuleId};
use shimmer_proto::{
    decode_client, encode, is_valid_pattern, ops, topic_matches, topics, ClientFrame, ServerFrame, MAX_LINE_BYTES,
    PROTOCOL_VERSION,
};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, Semaphore};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::core::Core;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Requests in flight per connection. Reading pauses beyond this, which is the backpressure.
const MAX_IN_FLIGHT: usize = 64;

pub async fn serve(core: Arc<Core>, listener: UnixListener, tracker: TaskTracker) {
    loop {
        tokio::select! {
            _ = core.shutdown.cancelled() => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let (core, t) = (core.clone(), tracker.clone());
                    tracker.spawn(async move {
                        if let Err(e) = connection(core, stream, t).await {
                            tracing::debug!(error = %e, "connection ended with an I/O error");
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!(error = %e, "accept failed");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
    }
}

enum Line {
    Eof,
    Text(Vec<u8>),
    TooLong,
}

/// `read_until` with a ceiling, so a peer that never sends a newline cannot eat memory.
async fn read_line<R: AsyncBufRead + Unpin>(r: &mut R) -> io::Result<Line> {
    let mut buf = Vec::new();
    loop {
        let available = r.fill_buf().await?;
        if available.is_empty() {
            return Ok(if buf.is_empty() { Line::Eof } else { Line::Text(buf) });
        }
        let newline = available.iter().position(|&b| b == b'\n');
        let take = newline.unwrap_or(available.len());
        buf.extend_from_slice(&available[..take]);
        r.consume(newline.map_or(take, |i| i + 1));
        if buf.len() > MAX_LINE_BYTES {
            return Ok(Line::TooLong);
        }
        if newline.is_some() {
            return Ok(Line::Text(buf));
        }
    }
}

async fn write_frame(w: &mut OwnedWriteHalf, frame: &ServerFrame) -> io::Result<()> {
    let line = encode(frame).map_err(io::Error::other)?;
    w.write_all(line.as_bytes()).await?;
    w.flush().await
}

async fn connection(core: Arc<Core>, stream: UnixStream, tracker: TaskTracker) -> io::Result<()> {
    let (rd, mut wr) = stream.into_split();
    let mut rd = BufReader::new(rd);

    // Handshake: `hello` first, before anything else.
    let first = tokio::time::timeout(HANDSHAKE_TIMEOUT, read_line(&mut rd))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "no hello"))??;
    let hello = match first {
        Line::Eof => return Ok(()),
        Line::TooLong => {
            return write_frame(&mut wr, &ServerFrame::error(Error::bad_request("line too long"))).await;
        }
        Line::Text(bytes) => {
            std::str::from_utf8(&bytes).map_err(|_| Error::bad_request("frame is not UTF-8")).and_then(decode_client)
        }
    };
    match hello {
        Ok(ClientFrame::Hello { v, .. }) if v == PROTOCOL_VERSION => {}
        Ok(ClientFrame::Hello { v, .. }) => {
            let e = Error::new(
                shimmer_core::ErrorCode::UnsupportedVersion,
                format!("protocol v{v} not supported; this daemon speaks v{PROTOCOL_VERSION}"),
            )
            .with_detail(json!({"supported": [PROTOCOL_VERSION]}));
            return write_frame(&mut wr, &ServerFrame::error(e)).await;
        }
        Ok(_) => return write_frame(&mut wr, &ServerFrame::error(Error::bad_request("expected hello"))).await,
        Err(e) => return write_frame(&mut wr, &ServerFrame::error(e)).await,
    }
    let session = ulid::Ulid::new().to_string();
    write_frame(&mut wr, &ServerFrame::welcome_for(env!("CARGO_PKG_VERSION"), session, core.backend.store.home()))
        .await?;

    let subs: Arc<Mutex<Vec<String>>> = Arc::default();
    let (out_tx, out_rx) = mpsc::channel::<ServerFrame>(256);
    let writer = tracker.spawn(writer(wr, out_rx, core.backend.bus.subscribe(), subs.clone(), core.shutdown.clone()));

    let in_flight = Arc::new(Semaphore::new(MAX_IN_FLIGHT));
    loop {
        let line = tokio::select! {
            _ = core.shutdown.cancelled() => break,
            l = read_line(&mut rd) => l?,
        };
        let bytes = match line {
            Line::Eof => break,
            Line::TooLong => {
                let _ = out_tx.send(ServerFrame::error(Error::bad_request("line too long"))).await;
                break;
            }
            Line::Text(b) if b.iter().all(u8::is_ascii_whitespace) => continue,
            Line::Text(b) => b,
        };
        let frame =
            std::str::from_utf8(&bytes).map_err(|_| Error::bad_request("frame is not UTF-8")).and_then(decode_client);
        match frame {
            Err(e) => {
                let _ = out_tx.send(ServerFrame::error(e)).await;
            }
            Ok(ClientFrame::Hello { .. }) => {
                let _ = out_tx.send(ServerFrame::error(Error::bad_request("already greeted"))).await;
            }
            Ok(ClientFrame::Request { id, op, params, queue, .. }) => {
                let Ok(permit) = in_flight.clone().acquire_owned().await else { break };
                let (core, out) = (core.clone(), out_tx.clone());
                tracker.spawn(async move {
                    let result = core.handle_request(&op, params, queue).await;
                    let ok = result.is_ok();
                    let frame = match result {
                        Ok(data) => ServerFrame::ok(id, data),
                        Err(e) => ServerFrame::err(id, e),
                    };
                    let _ = out.send(frame).await;
                    drop(permit);
                    // Cancel only after the response is queued; the writer drains it first.
                    if ok && op == ops::CORE_SHUTDOWN {
                        core.shutdown.cancel();
                    }
                });
            }
            Ok(ClientFrame::Subscribe { id, topics, .. }) => {
                let reply = match topics.iter().find(|t| !is_valid_pattern(t)) {
                    Some(bad) => ServerFrame::err(id, Error::invalid_params(format!("invalid topic pattern '{bad}'"))),
                    None => {
                        let total = update_subs(&subs, |s| {
                            for t in topics {
                                if !s.contains(&t) {
                                    s.push(t);
                                }
                            }
                        });
                        ServerFrame::ok(id, json!({"subscribed": total}))
                    }
                };
                let _ = out_tx.send(reply).await;
            }
            Ok(ClientFrame::Unsubscribe { id, topics, .. }) => {
                let total = update_subs(&subs, |s| s.retain(|p| !topics.contains(p)));
                let _ = out_tx.send(ServerFrame::ok(id, json!({"subscribed": total}))).await;
            }
        }
    }
    // Dropping our sender lets the writer finish once in-flight handlers have replied.
    drop(out_tx);
    let _ = writer.await;
    Ok(())
}

/// Mutate the subscription list and return its new length. Synchronous on purpose: the lock
/// must never be held across an `.await`.
fn update_subs(subs: &Mutex<Vec<String>>, f: impl FnOnce(&mut Vec<String>)) -> usize {
    let mut s = subs.lock().unwrap_or_else(|e| e.into_inner());
    f(&mut s);
    s.len()
}

/// The only task that touches the socket's write half, so frames never interleave.
async fn writer(
    mut wr: OwnedWriteHalf,
    mut out: mpsc::Receiver<ServerFrame>,
    mut events: broadcast::Receiver<Event>,
    subs: Arc<Mutex<Vec<String>>>,
    shutdown: CancellationToken,
) {
    loop {
        let frame = tokio::select! {
            biased;
            f = out.recv() => match f {
                Some(f) => f,
                None => break,
            },
            ev = events.recv() => match ev {
                Ok(e) => {
                    let wanted = subs.lock().unwrap_or_else(|p| p.into_inner()).iter().any(|p| topic_matches(p, &e.topic));
                    if !wanted { continue }
                    ServerFrame::event(e)
                }
                Err(broadcast::error::RecvError::Lagged(n)) => ServerFrame::event(Event::new(
                    ModuleId::new("core"),
                    topics::STREAM_LAGGED,
                    json!({"dropped": n}),
                    chrono::Utc::now(),
                )),
                Err(broadcast::error::RecvError::Closed) => continue,
            },
            _ = shutdown.cancelled() => {
                while let Ok(f) = out.try_recv() {
                    if write_frame(&mut wr, &f).await.is_err() { return }
                }
                break;
            }
        };
        if write_frame(&mut wr, &frame).await.is_err() {
            return;
        }
    }
    let _ = wr.shutdown().await;
}
