//! The three data structures that describe sound: [`Source`] (an addressable
//! audio resource), [`Clip`] (a slice of a source, placed at a timecode on a
//! track) and [`Track`] (a timeline of non-overlapping clips).
//!
//! Timecodes are `std::time::Duration` measured from their own origin: a clip's
//! `from`/`to` count from the start of the source, a clip's `at` and a player's
//! playhead count from the start of the track.
//!
//! Kept deliberately small: no transport, no decoding, no metadata — just the
//! shape those layers will sit on.

use std::sync::Arc;
use std::time::Duration;

/// An audio resource: something that can be decoded and played.
///
/// Shared by reference (`Arc`), because cutting one file into three clips must
/// not duplicate it — the decode cache and any future per-source state live at
/// this layer, and `Arc::ptr_eq` tells you whether two clips really cite the
/// same source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    /// Address of the resource: local path, URL, ...
    pub uri: String,
    /// Known length. `None` means either "not probed yet" or "has no end"
    /// (live stream); deciding which is the backend's business, not the model's.
    pub duration: Option<Duration>,
}

impl Source {
    /// A source of unknown length.
    pub fn new(uri: impl Into<String>) -> Self {
        Self {
            uri: uri.into(),
            duration: None,
        }
    }

    /// A source with a known length, e.g. after a probe.
    #[must_use]
    pub fn with_duration(mut self, duration: impl Into<Option<Duration>>) -> Self {
        self.duration = duration.into();
        self
    }

    /// A shareable reference, the form [`Clip`] wants.
    pub fn shared(uri: impl Into<String>) -> Arc<Self> {
        Arc::new(Self::new(uri))
    }

    /// Whether no end is known for this source.
    #[must_use]
    pub fn is_open_ended(&self) -> bool {
        self.duration.is_none()
    }
}

/// A slice of a source, occupying `at .. at + duration()` on one [`Track`].
///
/// Two pairs of timecodes, deliberately separate: `from`/`to` select *which
/// part of the source* plays, `at` says *where on the track* it lands. So the
/// same 7 seconds can be lifted out of the middle of a file and parked anywhere
/// on the timeline without touching the source.
#[derive(Debug, Clone, PartialEq)]
pub struct Clip {
    /// The cited source.
    pub source: Arc<Source>,
    /// Start position on the owning track.
    pub at: Duration,
    /// In-point, measured into the source.
    pub from: Duration,
    /// Out-point, measured into the source. `None` = play to the end of the
    /// source, which makes the clip's own length unknown if the source's length
    /// is unknown too.
    pub to: Option<Duration>,
}

impl Clip {
    /// The whole source, parked at the track origin.
    pub fn new(source: Arc<Source>) -> Self {
        Self {
            source,
            at: Duration::ZERO,
            from: Duration::ZERO,
            to: None,
        }
    }

    /// The `from .. to` slice of a source, parked at the track origin.
    pub fn sliced(source: Arc<Source>, from: Duration, to: impl Into<Option<Duration>>) -> Self {
        Self {
            source,
            at: Duration::ZERO,
            from,
            to: to.into(),
        }
    }

    /// Move the clip to `at` on the track.
    #[must_use]
    pub fn at(mut self, at: Duration) -> Self {
        self.at = at;
        self
    }

    /// Set the out-point.
    #[must_use]
    pub fn to(mut self, to: impl Into<Option<Duration>>) -> Self {
        self.to = to.into();
        self
    }

    /// How long this clip occupies. `None` when the out-point is open *and* the
    /// source has no known length.
    #[must_use]
    pub fn duration(&self) -> Option<Duration> {
        match self.to {
            Some(to) => Some(to.saturating_sub(self.from)),
            None => Some(self.source.duration?.saturating_sub(self.from)),
        }
    }

    /// Where this clip stops occupying the track.
    #[must_use]
    pub fn end(&self) -> Option<Duration> {
        Some(self.at + self.duration()?)
    }

    /// Whether track time `t` falls inside this clip. Half-open: `at` is
    /// included, `end()` is not. An unknown length extends to infinity.
    #[must_use]
    pub fn covers(&self, t: Duration) -> bool {
        t >= self.at && self.end().is_none_or(|end| t < end)
    }

    /// Whether two clips fight over the same track timecode.
    #[must_use]
    pub fn overlaps(&self, other: &Self) -> bool {
        // Only a clip that finishes at or before the other one starts is clear of it.
        let clear = |a: &Clip, b: &Clip| a.end().is_some_and(|end| end <= b.at);
        !(clear(self, other) || clear(other, self))
    }
}

