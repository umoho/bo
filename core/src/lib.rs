//! `bo-core` — the spine both the daemon and the client are built on.
//!
//! The data model and the engine that makes sound from it: the layers the
//! daemon (the arrangement and its transport) and the client (a typed
//! command surface) both speak. Split out of `bo` so the daemon can live
//! beside the library, each depending on the same core:
//!
//! * [`track`] — the content layer: [`track::Source`] → [`track::Clip`] →
//!   [`track::Track`], slices of audio sources laid on non-overlapping
//!   timelines. Content answers *what* plays and *when*; it holds no signal
//!   state.
//! * [`control`] — the control sources that modulate a clip's parameters as
//!   they play: the curve, the LFO and the sidechain, each one serializable
//!   JSON object.
//! * [`bus`] — the signal layer: [`bus::Output`] (a track's edge into the
//!   mix, carrying gain, mute and placement) and the bus nodes it feeds —
//!   the master ([`bus::Bus`]) and the group buses ([`bus::Group`]).
//! * [`engine`] — [`engine::Player`]: transport state over stacked tracks,
//!   with an [`engine::Backend`] seam for whatever actually makes sound,
//!   and the rodio implementation ([`engine::rodio`]).
//! * [`time`] — the timecode text every surface speaks.

pub mod bus;
pub mod time;
pub mod control;
pub mod engine;
pub mod track;
