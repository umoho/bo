//! The command surface as a library: a [`Bo`] session you drive with typed
//! calls.
//!
//! The CLI speaks one command per invocation to a long-running daemon; this
//! module is the same vocabulary in process. [`Bo`] owns the arrangement and
//! the transport (like the daemon does) and each method is one command —
//! place a clip with [`Bo::put`], tune it, play it, render it.
//!
//! A [`Bo`] is its own session, born stopped and empty on the silent
//! backend, so nothing needs a sound device. Make it audible with
//! [`Bo::with_backend`] and a real backend.
//!
//! # Placing a clip
//!
//! [`Bo::put`] takes three plain things: a source address (a file the caller
//! manages — bo does not open or probe it unless it must), a [`Slice`] (the
//! `from..to` window into that source, or the whole source), and a
//! [`TrackPos`] (a track and a timecode). A clip with an open end (a slice
//! whose `to` is `None`) is measured when it is put, because only decoding
//! the source can say where it ends; a closed slice touches no disk at all
//! until play or render.
//!
//! ```no_run
//! use bo::session::{Bo, Slice, TrackRef};
//! use std::time::Duration;
//!
//! let mut bo = Bo::new();
//! // The 1:00–2:00 window of the file, on track 0 at 30 s in:
//! let put = bo.put("bed.wav", "1:00-2:00".parse()?, TrackRef(0).at(Duration::from_secs(30)))?;
//! assert_eq!(put.track, 0);
//! # Ok::<(), bo::session::Error>(())
//! ```

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use crate::engine::rodio::probe;
use crate::engine::{Backend, BackendError, Change, Landed, Player, Silent};
use crate::time;
use crate::track::{Clip, Fade, Source, Track};

/// A session: an arrangement (stacked tracks of clips) and a transport.
///
/// [`Bo::new`] is a fresh session on the silent backend — deterministic,
/// needs no device, and makes no sound; swap in a real backend (e.g.
/// `engine::rodio::Rodio`) with [`Bo::with_backend`] to hear it.
///
/// Methods are the CLI verbs with the CLI's semantics, minus the reply
/// grammar: an edit a running graph can take lands as it is made, one it
/// cannot waits for the next `apply` — the [`Landed`] report says which.
#[derive(Debug)]
pub struct Bo<B: Backend = Silent> {
    player: Player<B>,
}

impl Default for Bo<Silent> {
    fn default() -> Self {
        Self::new()
    }
}

impl Bo<Silent> {
    /// A fresh, empty session: the silent backend, stopped at zero.
    #[must_use]
    pub fn new() -> Self {
        Self {
            player: Player::default(),
        }
    }
}

impl<B: Backend> Bo<B> {
    /// A session that plays through `backend` (e.g. `rodio::Rodio::try_new()`).
    #[must_use]
    pub fn with_backend(backend: B) -> Self {
        Self {
            player: Player::new(backend),
        }
    }

