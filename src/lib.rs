//! `bo` — an audio editor and mixer for agents, as a library.
//!
//! The data model and engine live in [`bo_core`] (re-exported here, so
//! `bo::track` still reads as before). This crate adds the two surfaces on
//! top of that spine:
//!
//! * [`session`] — a session: today the daemon on its Unix socket, spawned
//!   on demand, speaking a typed JSON wire ([`session::Session`]).
//! * [`client`] — the typed client on a session: [`client::Bo`] puts clips
//!   and tunes a session like the CLI does, minus the reply grammar.
//!
//! The binary crate in this package (`bo` on the command line) is the text
//! front-end and, for now, the daemon's home.

pub use bo_core::{bus, control, engine, time, track};

pub mod client;
pub mod session;
