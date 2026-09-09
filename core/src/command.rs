//! The session command vocabulary — the data a command carries, shared by
//! the executors ([`bo_engine`]) and the client ([`bo`]).
//!
//! Pure data, no execution: [`Slice`] and [`TrackPos`] say *what* a put
//! places and *where*; [`Put`] is what it placed; [`Error`] is every way a
//! command can be refused. The engine runs commands against its player; the
//! client sends them and decodes the replies; the daemon translates between
//! the two over its wire. None of that lives here.

use std::fmt;
use std::time::Duration;

use crate::time;
use crate::track::Fade;

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

/// How an edit reached the sound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Landed {
    /// The running graph took it, without interrupting playback.
    Live,
    /// Nothing is running, or the edit needs a graph built from scratch: it
    /// is remembered, and lands at the next `apply`, `play` or `resume`.
    Pending,
}

/// A track, addressed by its index. A `put` grows the session to fit.
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
    /// Whether the placement joined a running graph now ([`Landed::Live`])
    /// or waits for the next `apply` ([`Landed::Pending`]).
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

/// Why a put was refused.
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

/// Everything a session command can refuse, without panicking.
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
    /// The session host (the daemon) could not be reached, or its reply
    /// could not be read.
    Daemon(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(msg) => f.write_str(msg),
            Self::Probe { uri, why } => write!(f, "cannot measure {uri}: {why}"),
            Self::Overlap(overlap) => write!(f, "{overlap}"),
            Self::Backend(err) => write!(f, "{err}"),
            Self::Daemon(msg) => f.write_str(msg),
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

    #[test]
    fn slice_text_parses_closed_open_and_bad() {
        assert_eq!(
            "1:00-2:00".parse::<Slice>().unwrap(),
            Slice::window(Duration::from_secs(60), Duration::from_secs(120))
        );
        assert_eq!(
            "1:00-".parse::<Slice>().unwrap(),
            Slice {
                from: Duration::from_secs(60),
                to: None,
            }
        );
        assert!("1:00".parse::<Slice>().is_err(), "needs a -");
        assert!("x-y".parse::<Slice>().is_err(), "bad timecode");
    }

    #[test]
    fn tracks_and_slices_convert() {
        let t: TrackRef = 3.into();
        assert_eq!(t.at(Duration::from_secs(9)), TrackPos {
            track: 3,
            at: Duration::from_secs(9),
        });
        assert_eq!(usize::from(t), 3);
        assert_eq!(TrackPos::from((1, Duration::ZERO)).track, 1);
        assert_eq!(Slice::from((Duration::ZERO, Duration::from_secs(5))).to, Some(Duration::from_secs(5)));
    }

    #[test]
    fn landed_is_copy_data() {
        let a = Landed::Live;
        let b = a; // Copy
        assert_eq!(a, b);
    }
}