    /// Place a clip: the `from..to` window `slice` of source `uri`, on
    /// `on.track` at track-time `on.at`.
    ///
    /// A refused put — a slice with no end whose source cannot be measured,
    /// or a placement that collides with a resident clip — is an `Err` and
    /// leaves the session exactly as it was.
    ///
    /// The track is grown to fit: `put(uri, slice, TrackRef(7).at(..))`
    /// creates the missing tracks. A clip placed past the end of a running
    /// track's queue joins that queue as it is placed; anything else waits
    /// for an `apply` — see [`Put::landed`].
    pub fn put(&mut self, uri: &str, slice: Slice, on: TrackPos) -> Result<Put, Error> {
        // An open slice plays to the source's end; resolve that end now, so
        // every clip has a known finite length and none can silently block
        // its track. Same refusal the CLI makes.
        let to = match slice.to {
            Some(to) => to,
            None => probe(uri).map_err(|why| Error::Probe {
                uri: uri.to_string(),
                why,
            })?,
        };
        // The track is addressed by index, created on demand like the CLI's.
        while self.player.tracks().len() <= on.track {
            self.player.add_track(Track::new());
        }
        let clip = Clip::sliced(Arc::new(Source::new(uri)), slice.from, to)
            .at(on.at)
            .gain(1.0)
            .fade(Fade::default());
        // Refuse a collision before inserting anything: a rejected put
        // leaves no trace, and says where the clip could go instead.
        let view = &self.player.tracks()[on.track];
        if let Some(conflict) = view.clips().iter().find(|c| c.overlaps(&clip)) {
            return Err(Error::Overlap(Overlap {
                track: on.track,
                at: clip.at,
                conflict: conflict.id,
                conflict_at: conflict.at,
                conflict_end: conflict.end(),
                next_free: view.next_free_start(clip.at, clip.duration()),
            }));
        }
        let id = self.player.tracks_mut()[on.track]
            .insert(clip)
            .expect("pre-checked: the insert cannot collide");
        // Placement past the queued tail joins the running graph; the rest
        // waits for an apply — exactly what the daemon does.
        let landed = self.player.changed(Change::Appended(on.track));
        Ok(Put {
            track: on.track,
            clip: PlacedClip {
                id,
                uri: uri.to_string(),
                at: on.at,
                from: slice.from,
                to,
                gain: 1.0,
                fade: Fade::default(),
            },
            landed,
        })
    }

    /// The tracks, in order.
    #[must_use]
    pub fn tracks(&self) -> &[Track] {
        self.player.tracks()
    }

    /// The whole arrangement's length: the latest end across tracks.
    #[must_use]
    pub fn duration(&self) -> Duration {
        self.player.duration()
    }

    /// The playhead timecode.
    #[must_use]
    pub fn playhead(&self) -> Duration {
        self.player.playhead()
    }
}

/// A track, addressed by its index. `put` grows the session to fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TrackRef(pub usize);

impl TrackRef {
    /// A position on this track: `TrackRef(0).at(t)` — the CLI's `0@t`.
    #[must_use]
    pub const fn at(self, at: Duration) -> TrackPos {
        TrackPos {
            track: self.0,
            at,
        }
    }
}

impl From<usize> for TrackRef {
    fn from(track: usize) -> Self {
        Self(track)
    }
}

impl From<TrackRef> for usize {
    fn from(track: TrackRef) -> Self {
        track.0
    }
}

impl fmt::Display for TrackRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A track and a timecode: where a clip lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackPos {
    /// Track index, created on demand by a put.
    pub track: usize,
    /// Position on that track.
    pub at: Duration,
}

impl From<(usize, Duration)> for TrackPos {
    fn from((track, at): (usize, Duration)) -> Self {
        Self { track, at }
    }
}

/// A `from..to` window into a source: where the clip starts reading and
/// where it stops. `to: None` means the source's end — resolved by probing
/// when the clip is put (the CLI's `uri,from-`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Slice {
    /// In-point, measured into the source.
    pub from: Duration,
    /// Out-point, measured into the source; `None` = the source's end.
    pub to: Option<Duration>,
}

impl Slice {
    /// The whole source.
    #[must_use]
    pub fn whole() -> Self {
        Self {
            from: Duration::ZERO,
            to: None,
        }
    }

    /// A closed `from .. to` window.
    #[must_use]
    pub const fn window(from: Duration, to: Duration) -> Self {
        Self { from, to: Some(to) }
    }
}

impl fmt::Display for Slice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", time::format(self.from), match self.to {
            Some(to) => time::format(to),
            None => String::new(),
        })
    }
}

impl std::str::FromStr for Slice {
    type Err = Error;