/// Why a [`Track::insert`] was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overlap {
    /// The position the rejected clip wanted.
    pub at: Duration,
    /// Index of the clip it collided with.
    pub conflict: usize,
}

impl std::fmt::Display for Overlap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "clip at {}s collides with clip #{}",
            self.at.as_secs_f64(),
            self.conflict
        )
    }
}

impl std::error::Error for Overlap {}

/// One timeline: clips sorted by start position, never overlapping.
///
/// Non-overlap is this type's invariant — a single track cannot play two
/// sources at the same timecode. Stacking happens *across* tracks, which is what
/// [`Player`](crate::engine::Player) mixes.
#[derive(Debug, Clone)]
pub struct Track {
    name: Option<String>,
    volume: f32,
    muted: bool,
    clips: Vec<Clip>,
}

impl Default for Track {
    fn default() -> Self {
        Self {
            name: None,
            volume: 1.0,
            muted: false,
            clips: Vec::new(),
        }
    }
}

impl Track {
    /// An unnamed track.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A named track; names are labels for humans and agents, not identity.
    #[must_use]
    pub fn named(name: impl Into<String>) -> Self {
        Self {
            name: Some(name.into()),
            volume: 1.0,
            muted: false,
            clips: Vec::new(),
        }
    }

    /// Label, if set.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Relabel the track.
    pub fn set_name(&mut self, name: impl Into<String>) {
        self.name = Some(name.into());
    }

    /// Gain of this track in the mix, `0.0 ..= 1.0`; full gain by default.
    ///
    /// Volume is arrangement data — the backend reads it from the track when
    /// planning a mix, so gain can never drift from the arrangement.
    #[must_use]
    pub fn volume(&self) -> f32 {
        self.volume
    }

    /// Set the track's gain in the mix, clamped to `0.0 ..= 1.0`.
    pub fn set_volume(&mut self, volume: f32) {
        self.volume = volume.clamp(0.0, 1.0);
    }

    /// Whether the track is muted in the mix.
    #[must_use]
    pub fn muted(&self) -> bool {
        self.muted
    }

