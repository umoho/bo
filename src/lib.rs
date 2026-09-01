//! `bo` — a player whose arrangement is data.
//!
//! Two modules so far:
//!
//! * [`track`] — [`track::Source`] → [`track::Clip`] → [`track::Track`]: slices
//!   of audio sources laid on non-overlapping timelines.
//! * [`engine`] — [`engine::Player`]: transport state over stacked tracks, with a
//!   [`engine::Backend`] seam for whatever actually makes sound.

pub mod engine;
pub mod track;