    /// Parse `from-to`, or `from-` for the source's end. Timecodes are
    /// `SS`, `MM:SS` or `HH:MM:SS` with an optional `.fff` fraction.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        let (from, to) = s.split_once('-').ok_or_else(|| {
            Error::Parse(format!("bad slice {s:?}: expected from-to"))
        })?;
        let from = time::parse(from).map_err(Error::Parse)?;
        let to = if to.trim().is_empty() {
            None
        } else {
            Some(time::parse(to).map_err(Error::Parse)?)
        };
        Ok(Self { from, to })
    }
}

impl From<(Duration, Duration)> for Slice {
    fn from((from, to): (Duration, Duration)) -> Self {
        Self::window(from, to)
    }
}

/// What a put placed, echoed like the CLI's reply.
#[derive(Debug, Clone, PartialEq)]
pub struct Put {
    /// The track the clip landed on.
    pub track: usize,
    /// The placed clip, as placed.
    pub clip: PlacedClip,
    /// Whether the placement joined a running graph now (`Live`) or waits
    /// for the next `apply` (`Pending`).
    pub landed: Landed,
}

/// One placed clip, echoed.
#[derive(Debug, Clone, PartialEq)]
pub struct PlacedClip {
    /// Stable id, never reused while the clip lives.
    pub id: u64,
    /// The cited source.
    pub uri: String,
    /// Start position on the owning track.
    pub at: Duration,
    /// In-point, measured into the source.
    pub from: Duration,
    /// Out-point, measured into the source.
    pub to: Duration,
    /// Gain in the mix, `0.0 ..= 1.0`.
    pub gain: f32,
    /// The fade envelope.
    pub fade: Fade,
}

/// Why a [`Bo::put`] was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overlap {
    /// The track the clip wanted.
    pub track: usize,
    /// Where it wanted to land.
    pub at: Duration,
    /// The id of the clip already sitting there.
    pub conflict: u64,
    /// The conflicting clip's span, start…
    pub conflict_at: Duration,
    /// …and end.
    pub conflict_end: Duration,
    /// Where the clip could go instead.
    pub next_free: Duration,
}

impl fmt::Display for Overlap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "put refused: track {} @ {} overlaps\nreason: clip #{} occupies [{:.3},{:.3}); \
             next free start is {:.3}s",
            self.track,
            time::format(self.at),
            self.conflict,
            self.conflict_at.as_secs_f64(),
            self.conflict_end.as_secs_f64(),
            self.next_free.as_secs_f64(),
        )
    }
}

impl std::error::Error for Overlap {}

/// Everything a session call can refuse, without panicking.
#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// Text that failed to parse (a slice, later a key or control source).
    Parse(String),
    /// An open-ended slice whose source could not be measured.
    Probe { uri: String, why: String },
    /// The placement collided with a resident clip.
    Overlap(Overlap),
    /// A backend refused the request (play, seek, apply).
    Backend(BackendError),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(msg) => f.write_str(msg),
            Self::Probe { uri, why } => write!(f, "cannot measure {uri}: {why}"),
            Self::Overlap(overlap) => write!(f, "{overlap}"),
            Self::Backend(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Backend(err) => Some(err),
            _ => None,
        }
    }
}

