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
}

impl Source {
    /// An addressable audio resource. Length lives on [`Clip`], not here:
    /// every clip is a finite, measured slice.
    pub fn new(uri: impl Into<String>) -> Self {
        Self { uri: uri.into() }
    }

    /// A shareable reference, the form [`Clip`] wants.
    pub fn shared(uri: impl Into<String>) -> Arc<Self> {
        Arc::new(Self::new(uri))
    }
}

/// The shape of a [`Fade`]'s amplitude ramp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FadeShape {
    /// A linear ramp.
    #[default]
    Linear,
}

impl FadeShape {
    /// Map a ramp position `x` in `0..=1` to a gain in `0..=1`.
    #[must_use]
    pub fn ramp(self, x: f32) -> f32 {
        let x = x.clamp(0.0, 1.0);
        match self {
            Self::Linear => x,
        }
    }
}

impl std::fmt::Display for FadeShape {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Linear => "linear",
        })
    }
}

impl std::str::FromStr for FadeShape {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim() {
            "linear" => Ok(Self::Linear),
            other => Err(format!("bad fade shape {other:?}: linear")),
        }
    }
}

/// A clip's amplitude envelope: fade in at the start, fade out at the end.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Fade {
    /// Ramp up to full over this long, at the clip's start.
    pub fade_in: Duration,
    /// The level the fade-in starts from, `0.0 ..= 1.0`; silence by default.
    pub fade_in_from: f32,
    /// Ramp down over this long, at the clip's end.
    pub fade_out: Duration,
    /// The level the fade-out ends at, `0.0 ..= 1.0`; silence by default.
    pub fade_out_to: f32,
    /// The curve of both ramps.
    pub shape: FadeShape,
}

impl Fade {
    /// Whether neither edge fades.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fade_in.is_zero() && self.fade_out.is_zero()
    }

    /// The envelope's gain at `pos` into a `length`-long span. The fade-in
    /// ramps `fade_in_from → 1.0` from the span's start, the fade-out ramps
    /// `1.0 → fade_out_to` into the span's end; the two multiply where they
    /// overlap (a span too short for both).
    #[must_use]
    pub fn gain_at(&self, pos: Duration, length: Duration) -> f32 {
        if length.is_zero() {
            return 1.0;
        }
        let mut gain = 1.0;
        let fade_in = self.fade_in.min(length);
        if pos < fade_in {
            let x = (pos.as_secs_f64() / fade_in.as_secs_f64()) as f32;
            let ramp = self.shape.ramp(x);
            gain *= self.fade_in_from + (1.0 - self.fade_in_from) * ramp;
        }
        if !self.fade_out.is_zero() {
            let fade_out_start = length.saturating_sub(self.fade_out);
            if pos >= fade_out_start {
                let x = (pos.saturating_sub(fade_out_start).as_secs_f64()
                    / self.fade_out.as_secs_f64()) as f32;
                let ramp = self.shape.ramp(x);
                gain *= self.fade_out_to + (1.0 - self.fade_out_to) * (1.0 - ramp);
            }
        }
        gain
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
    /// Stable handle assigned by [`Track::insert`]: never reused while the
    /// clip lives, untouched by later inserts or removals. `0` until placed.
    pub id: u64,
    /// The cited source.
    pub source: Arc<Source>,
    /// Start position on the owning track.
    pub at: Duration,
    /// In-point, measured into the source.
    pub from: Duration,
    /// Out-point, measured into the source.
    pub to: Duration,
    /// Gain applied to this clip in the mix, `0.0 ..= 1.0`; full by default.
    pub gain: f32,
    /// The fade envelope.
    pub fade: Fade,
}

impl Clip {
    /// The whole `length`-long source, parked at the track origin.
    pub fn new(source: Arc<Source>, length: Duration) -> Self {
        Self {
            id: 0,
            source,
            at: Duration::ZERO,
            from: Duration::ZERO,
            to: length,
            gain: 1.0,
            fade: Fade::default(),
        }
    }

