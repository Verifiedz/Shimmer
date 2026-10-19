use serde::{Deserialize, Serialize};
use serde_json::Value;
use shimmer_core::{CommandSpec, Error, Event, LaneConfig};

/// Client → daemon.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ClientFrame {
    Hello {
        v: u32,
        client: String,
        client_version: String,
    },
    Request {
        v: u32,
        /// Opaque, unique within the connection, echoed on the response.
        id: String,
        op: String,
        #[serde(default)]
        params: Value,
        /// Queue behaviour is orthogonal to the op, so it rides on the frame.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        queue: Option<QueueControl>,
    },
    Subscribe {
        v: u32,
        id: String,
        topics: Vec<String>,
    },
    Unsubscribe {
        v: u32,
        id: String,
        topics: Vec<String>,
    },
}

/// Daemon → client.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ServerFrame {
    Welcome {
        v: u32,
        daemon_version: String,
        session: String,
        /// The Shimmer folder this daemon serves, resolved (ADR 0039 §2). Absent from an older
        /// daemon and from `shimmer mockd`, which serves fixtures, not a folder.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        home: Option<String>,
    },
    Response {
        v: u32,
        id: String,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<Error>,
    },
    /// Pushed at any time after `subscribe`. Carries no id.
    Event { v: u32, event: Event },
    /// A failure with no request to attach to: a rejected handshake or an unparseable frame.
    Error { v: u32, error: Error },
}

impl ServerFrame {
    /// A `welcome` from something that serves no folder (`shimmer mockd`, test servers).
    pub fn welcome(daemon_version: impl Into<String>, session: impl Into<String>) -> Self {
        Self::Welcome {
            v: crate::PROTOCOL_VERSION,
            daemon_version: daemon_version.into(),
            session: session.into(),
            home: None,
        }
    }

    /// The real daemon's `welcome`, naming the folder it serves (ADR 0039 §2).
    pub fn welcome_for(daemon_version: impl Into<String>, session: impl Into<String>, home: &std::path::Path) -> Self {
        Self::Welcome {
            v: crate::PROTOCOL_VERSION,
            daemon_version: daemon_version.into(),
            session: session.into(),
            home: Some(crate::paths::resolve(home).display().to_string()),
        }
    }

    pub fn ok(id: impl Into<String>, data: Value) -> Self {
        Self::Response { v: crate::PROTOCOL_VERSION, id: id.into(), ok: true, data: Some(data), error: None }
    }

    pub fn err(id: impl Into<String>, error: Error) -> Self {
        Self::Response { v: crate::PROTOCOL_VERSION, id: id.into(), ok: false, data: None, error: Some(error) }
    }

    pub fn event(event: Event) -> Self {
        Self::Event { v: crate::PROTOCOL_VERSION, event }
    }

    pub fn error(error: Error) -> Self {
        Self::Error { v: crate::PROTOCOL_VERSION, error }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueuePriority {
    #[default]
    Normal,
    /// Promote ahead of scheduled work. Requires confirmation. `scheduled` is deliberately
    /// absent: only the scheduler produces it.
    Override,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueControl {
    #[serde(default)]
    pub priority: QueuePriority,
    #[serde(default)]
    pub confirm: bool,
    /// Echoed from a prior `confirmation_required`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_version: Option<u64>,
}

/// `data` of a response to a queued op.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedHandle {
    pub queued: bool,
    pub task_id: String,
    pub lane: String,
    pub position: usize,
}

/// `data` of `core.ping`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PingData {
    pub pong: bool,
    pub uptime_s: u64,
}

/// `data` of `core.events.list`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EventsListData {
    /// Oldest first, same convention as `events_matching`/`events_matching_recent`
    /// (`crates/store/src/index.rs`).
    pub events: Vec<Event>,
}

/// `data` of `core.manifest`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ManifestData {
    pub protocol: u32,
    pub lanes: Vec<LaneConfig>,
    pub modules: Vec<ModuleInfo>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModuleInfo {
    pub id: String,
    pub version: String,
    pub namespace: String,
    pub commands: Vec<CommandSpec>,
    pub topics: Vec<String>,
}

