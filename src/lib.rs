//! `bo` — an audio editor and mixer for agents, as a client.
//!
//! The data model lives in [`bo_core`] and the engine in [`bo_engine`],
//! both re-exported here so `bo::track`, `bo::engine`, … still read as
//! before. This crate adds the client surfaces on top:
//!
//! * [`session`] — a session: today the daemon on its Unix socket, spawned
//!   on demand, speaking a typed JSON wire ([`session::Session`]).
//! * [`client`] — the typed client on a session: [`client::Bo`] puts clips
//!   and tunes a session like the CLI does, minus the reply grammar.
//!
//! Operations are forwarded to an executor: the daemon over its socket
//! ([`Session`](session::Session)), or — for a process session — the engine
//! directly. The binary crate in this package (`bo` on the command line) is
//! the text front-end and, for now, the daemon's home.

pub use bo_core::{bus, control, time, track};
pub use bo_engine as engine;

pub mod client;
pub mod session;