    /// Mute or unmute the track. A muted track contributes nothing to the
    /// mix regardless of its volume.
    pub fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
    }

    /// The clips, ordered by track position.
    #[must_use]
    pub fn clips(&self) -> &[Clip] {
        &self.clips
    }

    /// Number of clips.
    #[must_use]
    pub fn len(&self) -> usize {
        self.clips.len()
    }

    /// Whether the track holds nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.clips.is_empty()
    }

    /// Insert a clip, preserving order and the non-overlap invariant.
    ///
    /// On collision the clip is handed back untouched, so a refused insert
    /// leaves the track exactly as it was.
    pub fn insert(&mut self, clip: Clip) -> Result<usize, (Clip, Overlap)> {
        if let Some(conflict) = self.clips.iter().position(|c| c.overlaps(&clip)) {
            let at = clip.at;
            return Err((clip, Overlap { at, conflict }));
        }
        let at = clip.at;
        let index = self.clips.partition_point(|c| c.at < at);
        self.clips.insert(index, clip);
        Ok(index)
    }

    /// Append a clip after the current tail.
    ///
    /// A track whose tail is unknowable (an open-ended clip on an unprobed
    /// source) has no "after", so the clip is attempted at position 0 and
    /// predictably refused.
    pub fn push(&mut self, mut clip: Clip) -> Result<usize, (Clip, Overlap)> {
        if let Some(tail) = self.duration() {
            clip.at = tail;
        }
        self.insert(clip)
    }

    /// Remove the clip at `index`.
    #[must_use]
    pub fn remove(&mut self, index: usize) -> Option<Clip> {
        (index < self.clips.len()).then(|| self.clips.remove(index))
    }

    /// Drop every clip, keeping the name.
    pub fn clear(&mut self) {
        self.clips.clear();
    }

    /// The furthest end of any clip; `None` if some clip has no knowable end.
    #[must_use]
    pub fn duration(&self) -> Option<Duration> {
        let mut tail = Duration::ZERO;
        for clip in &self.clips {
            tail = tail.max(clip.end()?);
        }
        Some(tail)
    }

    /// The clip occupying track time `t`, at most one by the invariant.
    #[must_use]
    pub fn clip_at(&self, t: Duration) -> Option<&Clip> {
        self.clips.iter().find(|c| c.covers(t))
    }

    /// Whether nothing plays at `t` — a gap, or past the end.
    #[must_use]
    pub fn is_silent_at(&self, t: Duration) -> bool {
        self.clip_at(t).is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    fn src(label: &str, len: Option<Duration>) -> Arc<Source> {
        Arc::new(Source {
            uri: format!("{label}.wav"),
            duration: len,
        })
    }

    #[test]
    fn clip_length_comes_from_the_source_slice() {
        let bed = src("bed", Some(secs(30)));
        assert_eq!(Clip::new(bed.clone()).duration(), Some(secs(30)));
        assert_eq!(
            Clip::sliced(bed.clone(), secs(5), secs(12)).duration(),
            Some(secs(7))
        );
        // Playing an unprobed source to its end: length unknown.
        assert_eq!(Clip::new(src("raw", None)).duration(), None);
        // An out-point before the in-point is not an error, just an empty clip.
        assert_eq!(
            Clip::sliced(bed.clone(), secs(9), secs(2)).duration(),
            Some(Duration::ZERO)
        );
        assert!(Clip::new(bed).covers(Duration::ZERO));
        assert!(src("raw", None).is_open_ended());
    }

    #[test]
    fn clips_on_one_track_may_not_overlap() {
        let a = src("a", Some(secs(10)));
        let b = src("b", Some(secs(4)));
        let mut t = Track::named("bed");
        assert_eq!(t.name(), Some("bed"));
        assert_eq!(t.insert(Clip::new(a.clone())), Ok(0));
        // Butt-joined at 10s: half-open spans, sharing an endpoint is fine.
        assert_eq!(t.insert(Clip::new(b.clone()).at(secs(10))), Ok(1));
        assert_eq!(
            t.clips().iter().map(|c| c.at).collect::<Vec<_>>(),
            [secs(0), secs(10)]
        );
        // Landing on top of a.
        let (returned, err) = t.insert(Clip::new(a.clone()).at(secs(3))).unwrap_err();
        assert_eq!(err.conflict, 0);
        assert_eq!(returned.at, secs(3), "the clip is handed back");
        assert_eq!(t.len(), 2, "a refused insert leaves no trace");
        assert_eq!(
            t.insert(Clip::sliced(b.clone(), secs(0), secs(3)).at(secs(3)))
                .unwrap_err()
                .1
                .conflict,
            0
        );
    }

    #[test]
    fn an_unbounded_clip_grows_forever() {
        // Unprobed source and no out-point: length unknowable, so it extends to
        // infinity and blocks everything after it.
        let mut t = Track::new();
        t.insert(Clip::new(src("live", None)).at(secs(2))).unwrap();
        assert_eq!(t.duration(), None);
        assert_eq!(
            t.insert(Clip::new(src("other", Some(secs(5)))).at(secs(60)))
                .unwrap_err()
                .1
                .conflict,
            0
        );
        assert!(
            t.clip_at(secs(9999)).is_some(),
            "it still covers later time"
        );
        assert!(t.clip_at(secs(1)).is_none(), "silent before it starts");
    }

    #[test]
    fn tracks_can_be_muted() {
        let mut t = Track::named("voice");
        assert!(!t.muted(), "audible by default");
        t.set_muted(true);
        assert!(t.muted());
        t.set_muted(false);
        assert!(!t.muted());
    }

    #[test]
    fn tracks_carry_their_own_volume() {
        let mut t = Track::named("bed");
        assert_eq!(t.volume(), 1.0, "full gain by default");
        t.set_volume(0.5);
        assert_eq!(t.volume(), 0.5);
        t.set_volume(2.5);
        assert_eq!(t.volume(), 1.0, "clamped");
        t.set_volume(-1.0);
        assert_eq!(t.volume(), 0.0);
        // Gain does not touch the timeline.
        t.insert(Clip::new(src("a", Some(secs(10))))).unwrap();
        assert_eq!(t.duration(), Some(secs(10)));
    }

    #[test]
    fn push_appends_after_the_tail() {
        let mut t = Track::new();
        assert_eq!(t.duration(), Some(Duration::ZERO));
        t.push(Clip::new(src("a", Some(secs(10))))).unwrap();
        t.push(Clip::sliced(src("b", Some(secs(4))), secs(1), secs(3)))
            .unwrap();
        assert_eq!(
            t.clips()[1].at,
            secs(10),
            "second one starts where first ends"
        );
        assert_eq!(t.duration(), Some(secs(12)));
        assert_eq!(t.clip_at(secs(11)).unwrap().source.uri, "b.wav");
        assert!(t.remove(0).is_some());
        assert_eq!(t.len(), 1);
        assert!(t.remove(9).is_none());
        t.clear();
        assert!(t.is_empty() && t.is_silent_at(Duration::ZERO));
    }
}
