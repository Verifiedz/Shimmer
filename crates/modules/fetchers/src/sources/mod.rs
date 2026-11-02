//! One file per `Source`. All four are *kinds* (ADR 0028 §2a/§3/§10/§15), registered in
//! [`crate::kind::kinds`] and built on demand per configured instance by
//! [`crate::source::resolve`] -- there is no fixed, compiled-in registry any more.

pub mod hn;
pub mod json_api;
pub mod rss;
pub mod wwr;
