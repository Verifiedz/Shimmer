//! One file per `Source`. `hn` and `wwr` are the fixed, compiled-in sources (ADR 0028 §10),
//! registered in [`crate::source::registry`]. `rss` is a *kind* (ADR 0028 §2a) -- never in
//! `registry`, built on demand per configured instance by [`crate::source::resolve`].

pub mod hn;
pub mod jobbank;
pub mod rss;
pub mod wwr;
