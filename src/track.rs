//! The content layer: the three data structures that describe *what plays and
//! when* — [`Source`] (an addressable audio resource), [`Clip`] (a slice of a
//! source, placed at a timecode on a track) and [`Track`] (a container of
//! clips on one parallel timeline).
//!
//! Timecodes are `std::time::Duration` measured from their own origin: a clip's
//! `from`/`to` count from the start of the source, a clip's `at` and a player's
//! playhead count from the start of the track.
//!
//! Kept deliberately small, and deliberately apart from the signal layer
//! ([`crate::bus`]): nothing here says how a sound is *mixed* — that lives on
//! a track's output edge, not on the content. No transport, no decoding, no
//! metadata — just the shape those layers will sit on.

use std::sync::Arc;
use std::time::Duration;

use crate::bus::{BusRef, Output, Placement};

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

/// One breakpoint of a [`Curve`]: the offset it outputs at a clip-local
/// timecode. Clip-local, so a curve rides its clip: move the clip and the
/// whole curve moves with it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Keyframe {
    /// Where on the clip this point sits, measured from the clip's start.
    pub at: Duration,
    /// The offset the source outputs here — an add-on to the parameter's
    /// static base, not an absolute value.
    pub value: f32,
}

/// A hand-drawn curve: the first control source (in GStreamer terms, a src
/// — it only produces). Its signal is a scalar offset over the clip's own
/// time, linear between keyframes and held flat beyond the first and last,
/// so a clip that carries one has a value at every moment it plays. The
/// broader notion — using curves to drive parameters as they play, live and
/// rendered alike — is *automation*; this struct is the concrete curve a
/// curve-automation is built on.
///
/// A curve knows nothing about which parameter it drives or what that
/// parameter allows (the jack clamps); it only answers "what offset at this
/// moment". An empty curve is silence: an offset of zero.
#[derive(Debug, Clone, PartialEq)]
pub struct Curve {
    keyframes: Vec<Keyframe>,
}

impl Curve {
    /// A curve over the given breakpoints. They are sorted by time on the
    /// way in, so authoring order never matters; at a time shared by two
    /// keyframes the later one wins.
    pub fn new(mut keyframes: Vec<Keyframe>) -> Self {
        keyframes.sort_by_key(|k| k.at);
        Self { keyframes }
    }

    /// Whether the curve carries no breakpoints — and so no signal.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keyframes.is_empty()
    }

    /// The breakpoints, in time order.
    #[must_use]
    pub fn keyframes(&self) -> &[Keyframe] {
        &self.keyframes
    }

    /// The curve's signal at clip-local time `t`: linear between two
    /// keyframes, held flat before the first and at the last. Empty — no
    /// signal — is zero.
    #[must_use]
    pub fn value_at(&self, t: Duration) -> f32 {
        let ks = &self.keyframes;
        let Some(first) = ks.first() else {
            return 0.0;
        };
        if ks.len() == 1 {
            return first.value;
        }
        if t < first.at {
            return first.value;
        }
        for pair in ks.windows(2) {
            let a = &pair[0];
            let b = &pair[1];
            if t >= a.at && t < b.at {
                if b.at == a.at {
                    return b.value; // unreachable in sorted input, kept honest
                }
                let x = (t - a.at).as_secs_f64() / (b.at - a.at).as_secs_f64();
                return a.value + (b.value - a.value) * x as f32;
            }
        }
        ks.last().expect("nonempty").value
    }
}

impl std::fmt::Display for Curve {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (i, k) in self.keyframes.iter().enumerate() {
            if i > 0 {
                f.write_str(",")?;
            }
            write!(f, "{}:{}", k.at.as_secs_f64(), k.value)?;
        }
        Ok(())
    }
}

impl std::str::FromStr for Curve {
    type Err = String;

    /// `time:value[,time:value...]` — times in seconds, relative to the
    /// clip's start. An empty string is an empty curve.
    fn from_str(s: &str) -> Result<Self, String> {
        let s = s.trim();
        if s.is_empty() {
            return Ok(Self::new(Vec::new()));
        }
        let mut keyframes = Vec::new();
        for part in s.split(',') {
            let part = part.trim();
            let (at, value) = part.split_once(':').ok_or_else(|| {
                format!("bad keyframe {part:?}: expected TIME:VALUE")
            })?;
            let secs: f64 = at.trim()
                .parse()
                .map_err(|_| format!("bad keyframe time {at:?}: seconds"))?;
            if secs < 0.0 {
                return Err(format!("bad keyframe time {at:?}: not negative"));
            }
            let at = std::time::Duration::try_from_secs_f64(secs)
                .map_err(|_| format!("bad keyframe time {at:?}"))?;
            let value: f32 = value.trim().trim_start_matches('+').parse().map_err(|_| {
                format!("bad keyframe value {value:?}: a number")
            })?;
            keyframes.push(Keyframe { at, value });
        }
        Ok(Self::new(keyframes))
    }
}

