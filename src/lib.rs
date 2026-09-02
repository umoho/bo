//! `bo` — an audio editor and mixer for agents.
//!
//! One command per action, from a shell or driven by an agent: build a
//! session out of stacked tracks, each holding clips that slice an audio
//! source; tune the mix; then audition it live or render it offline. Not a
//! DAW yet — but this data model (`Source` → `Clip` → `Track` over a
//! timeline) is the spine a CLI DAW would grow from.
//!
//! Two modules so far:
//!
//! * [`track`] — [`track::Source`] → [`track::Clip`] → [`track::Track`]: slices
//!   of audio sources laid on non-overlapping timelines.
//! * [`engine`] — [`engine::Player`]: transport state over stacked tracks, with a
//!   [`engine::Backend`] seam for whatever actually makes sound.

pub mod engine;
pub mod track;
