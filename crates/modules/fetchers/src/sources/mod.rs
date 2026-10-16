//! One file per `Source`. `hn` and `wwr` are the fixed, compiled-in sources (ADR 0028 §10),
//! registered in [`crate::source::registry`]. `rss` and `json_api` are *kinds* (ADR 0028
//! §2a/§15) -- never in `registry`, built on demand per configured instance by
//! [`crate::source::resolve`].

pub mod hn;
pub mod json_api;
pub mod rss;
pub mod wwr;
