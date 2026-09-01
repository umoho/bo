//! [`Player`]: transport state over a set of stacked [`Track`]s, plus the
//! [`Backend`] seam through which sound actually comes out.
//!
//! The player owns three things and nothing else: the tracks, a playhead
//! timecode, and a state (`Stopped`/`Playing`/`Paused`). Which clips are audible
//! at a given timecode is *derived* (`active_clips`) rather than stored, so the
//! mix can never drift out of sync with the arrangement.

use std::fmt;
use std::time::Duration;

pub mod rodio;
pub mod timeline;

use crate::track::{Clip, Track};

/// Why a backend could not do what it was told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendError {
    /// Which backend said no.
    pub backend: String,
    /// What it said.
    pub message: String,
}

impl BackendError {
    /// A failure from a named backend.
    pub fn new(backend: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            backend: backend.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.backend, self.message)
    }
}

impl std::error::Error for BackendError {}

/// The thing that makes sound.
///
/// `Player` never decodes anything itself; it decides *what* should be audible
/// and hands that over. Implementations: [`Silent`] here (headless, used by
/// tests and CI), and whatever real audio backend the daemon picks at startup.
pub trait Backend {
    /// Start (or restart) playback of `tracks` from playhead `at`.
    ///
    /// `at` is a track timecode; an implementation reads each clip from
    /// `clip.from + (at - clip.at)` and skips clips that have already passed.
    fn play(&mut self, tracks: &[Track], at: Duration) -> Result<(), BackendError>;

    /// Suspend output, keeping position.
    fn pause(&mut self);

    /// Resume after [`Backend::pause`], without re-planning.
    fn resume(&mut self);

    /// Silence everything and forget position.
    fn stop(&mut self);

    /// Master gain, already clamped to `0.0 ..= 1.0`.
    fn set_volume(&mut self, volume: f32);

    /// Where the audio clock really is, if this backend can say. Used to
    /// correct the playhead against drift.
    fn position(&self) -> Option<Duration> {
        None
    }
}

/// A backend that hears nothing: for tests, CI, and `--backend silent`.
#[derive(Debug, Default, Clone)]
pub struct Silent {
    /// Last `play` request, as `(track count, start timecode)`.
    pub last_play: Option<(usize, Duration)>,
    /// Requests so far, in order. Lets a test assert "seek re-planned".
    pub events: Vec<BackendEvent>,
    volume: f32,
}

/// One thing a [`Silent`] backend was asked to do.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BackendEvent {
    Play,
    Pause,
    Resume,
    Stop,
    SetVolume,
}

impl Backend for Silent {
    fn play(&mut self, tracks: &[Track], at: Duration) -> Result<(), BackendError> {
        self.last_play = Some((tracks.len(), at));
        self.events.push(BackendEvent::Play);
        Ok(())
    }

    fn pause(&mut self) {
        self.events.push(BackendEvent::Pause);
    }

    fn resume(&mut self) {
        self.events.push(BackendEvent::Resume);
    }

    fn stop(&mut self) {
        self.events.push(BackendEvent::Stop);
    }

    fn set_volume(&mut self, volume: f32) {
        self.volume = volume;
        self.events.push(BackendEvent::SetVolume);
    }
}

impl Silent {
    /// Volume last requested, for assertions.
    #[must_use]
    pub fn volume(&self) -> f32 {
        self.volume
    }
}

/// Transport state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum State {
    /// Nothing loaded into the backend; playhead is at zero.
    #[default]
    Stopped,
    Playing,
    Paused,
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Stopped => "stopped",
            Self::Playing => "playing",
            Self::Paused => "paused",
        })
    }
}

/// A playhead over a set of simultaneously mixed tracks.
#[derive(Debug)]
pub struct Player<B: Backend = Silent> {
    tracks: Vec<Track>,
    playhead: Duration,
    state: State,
    volume: f32,
    backend: B,
}

impl Default for Player<Silent> {
    fn default() -> Self {
        Self::new(Silent::default())
    }
}

impl<B: Backend> Player<B> {
    /// A stopped player over an empty set of tracks.
    pub fn new(backend: B) -> Self {
        Self {
            tracks: Vec::new(),
            playhead: Duration::ZERO,
            state: State::Stopped,
            volume: 1.0,
            backend,
        }
    }