/// A control source — the "cable" plugged into a parameter's input. Every
/// source is a scalar over the clip's own time, produced where the samples
/// flow; the parameter it drives is the static base plus the sum of its
/// active sources (a parameter with no cable is just its base).
///
/// v1 ships exactly one kind of source, the hand-drawn curve; further srcs
/// (an LFO) and filters (an envelope follower — the detector side of a
/// future sidechain) slot in as further variants. Automation is the notion
/// of using such a source to drive a parameter; this enum is the registry of
/// concrete sources automation can draw on.
#[derive(Debug, Clone, PartialEq)]
pub enum ControlSource {
    /// A hand-drawn keyframe curve ([`Curve`]).
    Curve(Curve),
}

impl ControlSource {
    /// This source's signal at clip-local time `t` — an offset on top of
    /// the parameter's static base.
    #[must_use]
    pub fn value_at(&self, t: Duration) -> f32 {
        match self {
            Self::Curve(curve) => curve.value_at(t),
        }
    }
}

impl std::fmt::Display for ControlSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Curve(curve) => curve.fmt(f),
        }
    }
}

impl std::str::FromStr for ControlSource {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        Ok(Self::Curve(s.parse()?))
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
    /// Control sources on this clip's gain — its own gain input. A clip's
    /// gain is the static `gain` above plus the sum of its active sources,
    /// clamped to `0.0 ..= 1.0`; an empty list is no cable.
    pub gain_controls: Vec<ControlSource>,
    /// The fade envelope.
    pub fade: Fade,
    /// Where this clip sits in the bus space, when it does not follow its
    /// track's output. `None` (the default) inherits the track's placement;
    /// the surface for setting a per-clip placement is not open yet.
    pub placement: Option<Placement>,
    /// Control sources on this clip's pan — its own pan input. A clip's pan
    /// is the effective static position (the placement, or the track's pan
    /// when the clip has none) plus the sum of its active sources, clamped
    /// to the field; an empty list is no cable.
    pub pan_controls: Vec<ControlSource>,
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
            placement: None,
            gain_controls: Vec::new(),
            pan_controls: Vec::new(),
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
            placement: None,
            gain_controls: Vec::new(),
            pan_controls: Vec::new(),
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

/// The content layer's unit: a track is a container of clips on one parallel
/// timeline — nothing more.
///
/// It answers *what* plays and *when*: clips sorted by start position, never
/// overlapping (a single track cannot play two sources at the same timecode;
/// stacking happens *across* tracks, which is what
/// [`Player`](crate::engine::Player) mixes).
///
/// How a track *sounds* is not content, and does not live here: its gain,
/// its mute and where it sits in the mix belong to its [`Output`] — the
/// signal-layer edge (see [`crate::bus`]) that carries this track's clips
/// into a bus. Content is what plays; the signal layer is where the sound
/// goes. Neither borrows the other's concepts, so a track never mixes a
/// signal input in beside its clips, and the signal layer never grows a
/// timeline.
#[derive(Debug, Clone, Default)]
pub struct Track {
    name: Option<String>,
    out: Output,
    clips: Vec<Clip>,
    next_id: u64,
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
            out: Output::default(),
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

    /// This track's output edge: the strip (gain, mute, placement) and where
    /// its signal goes.
    #[must_use]
    pub fn out(&self) -> &Output {
        &self.out
    }

    /// Gain of this track's output in the mix, `0.0 ..= 1.0`; full gain by
    /// default.
    ///
    /// Volume is arrangement data — the backend reads it from the track when
    /// planning a mix, so gain can never drift from the arrangement.
    #[must_use]
    pub fn volume(&self) -> f32 {
        self.out.gain
    }

    /// Set the track's gain in the mix, clamped to `0.0 ..= 1.0`.
    pub fn set_volume(&mut self, volume: f32) {
        self.out.set_gain(volume);
    }

    /// Whether the track is muted in the mix.
    #[must_use]
    pub fn muted(&self) -> bool {
        self.out.muted
    }

    /// Mute or unmute the track. A muted track contributes nothing to the
    /// mix regardless of its volume.
    pub fn set_muted(&mut self, muted: bool) {
        self.out.muted = muted;
    }

    /// Where this track's output sits on the bus, `-1.0 ..= 1.0` (hard left
    /// to hard right). Zero — center — is the default.
    #[must_use]
    pub fn pan(&self) -> f32 {
        self.out.placement.position()
    }

    /// Set the track's placement on the bus, clamped to `-1.0 ..= 1.0`.
    pub fn set_pan(&mut self, pan: f32) {
        self.out.placement.set_position(pan);
    }

    /// Which bus this track's output feeds: the master, or a group bus by
    /// its id. The master is the default.
    #[must_use]
    pub fn bus(&self) -> BusRef {
        self.out.target
    }