/// One frame as a line: compact JSON plus `\n`. Never contains another newline.
pub fn encode<T: Serialize>(frame: &T) -> Result<String, Error> {
    let mut s = serde_json::to_string(frame).map_err(|e| Error::internal(format!("encode: {e}")))?;
    s.push('\n');
    Ok(s)
}

pub fn decode_client(line: &str) -> Result<ClientFrame, Error> {
    serde_json::from_str(line.trim_end()).map_err(|e| Error::bad_request(format!("malformed frame: {e}")))
}

pub fn decode_server(line: &str) -> Result<ServerFrame, Error> {
    serde_json::from_str(line.trim_end()).map_err(|e| Error::bad_request(format!("malformed frame: {e}")))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// ADR 0039 §2: the real daemon names its folder; a welcome without one still decodes.
    #[test]
    fn welcome_names_its_folder_and_a_welcome_without_one_still_decodes() {
        let with =
            serde_json::to_value(ServerFrame::welcome_for("0.1.0", "s1", std::path::Path::new("/x/y/../shimmer")))
                .unwrap();
        assert_eq!(with["home"], "/x/shimmer");
        let without = serde_json::to_value(ServerFrame::welcome("0.1.0", "s1")).unwrap();
        assert!(without.get("home").is_none(), "absent, not null: {without}");
        let old = r#"{"v":2,"kind":"welcome","daemon_version":"0.1.0","session":"s1"}"#;
        assert!(matches!(decode_server(old).unwrap(), ServerFrame::Welcome { home: None, .. }));
    }

    #[test]
    fn protocol_md_examples_parse() {
        let hello = r#"{"v": 2, "kind": "hello", "client": "tui", "client_version": "0.3.1"}"#;
        assert!(matches!(decode_client(hello).unwrap(), ClientFrame::Hello { v: 2, .. }));

        let req = r#"{"v":2,"kind":"request","id":"p2","op":"fetchers.run",
            "params":{"source":"linkedin"},
            "queue":{"priority":"override","confirm":true,"queue_version":41}}"#
            .replace('\n', "");
        let ClientFrame::Request { queue, .. } = decode_client(&req).unwrap() else { panic!("not a request") };
        assert_eq!(
            queue.unwrap(),
            QueueControl { priority: QueuePriority::Override, confirm: true, queue_version: Some(41) }
        );

        let sub = r#"{"v":2,"kind":"subscribe","id":"s1","topics":["records.*","queue.task.failed"]}"#;
        assert!(matches!(decode_client(sub).unwrap(), ClientFrame::Subscribe { .. }));
    }

    #[test]
    fn request_defaults_and_scheduled_is_not_settable() {
        let req = r#"{"v":2,"kind":"request","id":"1","op":"core.ping"}"#;
        let ClientFrame::Request { params, queue, .. } = decode_client(req).unwrap() else { panic!() };
        assert_eq!((params, queue), (Value::Null, None));

        let sched = r#"{"v":2,"kind":"request","id":"1","op":"x.y","queue":{"priority":"scheduled"}}"#;
        assert!(decode_client(sched).is_err());
    }

    #[test]
    fn responses_match_documented_shape() {
        let ok = serde_json::to_value(ServerFrame::ok("r1", json!({"pong": true}))).unwrap();
        assert_eq!(ok, json!({"v":2,"kind":"response","id":"r1","ok":true,"data":{"pong":true}}));
        let err = serde_json::to_value(ServerFrame::err("r1", Error::not_found("no collection 'x'"))).unwrap();
        assert_eq!(
            err,
            json!({"v":2,"kind":"response","id":"r1","ok":false,
                "error":{"code":"not_found","message":"no collection 'x'","detail":null}})
        );
    }

    #[test]
    fn encoded_frame_is_one_line() {
        let f = ServerFrame::ok("1", json!({"text": "a\nb"}));
        let line = encode(&f).unwrap();
        assert_eq!(line.matches('\n').count(), 1);
        assert!(line.ends_with('\n'));
        assert_eq!(decode_server(&line).unwrap(), f);
    }
}
