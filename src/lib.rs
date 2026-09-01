//! `bo` — an agentic CLI player for audio *and* speech.
//!
//! It plays collections of files/streams and generated speech (TTS) through the
//! same playlist, because to an agent orchestrating them they are the same
//! thing: rows that become sound, one at a time, in some order.
//!
//! The crate is split so that the parts an agent drives are pure data, and the
//! parts that touch the outside world are thin shells:
//!
//! | layer | responsibility | status |
//! |---|---|---|
//! | [`playlist`] | entries, play order, cursor, mode, history | done |
//! | engine | decode or synthesize + output (play/pause/seek/volume/rate) | next |
//! | ui | terminal rendering, key handling, REPL | later |
//! | agent | tool surface over the layers above | later |
//!
//! Everything the agent can do — queue, reorder, search, jump, toggle shuffle —
//! is a method on [`playlist::Playlist`] that returns data, so a turn can be
//! planned, echoed back and asserted on without any sound or TTY in the loop.

#![warn(missing_docs, missing_debug_implementations, rust_2018_idioms)]

pub mod playlist;
