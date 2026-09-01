//! `bo` — an agentic CLI music player.
//!
//! The crate is split so that the parts an agent drives are pure data, and the
//! parts that touch the outside world are thin shells:
//!
//! | layer | responsibility | status |
//! |---|---|---|
//! | [`playlist`] | entries, play order, cursor, mode, history | done |
//! | engine | decode + output (play/pause/seek/volume) | next |
//! | ui | terminal rendering, key handling, REPL | later |
//! | agent | tool surface over the layers above | later |
//!
//! Everything the agent can do — queue, reorder, search, jump, toggle shuffle —
//! is a method on [`playlist::Playlist`] that returns data, so a turn can be
//! planned, echoed back and asserted on without any audio or TTY in the loop.

#![warn(missing_docs, missing_debug_implementations, rust_2018_idioms)]

pub mod playlist;