    /// The arrangement being played.
    #[must_use]
    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    /// Edit the arrangement. Whether a live edit takes effect now or at the next
    /// `play` is up to the caller: the player does not re-plan behind your back.
    pub fn tracks_mut(&mut self) -> &mut Vec<Track> {
        &mut self.tracks
    }

    /// Add a track on top of the existing ones.
    pub fn add_track(&mut self, track: Track) -> usize {
        self.tracks.push(track);
        self.tracks.len() - 1
    }

    /// Remove a track by index.
    pub fn remove_track(&mut self, index: usize) -> Option<Track> {
        (index < self.tracks.len()).then(|| self.tracks.remove(index))
    }

    /// Current transport state.
    #[must_use]
    pub fn state(&self) -> State {
        self.state
    }

    /// Whether the transport is running.
    #[must_use]
    pub fn is_playing(&self) -> bool {
        self.state == State::Playing
    }

    /// The playhead timecode. While running, a backend that knows its own clock
    /// is believed over our own bookkeeping.
    #[must_use]
    pub fn playhead(&self) -> Duration {
        if self.state == State::Playing
            && let Some(real) = self.backend.position()
        {
            return real;
        }
        self.playhead
    }

    /// Master gain, clamped to `0.0 ..= 1.0`.
    #[must_use]
    pub fn volume(&self) -> f32 {
        self.volume
    }

    /// Set master gain.
    pub fn set_volume(&mut self, volume: f32) {
        self.volume = volume.clamp(0.0, 1.0);
        self.backend.set_volume(self.volume);
    }

    /// The whole arrangement's length: the latest end across tracks. `None` when
    /// some clip has no knowable end.
    #[must_use]
    pub fn duration(&self) -> Option<Duration> {
        self.tracks.iter().try_fold(Duration::ZERO, |acc, track| {
            Some(acc.max(track.duration()?))
        })
    }

    /// Every clip audible at track time `t`, one per track at most. This *is*
    /// the mix; nothing about it is cached.
    pub fn active_clips(&self, t: Duration) -> impl Iterator<Item = (usize, &Clip)> {
        self.tracks
            .iter()
            .enumerate()
            .filter_map(move |(index, track)| track.clip_at(t).map(|clip| (index, clip)))
    }

    /// Borrow the backend, e.g. to query it directly.
    #[must_use]
    pub fn backend(&self) -> &B {
        &self.backend
    }

    /// Mutable access to the backend.
    pub fn backend_mut(&mut self) -> &mut B {
        &mut self.backend
    }

    /// Replace the backend, e.g. swap in a real audio implementation. Leaves
    /// transport stopped.
    pub fn with_backend<C: Backend>(self, backend: C) -> Player<C> {
        let Player {
            tracks,
            playhead,
            volume,
            ..
        } = self;
        Player {
            tracks,
            playhead,
            state: State::Stopped,
            volume,
            backend,
        }
    }
}

impl<B: Backend> Player<B> {
    /// Start playing from the current playhead.
    pub fn play(&mut self) -> Result<(), BackendError> {
        self.backend.play(&self.tracks, self.playhead)?;
        self.state = State::Playing;
        Ok(())
    }

    /// Hold position and silence output.
    pub fn pause(&mut self) {
        if self.state == State::Playing {
            self.backend.pause();
            self.state = State::Paused;
        }
    }

    /// Continue from where `pause` left off — no re-planning.
    pub fn resume(&mut self) -> Result<(), BackendError> {
        if self.state == State::Paused {
            self.backend.resume();
            self.state = State::Playing;
        } else if self.state == State::Stopped {
            return self.play();
        }
        Ok(())
    }

    /// Stop and rewind to zero.
    pub fn stop(&mut self) {
        self.backend.stop();
        self.playhead = Duration::ZERO;
        self.state = State::Stopped;
    }

    /// Drop every track and reset the transport: stop playback, rewind the
    /// playhead, and clear the arrangement.
    pub fn reset(&mut self) {
        self.stop();
        self.tracks.clear();
    }

    /// Jump the playhead. A running transport is re-planned from the new
    /// position, because most backends cannot seek mid-stream.
    pub fn seek(&mut self, at: Duration) -> Result<(), BackendError> {
        self.playhead = at;
        if self.state == State::Playing {
            self.backend.play(&self.tracks, at)?;
        }
        Ok(())
    }

