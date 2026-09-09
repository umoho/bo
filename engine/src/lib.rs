//! `bo-engine` — the actual execution on top of bo's data model.
//!
//! The model in `bo-core` says *what* plays; this crate makes sound from
//! it: the transport over stacked tracks, the backend seam (headless
//! silence, real audio through rodio), and the offline mix — render,
//! probing and measuring. Executors — the daemon, or an in-process
//! session — are thin wrappers around this crate.

pub mod engine;
