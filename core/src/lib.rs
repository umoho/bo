//! `bo-core` — bo's data model.
//!
//! The structures both the executors ([`bo_engine`]) and the client are
//! built on. Pure data, no execution: nothing here makes sound, reads a
//! file, or runs a transport.
//!
//! * [`track`] — the content layer: [`track::Source`] → [`track::Clip`] →
//!   [`track::Track`], slices of audio sources laid on non-overlapping
//!   timelines. Content answers *what* plays and *when*.
//! * [`control`] — the control sources that modulate a clip's parameters as
//!   they play: the curve, the LFO and the sidechain.
//! * [`bus`] — the signal layer: [`bus::Output`] and the bus nodes it feeds
//!   — the master ([`bus::Bus`]) and the group buses ([`bus::Group`]).
//! * [`time`] — the timecode text every surface speaks.

pub mod bus;
pub mod command;
pub mod time;
pub mod control;
pub mod track;