    /// Apply the arrangement to a running transport: rebuild the backend's
    /// playback graph from the current playhead, so pending mix changes
    /// (volume, mute) take effect now. No-op unless playing; the playhead is
    /// untouched either way.
    pub fn apply(&mut self) -> Result<(), BackendError> {
        if self.state == State::Playing {
            self.backend.play(&self.tracks, self.playhead)?;
        }
        Ok(())
    }

    /// Move the playhead to match a backend-reported clock, without re-planning.
    pub fn set_playhead(&mut self, at: Duration) {
        self.playhead = at;
    }

    /// Step the playhead forward by wall-clock elapsed time. Backends that
    /// cannot report a position use this.
    pub fn advance(&mut self, dt: Duration) {
        if self.state == State::Playing {
            self.playhead += dt;
        }
    }

    /// Whether playback of a finite arrangement has run to its end.
    ///
    /// The playhead is compared against the arrangement's known length; an
    /// arrangement containing an open-ended clip has no knowable length and
    /// never finishes. Long-running callers use this to decide when a program
    /// is done.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        match self.duration() {
            Some(length) => self.playhead() >= length,
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::track::Clip;
    use std::sync::Arc;

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    fn track_with(uri: &str, len: u64) -> Track {
        let mut t = Track::named(uri);
        t.insert(Clip::new(Arc::new(crate::track::Source {
            uri: uri.into(),
            duration: Some(secs(len)),
        })))
        .unwrap();
        t
    }

    #[test]
    fn transport_state_machine() {
        let mut p: Player<Silent> = Player::default();
        p.add_track(track_with("a.wav", 10));
        assert_eq!(p.state(), State::Stopped);
        assert_eq!(p.playhead(), Duration::ZERO);

        p.play().unwrap();
        assert_eq!(p.state(), State::Playing);
        assert_eq!(p.backend().last_play, Some((1, Duration::ZERO)));

        p.pause();
        assert_eq!(p.state(), State::Paused);
        p.resume().unwrap();
        assert_eq!(p.state(), State::Playing);
        assert_eq!(
            p.backend().events,
            vec![
                BackendEvent::Play,
                BackendEvent::Pause,
                BackendEvent::Resume
            ]
        );

        p.stop();
        assert_eq!(p.state(), State::Stopped);
        assert_eq!(p.playhead(), Duration::ZERO);
        // pause/advance on a stopped transport are no-ops.
        p.pause();
        p.advance(secs(5));
        assert_eq!(p.playhead(), Duration::ZERO);
        assert_eq!(p.state(), State::Stopped);
    }

    #[test]
    fn seeking_replans_a_running_transport_only() {
        let mut p: Player<Silent> = Player::default();
        p.add_track(track_with("a.wav", 10));
        p.seek(secs(4)).unwrap();
        assert_eq!(p.playhead(), secs(4));
        assert_eq!(p.backend().last_play, None, "stopped: nothing to plan");

        p.play().unwrap();
        assert_eq!(
            p.backend().last_play,
            Some((1, secs(4))),
            "starts where we parked"
        );
        p.seek(secs(7)).unwrap();
        assert_eq!(p.backend().last_play, Some((1, secs(7))));
        assert_eq!(
            p.backend()
                .events
                .iter()
                .filter(|e| **e == BackendEvent::Play)
                .count(),
            2
        );
    }

    #[test]
    fn apply_rebuilds_a_running_transport_only() {
        let mut p: Player<Silent> = Player::default();
        p.add_track(track_with("a.wav", 10));
        let plays = |p: &Player<Silent>| {
            p.backend()
                .events
                .iter()
                .filter(|e| **e == BackendEvent::Play)
                .count()
        };

        p.apply().unwrap();
        assert_eq!(plays(&p), 0, "stopped: nothing to plan");
        assert_eq!(p.backend().last_play, None);

        p.play().unwrap();
        p.seek(Duration::from_secs(4)).unwrap();
        assert_eq!(plays(&p), 2);
        p.apply().unwrap();
        assert_eq!(plays(&p), 3, "apply rebuilds from the current playhead");
        assert_eq!(p.backend().last_play, Some((1, Duration::from_secs(4))));
        assert_eq!(
            p.playhead(),
            Duration::from_secs(4),
            "apply does not move the playhead"
        );

        // Paused: nothing to rebuild.
        p.pause();
        p.apply().unwrap();
        assert_eq!(plays(&p), 3);
    }

