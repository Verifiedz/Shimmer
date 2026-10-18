//! `shimmer-proto`: wire types for `docs/protocol.md`. Change-controlled (CLAUDE.md §4).
//!
//! Pure data and pure functions: no sockets, no runtime. Clients and the daemon each own
//! their own I/O and share only these types.

pub mod frame;
pub mod paths;
pub mod topic;

pub use frame::{
    decode_client, decode_server, encode, ClientFrame, EventsListData, ManifestData, ModuleInfo, PingData,
    QueueControl, QueuePriority, QueuedHandle, ServerFrame,
};
pub use topic::{is_valid_pattern, topic_matches};

/// Bumped only on breaking changes. The daemon refuses any other value.
pub const PROTOCOL_VERSION: u32 = 2;

/// A longer line is a protocol error and the daemon closes the connection.
pub const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

/// Bounded per-connection event queue before the daemon drops the oldest.
pub const EVENT_QUEUE_CAPACITY: usize = 1024;

/// Canonical op names for the always-present core and queue ops.
pub mod ops {
    pub const CORE_PING: &str = "core.ping";
    pub const CORE_MANIFEST: &str = "core.manifest";
    pub const CORE_SHUTDOWN: &str = "core.shutdown";
    pub const QUEUE_LIST: &str = "queue.list";
    pub const QUEUE_SHOW: &str = "queue.show";
    pub const QUEUE_CANCEL: &str = "queue.cancel";
    pub const QUEUE_MOVE: &str = "queue.move";
    pub const EVENTS_LIST: &str = "core.events.list";
}

/// Topics the daemon itself defines.
pub mod topics {
    pub const STREAM_LAGGED: &str = "core.stream.lagged";
}