    /// Route this track's output to a bus — the master, or a group bus.
    /// Routing is structure: only a rebuilt graph sounds it.
    pub fn set_bus(&mut self, target: BusRef) {
        self.out.target = target;
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
    fn tracks_carry_their_own_placement() {
        let mut t = Track::named("bed");
        assert_eq!(t.pan(), 0.0, "center by default");
        assert_eq!(t.out().placement.position(), 0.0);
        t.set_pan(-0.5);
        assert_eq!(t.pan(), -0.5);
        t.set_pan(2.0);
        assert_eq!(t.pan(), 1.0, "clamped to hard right");
        t.set_pan(-3.0);
        assert_eq!(t.pan(), -1.0, "clamped to hard left");
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
    fn tracks_can_be_routed_to_a_group_bus() {
        let mut t = Track::new();
        assert_eq!(t.bus(), BusRef::Master, "the master is the default");
        t.set_bus(BusRef::Group(2));
        assert_eq!(t.bus(), BusRef::Group(2));
        // Routing is signal, not content: the timeline does not move.
        t.insert(Clip::new(src("a"), secs(10))).unwrap();
        assert_eq!(t.duration(), secs(10));
        assert_eq!(t.bus(), BusRef::Group(2));
        t.set_bus(BusRef::Master);
        assert_eq!(t.bus(), BusRef::Master);
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
    fn a_curve_interpolates_between_keyframes_and_holds_outside() {
        let secs = |s: u64| Duration::from_secs(s);
        let curve = Curve::new(vec![
            Keyframe { at: Duration::ZERO, value: 1.0 },
            Keyframe { at: secs(4), value: -1.0 },
        ]);
        assert_eq!(curve.value_at(Duration::ZERO), 1.0);
        assert!((curve.value_at(secs(2)) - 0.0).abs() < 1e-6, "linear midpoint");
        assert!((curve.value_at(secs(3)) - (-0.5)).abs() < 1e-6);
        assert_eq!(curve.value_at(secs(4)), -1.0);
        // Outside the span the edges hold flat — before the first point too,
        // when the curve does not start at the clip's origin.
        assert_eq!(curve.value_at(secs(10)), -1.0);
        let late = Curve::new(vec![
            Keyframe { at: secs(2), value: 0.5 },
            Keyframe { at: secs(4), value: -0.5 },
        ]);
        assert_eq!(late.value_at(secs(1)), 0.5, "flat before the first point");
    }

    #[test]
    fn a_curve_sorts_authoring_order_and_a_single_point_is_constant() {
        let secs = |s: u64| Duration::from_secs(s);
        // Written back to front: sorted on the way in.
        let curve = Curve::new(vec![
            Keyframe { at: secs(3), value: -1.0 },
            Keyframe { at: secs(1), value: 1.0 },
            Keyframe { at: secs(2), value: 0.0 },
        ]);
        let ats: Vec<u64> = curve.keyframes().iter().map(|k| k.at.as_secs()).collect();
        assert_eq!(ats, vec![1, 2, 3]);
        assert!((curve.value_at(secs(2)) - 0.0).abs() < 1e-6);
        // One point: that value everywhere.
        let flat = Curve::new(vec![Keyframe { at: Duration::ZERO, value: -0.5 }]);
        assert_eq!(flat.value_at(secs(9)), -0.5);
        // Empty: silence, an offset of zero.
        assert_eq!(Curve::new(Vec::new()).value_at(secs(1)), 0.0);
    }

    #[test]
    fn a_curve_round_trips_through_its_text() {
        let curve = Curve::new(vec![
            Keyframe { at: Duration::from_secs_f64(0.0), value: 1.0 },
            Keyframe { at: Duration::from_secs_f64(3.2), value: -1.0 },
            Keyframe { at: Duration::from_secs_f64(4.0), value: 0.5 },
        ]);
        let text = curve.to_string();
        assert_eq!(text, "0:1,3.2:-1,4:0.5", "{text}");
        assert_eq!(text.parse::<Curve>().unwrap(), curve, "display and parse agree");
        // Authoring may lead values with a sign and scatter the order.
        assert_eq!(
            "3.2:-1,+0:1,4:+0.5".parse::<Curve>().unwrap(),
            curve,
            "order and '+' are forgiven"
        );
        assert_eq!(" ".parse::<Curve>().unwrap(), Curve::new(Vec::new()));
        assert!("1".parse::<Curve>().is_err(), "no ':' is refused");
        assert!("x:1".parse::<Curve>().is_err());
        assert!("1:x".parse::<Curve>().is_err());
        assert!("-1:0".parse::<Curve>().is_err(), "negative time is refused");
    }

    #[test]
    fn a_control_source_delegates_to_its_curve() {
        let curve = Curve::new(vec![
            Keyframe { at: Duration::ZERO, value: 1.0 },
            Keyframe { at: Duration::from_secs(2), value: -1.0 },
        ]);
        let source = ControlSource::Curve(curve);
        assert_eq!(source.value_at(Duration::from_secs(1)), 0.0);
        assert_eq!(source.to_string(), "0:1,2:-1");
        assert_eq!("0:1,2:-1".parse::<ControlSource>().unwrap(), source);
    }

    #[test]
    fn clips_come_with_no_cable_plugged_in() {
        let c = Clip::new(src("a"), secs(10));
        assert!(c.pan_controls.is_empty());
        assert!(c.gain_controls.is_empty());
        let s = Clip::sliced(src("b"), Duration::ZERO, secs(5));
        assert!(s.pan_controls.is_empty());
        assert!(s.gain_controls.is_empty());
    }
}
