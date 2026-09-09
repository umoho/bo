//! The session command protocol — data shared by the executors
//! ([`bo_engine`]) and the client ([`bo`]).
//!
//! Pure data, no execution. A [`Command`] says what to do (place a clip…);
//! the engine runs it ([`exec`](bo_engine::exec)); an [`Outcome`] or an
//! [`Error`] says what happened; a [`Reply`] carries either over the wire.
//! The vocabulary here is what travels — serialized as JSON between the
//! client and the daemon, with durations as whole milliseconds — and what
//! both sides decode back into typed values.

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::time;
use crate::track::Fade;

/// A duration as whole milliseconds, serde's exact unit on the wire.
pub mod ms {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(d.as_secs() * 1000 + u64::from(d.subsec_millis()))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        Ok(Duration::from_millis(u64::deserialize(d)?))
    }
}

/// A duration or nothing, serde's exact unit on the wire.
pub mod ms_opt {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(
        d: &Option<Duration>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        match d {
            Some(d) => s.serialize_some(&(d.as_secs() * 1000 + u64::from(d.subsec_millis()))),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
        Option::<u64>::deserialize(d).map(|ms| ms.map(Duration::from_millis))
    }
}

/// Why a backend could not do what it was told.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackPos {
    /// Track index, created on demand by a put.
    pub track: usize,
    /// Position on that track.
    #[serde(with = "ms")]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Slice {
    /// In-point, measured into the source.
    #[serde(with = "ms")]
    pub from: Duration,
    /// Out-point, measured into the source; `None` = the source's end.
    #[serde(with = "ms_opt")]
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

/// One command: what to do. The engine executes it
/// ([`exec`](bo_engine::exec)); commands travel as JSON over the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    /// Place a clip: the `from..to` window `slice` of source `uri`, on
    /// `on.track` at track-time `on.at`. The track is grown to fit.
    Put {
        uri: String,
        slice: Slice,
        on: TrackPos,
    },
}

/// The result of a [`Command`], as data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Outcome {
    /// A clip was placed ([`Command::Put`]).
    Put(Put),
}

/// A command's reply over the wire: the [`Outcome`] or the [`Error`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reply {
    /// The command ran.
    Ok(Outcome),
    /// The command was refused.
    Err(Error),
}

/// What a put placed, echoed like the CLI's reply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Put {
    /// The track the clip landed on.
    pub track: usize,
    /// The placed clip, as placed.
    pub clip: PlacedClip,
    /// Whether the placement joined a running graph now ([`Landed::Live`])
    /// or waits for the next `apply` ([`Landed::Pending`]).
    pub landed: Landed,
}

/// One placed clip, echoed. Gain and fades always carry their defaults on
/// the wire today (a plain put places full-gain, straight-faded); they are
/// echoed the moment a command can place anything else.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlacedClip {
    /// Stable id, never reused while the clip lives.
    pub id: u64,
    /// The cited source.
    pub uri: String,
    /// Start position on the owning track.
    #[serde(with = "ms")]
    pub at: Duration,
    /// In-point, measured into the source.
    #[serde(with = "ms")]
    pub from: Duration,
    /// Out-point, measured into the source.
    #[serde(with = "ms")]
    pub to: Duration,
    /// Gain in the mix, `0.0 ..= 1.0`; full gain by default.
    #[serde(skip, default = "full_gain")]
    pub gain: f32,
    /// The fade envelope; no fade by default.
    #[serde(skip)]
    pub fade: Fade,
}

/// A placed clip's default gain.
fn full_gain() -> f32 {
    1.0
}

/// Why a put was refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Overlap {
    /// The track the clip wanted.
    pub track: usize,
    /// Where it wanted to land.
    #[serde(with = "ms")]
    pub at: Duration,
    /// The id of the clip already sitting there.
    pub conflict: u64,
    /// The conflicting clip's span, start…
    #[serde(with = "ms")]
    pub conflict_at: Duration,
    /// …and end.
    #[serde(with = "ms")]
    pub conflict_end: Duration,
    /// Where the clip could go instead.
    #[serde(with = "ms")]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
        assert_eq!(
            Slice::from((Duration::ZERO, Duration::from_secs(5))).to,
            Some(Duration::from_secs(5))
        );
    }

    #[test]
    fn landed_is_copy_data() {
        let a = Landed::Live;
        let b = a; // Copy
        assert_eq!(a, b);
    }

    #[test]
    fn commands_and_replies_round_trip_as_json() {
        let cmd = Command::Put {
            uri: "bed.wav".to_string(),
            slice: Slice::window(Duration::from_secs(60), Duration::from_secs(120)),
            on: TrackPos {
                track: 2,
                at: Duration::from_millis(30_000),
            },
        };
        let text = serde_json::to_string(&cmd).unwrap();
        assert_eq!(
            serde_json::from_str::<Command>(&text).unwrap(),
            cmd,
            "the command survives the wire"
        );

        let reply = Reply::Err(Error::Overlap(Overlap {
            track: 0,
            at: Duration::ZERO,
            conflict: 1,
            conflict_at: Duration::from_secs(5),
            conflict_end: Duration::from_secs(10),
            next_free: Duration::from_secs(10),
        }));
        let text = serde_json::to_string(&reply).unwrap();
        assert_eq!(
            serde_json::from_str::<Reply>(&text).unwrap(),
            reply,
            "typed errors survive the wire"
        );
    }
}
