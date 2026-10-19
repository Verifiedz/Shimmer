//! Topic pattern matching for subscriptions: lives in `core` (ADR 0035 §5.2), re-exported here so
//! clients and the daemon are unchanged.

pub use shimmer_core::topic::{is_valid_pattern, topic_matches};