    /// The `from .. to` slice of a source, parked at the track origin.
    pub fn sliced(source: Arc<Source>, from: Duration, to: Duration) -> Self {
        Self {
            id: 0,
            source,
            at: Duration::ZERO,
            from,
            to,
            gain: 1.0,
            fade: Fade::default(),
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
    pub fn to(mut self, to: Duration) -> Self {
        self.to = to;
        self
    }

    /// Set the clip's gain, clamped to `0.0 ..= 1.0`.
    #[must_use]
    pub fn gain(mut self, gain: f32) -> Self {
        self.gain = gain.clamp(0.0, 1.0);
        self
    }

    /// Set the clip's fade envelope.
    #[must_use]
    pub fn fade(mut self, fade: Fade) -> Self {
        self.fade = fade;
        self
    }

    /// How long this clip occupies.
    #[must_use]
    pub fn duration(&self) -> Duration {
        self.to.saturating_sub(self.from)
    }

    /// Where this clip stops occupying the track.
    #[must_use]
    pub fn end(&self) -> Duration {
        self.at + self.duration()
    }

    /// Whether track time `t` falls inside this clip. Half-open: `at` is
    /// included, `end()` is not.
    #[must_use]
    pub fn covers(&self, t: Duration) -> bool {
        t >= self.at && t < self.end()
    }

    /// Whether two clips fight over the same track timecode.
    #[must_use]
    pub fn overlaps(&self, other: &Self) -> bool {
        // Only a clip that finishes at or before the other one starts is clear of it.
        let clear = |a: &Clip, b: &Clip| a.end() <= b.at;
        !(clear(self, other) || clear(other, self))
    }
}

/// Why a [`Track::insert`] was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overlap {
    /// The position the rejected clip wanted.
    pub at: Duration,
    /// Id of the clip it collided with.
    pub conflict: u64,
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
    next_id: u64,
}

impl Default for Track {
    fn default() -> Self {
        Self {
            name: None,
            volume: 1.0,
            muted: false,
            clips: Vec::new(),
            next_id: 0,
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
            next_id: 0,
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

    /// Insert a clip, preserving order and the non-overlap invariant, and
    /// return the stable id it was assigned.
    ///
    /// On collision the clip is handed back untouched, so a refused insert
    /// leaves the track exactly as it was.
    // The error hands the whole clip back with the collision; the error path
    // is rare and cold, so its size is not worth boxing the public shape.
    #[allow(clippy::result_large_err)]
    pub fn insert(&mut self, mut clip: Clip) -> Result<u64, (Clip, Overlap)> {
        if let Some(conflict) = self.clips.iter().find(|c| c.overlaps(&clip)) {
            let at = clip.at;
            let conflict = conflict.id;
            return Err((clip, Overlap { at, conflict }));
        }
        clip.id = self.next_id;
        self.next_id += 1;
        let id = clip.id;
        let at = clip.at;
        let index = self.clips.partition_point(|c| c.at < at);
        self.clips.insert(index, clip);
        Ok(id)
    }

    /// Append a clip after the current tail.
    #[allow(clippy::result_large_err)]
    pub fn push(&mut self, mut clip: Clip) -> Result<u64, (Clip, Overlap)> {
        clip.at = self.duration();
        self.insert(clip)
    }

    /// Place a clip that already carries a stable id — a clip moved within
    /// or between tracks keeps its identity. The caller must have verified
    /// the spot is free and that no resident clip carries the same id;
    /// nothing is checked here. The id counter is advanced past the planted
    /// id, so a later `insert` can never collide with it.
    pub fn insert_keeping_id(&mut self, clip: Clip) {
        let at = clip.at;
        self.next_id = self.next_id.max(clip.id + 1);
        let index = self.clips.partition_point(|c| c.at < at);
        self.clips.insert(index, clip);
    }

    /// Remove the clip with `id`. Ids are never reused, so a removed id stays
    /// gone.
    #[must_use]
    pub fn remove(&mut self, id: u64) -> Option<Clip> {
        let index = self.clips.iter().position(|c| c.id == id)?;
        Some(self.clips.remove(index))
    }

    /// Drop every clip, keeping the name; ids restart from zero.
    pub fn clear(&mut self) {
        self.clips.clear();
        self.next_id = 0;
    }

    /// The furthest end of any clip.
    #[must_use]
    pub fn duration(&self) -> Duration {
        self.clips
            .iter()
            .fold(Duration::ZERO, |tail, clip| tail.max(clip.end()))
    }

    /// The clip occupying track time `t`, at most one by the invariant.
    #[must_use]
    pub fn clip_at(&self, t: Duration) -> Option<&Clip> {
        self.clips.iter().find(|c| c.covers(t))
    }

    /// The clip with `id`, mutably.
    #[must_use]
    pub fn clip_mut(&mut self, id: u64) -> Option<&mut Clip> {
        self.clips.iter_mut().find(|c| c.id == id)
    }

    /// The earliest position at or after `at` where a clip of length `len`
    /// can be placed without overlapping any resident clip: the answer a
    /// collision report points at.
    ///
    /// Spans are half-open, so the answer may butt against a clip's end.
    #[must_use]
    pub fn next_free_start(&self, at: Duration, len: Duration) -> Duration {
        let mut pos = at;
        for clip in &self.clips {
            let end = clip.end();
            if end <= pos {
                continue; // entirely behind the candidate
            }
            if clip.at.saturating_sub(pos) >= len {
                return pos;
            }
            pos = pos.max(end); // jump past this clip
        }
        // Past every clip: the tail is free.
        pos
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

    fn src(label: &str) -> Arc<Source> {
        Arc::new(Source {
            uri: format!("{label}.wav"),
        })
    }

    #[test]
    fn clip_length_comes_from_the_source_slice() {
        let bed = src("bed");
        assert_eq!(Clip::new(bed.clone(), secs(30)).duration(), secs(30));
        assert_eq!(
            Clip::sliced(bed.clone(), secs(5), secs(12)).duration(),
            secs(7)
        );
        // An out-point before the in-point is not an error, just an empty clip.
        assert_eq!(
            Clip::sliced(bed.clone(), secs(9), secs(2)).duration(),
            Duration::ZERO
        );
        assert!(Clip::new(bed, secs(30)).covers(Duration::ZERO));
    }

    #[test]
    fn clips_on_one_track_may_not_overlap() {
        let a = src("a");
        let b = src("b");
        let mut t = Track::named("bed");
        assert_eq!(t.name(), Some("bed"));
        assert_eq!(t.insert(Clip::new(a.clone(), secs(10))), Ok(0));
        // Butt-joined at 10s: half-open spans, sharing an endpoint is fine.
        assert_eq!(t.insert(Clip::new(b.clone(), secs(4)).at(secs(10))), Ok(1));
        assert_eq!(
            t.clips().iter().map(|c| c.at).collect::<Vec<_>>(),
            [secs(0), secs(10)]
        );
        // Landing on top of a.
        let (returned, err) = t.insert(Clip::new(a.clone(), secs(10)).at(secs(3))).unwrap_err();
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
        t.insert(Clip::new(src("a"), secs(10))).unwrap();
        assert_eq!(t.duration(), secs(10));
    }

    #[test]
    fn push_appends_after_the_tail() {
        let mut t = Track::new();
        assert_eq!(t.duration(), Duration::ZERO);
        t.push(Clip::new(src("a"), secs(10))).unwrap();
        t.push(Clip::sliced(src("b"), secs(1), secs(3))).unwrap();
        assert_eq!(
            t.clips()[1].at,
            secs(10),
            "second one starts where first ends"
        );
        assert_eq!(t.duration(), secs(12));
        assert_eq!(t.clip_at(secs(11)).unwrap().source.uri, "b.wav");
        assert!(t.remove(0).is_some());
        assert_eq!(t.len(), 1);
        assert!(t.remove(9).is_none());
        t.clear();
        assert!(t.is_empty() && t.is_silent_at(Duration::ZERO));
    }

    #[test]
    fn next_free_start_skips_to_the_first_fit() {
        let mut t = Track::new();
        t.insert(Clip::new(src("a"), secs(10))).unwrap();
        t.insert(Clip::new(src("b"), secs(5)).at(secs(10))).unwrap();
        t.insert(Clip::new(src("c"), secs(2)).at(secs(20))).unwrap();
        // An empty track fits anywhere.
        assert_eq!(Track::new().next_free_start(secs(3), secs(2)), secs(3));
        // Inside a clip: jump past every resident until a gap fits. b sits
        // at 10..15 right after a, so the first fit past 3 is 15.
        assert_eq!(t.next_free_start(secs(3), secs(1)), secs(15));
        // A clip that fits in the gap between residents (b ends 15, c at 20).
        assert_eq!(t.next_free_start(secs(12), secs(2)), secs(15));
        // A clip too long for that gap jumps to the next one (tail past 22).
        assert_eq!(t.next_free_start(secs(12), secs(6)), secs(22));
        // A start already in a gap stays put; so does a butt-join at its end.
        assert_eq!(t.next_free_start(secs(15), secs(4)), secs(15));
        assert_eq!(t.next_free_start(secs(17), secs(2)), secs(17));
        assert_eq!(t.next_free_start(secs(15), secs(5)), secs(15));
        // Butt-joining a's end is not free while b occupies 10..15.
        assert_eq!(t.next_free_start(secs(10), secs(5)), secs(15));
    }

    #[test]
    fn fade_gain_ramps_linearly_at_both_edges() {
        let f = Fade {
            fade_in: secs(2),
            fade_in_from: 0.0,
            fade_out: secs(2),
            fade_out_to: 0.0,
            shape: FadeShape::Linear,
        };
        let len = secs(10);
        assert_eq!(f.gain_at(Duration::ZERO, len), 0.0);
        assert!((f.gain_at(secs(1), len) - 0.5).abs() < 1e-6);
        assert_eq!(f.gain_at(secs(2), len), 1.0);
        assert_eq!(f.gain_at(secs(5), len), 1.0, "mid-clip is unity");
        assert!((f.gain_at(secs(9), len) - 0.5).abs() < 1e-6);
        assert!(f.gain_at(secs(10), len).abs() < 1e-6, "the ramp reaches silence");
        // An empty fade is unity everywhere.
        assert_eq!(Fade::default().gain_at(secs(3), len), 1.0);
        assert!(Fade::default().is_empty());
    }

    #[test]
    fn fade_from_and_to_levels_hold_at_the_edges() {
        // Fade in from -10 dB (0.3) to full, fade out from full to 0.3.
        let f = Fade {
            fade_in: secs(2),
            fade_in_from: 0.3,
            fade_out: secs(2),
            fade_out_to: 0.3,
            shape: FadeShape::Linear,
        };
        let len = secs(10);
        assert!((f.gain_at(Duration::ZERO, len) - 0.3).abs() < 1e-6);
        assert!((f.gain_at(secs(1), len) - 0.65).abs() < 1e-6);
        assert_eq!(f.gain_at(secs(5), len), 1.0);
        assert!((f.gain_at(secs(9), len) - 0.65).abs() < 1e-6);
        assert!((f.gain_at(secs(10), len) - 0.3).abs() < 1e-6);
    }
}