impl From<BackendError> for Error {
    fn from(err: BackendError) -> Self {
        Self::Backend(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn write_test_wav(path: &Path, seconds: f32) {
        let rate = 44_100u32;
        let n = (rate as f32 * seconds) as usize;
        let mut data = Vec::with_capacity(n * 2);
        for i in 0..n {
            let v = (0.5 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / rate as f32).sin()
                * 32767.0) as i16;
            data.extend_from_slice(&v.to_le_bytes());
        }
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&rate.to_le_bytes());
        wav.extend_from_slice(&(rate * 2).to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
        wav.extend_from_slice(&data);
        std::fs::write(path, wav).unwrap();
    }

    fn src(uri: &str) -> String {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(Path::new(uri).file_name().unwrap());
        write_test_wav(&path, 0.5);
        // Keep the dir alive: leak it, so the file outlives the test.
        std::mem::forget(dir);
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn put_places_a_windowed_clip_on_a_track() {
        let mut bo = Bo::new();
        let uri = src("a.wav");
        let put = bo
            .put(&uri, Slice::window(Duration::ZERO, Duration::from_secs_f64(0.2)), TrackRef(0).at(Duration::ZERO))
            .unwrap();
        assert_eq!(put.track, 0);
        assert_eq!(put.clip.id, 0);
        assert_eq!(bo.tracks().len(), 1);
        assert_eq!(bo.tracks()[0].clips().len(), 1);
        assert_eq!(bo.tracks()[0].clips()[0].duration(), Duration::from_secs_f64(0.2));
        // Stopped transport: the placement waits for a play, not a rebuild.
        assert_eq!(put.landed, Landed::Pending);
    }

    #[test]
    fn an_open_slice_is_probed_to_the_sources_end() {
        let mut bo = Bo::new();
        let uri = src("a.wav");
        let put = bo.put(&uri, Slice::whole(), TrackRef(0).at(Duration::ZERO)).unwrap();
        assert_eq!(put.clip.from, Duration::ZERO);
        assert!(!put.clip.to.is_zero(), "an open slice resolves to a finite end");
        assert_eq!(put.clip.to, Duration::from_secs_f64(0.5));
    }

    #[test]
    fn a_collision_refuses_and_leaves_no_trace() {
        let mut bo = Bo::new();
        let uri = src("a.wav");
        let zero = Duration::ZERO;
        bo.put(&uri, Slice::window(Duration::ZERO, Duration::from_secs_f64(0.2)), TrackRef(0).at(zero))
            .unwrap();
        let err = bo
            .put(&uri, Slice::window(Duration::ZERO, Duration::from_secs_f64(0.2)), TrackRef(0).at(zero))
            .unwrap_err();
        match err {
            Error::Overlap(overlap) => {
                assert_eq!(overlap.track, 0);
                assert_eq!(overlap.conflict, 0);
                assert!(!overlap.next_free.is_zero(), "the freed tail is reported");
            }
            other => panic!("expected an overlap, got {other:?}"),
        }
        assert_eq!(bo.tracks()[0].clips().len(), 1, "a refused put leaves no trace");
        assert_eq!(bo.duration(), Duration::from_secs_f64(0.2));
    }

    #[test]
    fn butt_joined_clips_share_a_track_in_order() {
        let mut bo = Bo::new();
        let uri = src("a.wav");
        let d = Duration::from_secs_f64(0.2);
        bo.put(&uri, Slice::window(Duration::ZERO, d), TrackRef(0).at(Duration::ZERO)).unwrap();
        let put = bo.put(&uri, Slice::window(Duration::ZERO, d), TrackRef(0).at(d)).unwrap();
        assert_eq!(put.clip.id, 1);
        assert_eq!(bo.tracks()[0].clips().len(), 2);
        assert_eq!(bo.tracks()[0].clips()[1].at, d);
        assert_eq!(bo.duration(), d + d);
    }

    #[test]
    fn tracks_grow_to_fit_and_the_playhead_is_the_default_position() {
        let mut bo = Bo::new();
        let uri = src("a.wav");
        let d = Duration::from_secs_f64(0.2);
        let put = bo.put(&uri, Slice::window(Duration::ZERO, d), TrackRef(4).at(Duration::ZERO)).unwrap();
        assert_eq!(put.track, 4);
        assert_eq!(bo.tracks().len(), 5, "missing tracks are created");
    }

    #[test]
    fn slice_text_parses_closed_open_and_bad() {
        assert_eq!(
            "1:00-2:00".parse::<Slice>().unwrap(),
            Slice::window(Duration::from_secs(60), Duration::from_secs(120))
        );
        assert_eq!("1:00-".parse::<Slice>().unwrap(), Slice {
            from: Duration::from_secs(60),
            to: None,
        });
        assert!("1:00".parse::<Slice>().is_err(), "needs a -");
        assert!("x-y".parse::<Slice>().is_err(), "bad timecode");
    }
}
