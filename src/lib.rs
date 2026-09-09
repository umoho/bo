//! `bo` — an audio editor and mixer for agents.
//!
//! One command per action, from a shell or driven by an agent: build a
//! session out of stacked tracks, each holding clips that slice an audio
//! source; tune the mix; then audition it live or render it offline. Not a
//! DAW yet — but this data model is the spine a CLI DAW would grow from.
//!
//! Three modules, split along the one seam the model keeps clean:
//!
//! * [`track`] — the content layer: [`track::Source`] → [`track::Clip`] →
//!   [`track::Track`], slices of audio sources laid on non-overlapping
//!   timelines. Content answers *what* plays and *when*; it holds no signal
//!   state.
//! * [`control`] — the control sources that modulate a clip's parameters as
//!   they play: the curve, the LFO and the sidechain, each one serializable
//!   JSON object (its text form, the registry [`control::ControlSource`]).
//! * [`bus`] — the signal layer: [`bus::Output`] (a track's edge into the
//!   mix, carrying gain, mute and placement) and the bus nodes it feeds —
//!   the master ([`bus::Bus`]) and the group buses ([`bus::Group`]) several
//!   tracks can be routed into so one strip ducks them together. Signal
//!   answers *where the sound goes*; it holds no content.
//! * [`engine`] — [`engine::Player`]: transport state over stacked tracks,
//!   with an [`engine::Backend`] seam for whatever actually makes sound.
//! * [`client`] — the command surface as a library: a [`client::Bo`] you
//!   drive with typed calls, the CLI's verbs without the daemon.
//! * [`time`] — the timecode text both layers speak: a lenient
//!   `SS`/`MM:SS`/`HH:MM:SS` parse and the canonical `HH:MM:SS.fff` form.

pub mod bus;
pub mod client;
pub mod time;
pub mod control;
pub mod engine;
pub mod track;