    #[test]
    fn stacked_tracks_mix_at_the_same_timecode() {
        let mut p: Player<Silent> = Player::default();
        let mut bed = Track::named("bed");
        bed.insert(Clip::new(Arc::new(crate::track::Source {
            uri: "bed.wav".into(),
            duration: Some(secs(30)),
        })))
        .unwrap();
        p.add_track(bed);
        let mut ding = Track::named("ding");
        ding.insert(
            Clip::new(Arc::new(crate::track::Source {
                uri: "ding.wav".into(),
                duration: Some(secs(2)),
            }))
            .at(secs(5)),
        )
        .unwrap();
        p.add_track(ding);

        assert_eq!(p.duration(), Some(secs(30)), "longest track wins");
        let at_zero: Vec<_> = p.active_clips(Duration::ZERO).map(|(i, _)| i).collect();
        assert_eq!(at_zero, vec![0]);
        let at_five: Vec<_> = p.active_clips(secs(5)).map(|(i, _)| i).collect();
        assert_eq!(
            at_five,
            vec![0, 1],
            "two sources at one timecode, on separate tracks"
        );
        assert_eq!(
            p.active_clips(secs(8)).map(|(i, _)| i).collect::<Vec<_>>(),
            vec![0],
            "the ding has ended"
        );
        assert!(p.active_clips(secs(31)).next().is_none());
    }

    #[test]
    fn a_finite_arrangement_finishes_when_the_playhead_passes_its_end() {
        let mut p: Player<Silent> = Player::default();
        p.add_track(track_with("a.wav", 10));
        p.play().unwrap();
        assert!(!p.is_finished());
        p.advance(secs(9));
        assert!(!p.is_finished(), "still inside the arrangement");
        p.advance(secs(2));
        assert!(p.is_finished(), "the playhead passed the end");

        // An open-ended clip makes the length unknowable: never finishes.
        let mut live: Player<Silent> = Player::default();
        let mut t = Track::named("live");
        t.insert(Clip::new(Arc::new(crate::track::Source {
            uri: "live.wav".into(),
            duration: None,
        })))
        .unwrap();
        live.add_track(t);
        live.play().unwrap();
        live.advance(secs(9999));
        assert!(!live.is_finished());
    }

    #[test]
    fn volume_is_clamped_and_forwarded() {
        let mut p: Player<Silent> = Player::default();
        p.set_volume(2.5);
        assert_eq!(p.volume(), 1.0);
        assert_eq!(p.backend().volume(), 1.0);
        p.set_volume(-1.0);
        assert_eq!(p.volume(), 0.0);
    }

    #[test]
    fn swapping_the_backend_resets_transport() {
        let mut p: Player<Silent> = Player::default();
        p.add_track(track_with("a.wav", 10));
        p.play().unwrap();
        p.seek(secs(3)).unwrap();
        let mut swapped = p.with_backend(Silent::default());
        assert_eq!(swapped.state(), State::Stopped);
        assert_eq!(swapped.tracks().len(), 1, "the arrangement came across");
        assert_eq!(swapped.playhead(), secs(3), "so did the position");
        assert!(
            swapped.backend().events.is_empty(),
            "the new one saw nothing"
        );
        assert_eq!(swapped.remove_track(0).map(|t| t.clips().len()), Some(1));
        assert_eq!(swapped.tracks().len(), 0);
    }

    #[test]
    fn reset_clears_tracks_and_transport() {
        let mut p: Player<Silent> = Player::default();
        p.add_track(track_with("a.wav", 10));
        p.play().unwrap();
        p.seek(Duration::from_secs(3)).unwrap();
        p.reset();
        assert_eq!(p.tracks().len(), 0);
        assert_eq!(p.state(), State::Stopped);
        assert_eq!(p.playhead(), Duration::ZERO);
        assert!(p.backend().events.iter().any(|e| *e == BackendEvent::Stop));
    }
}
