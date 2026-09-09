//! The session command protocol — data shared by the executors
//! ([`bo_engine`]) and the client ([`bo`]).
//!
//! Pure data, no execution. A [`Command`] says what to do; the engine runs
//! it ([`exec`](bo_engine::exec)); an [`Outcome`] or an [`Error`] says what
//! happened; a [`Reply`] carries either over the wire. Commands speak the
//! model's own units — uris, durations, track indices — never client-side
//! conveniences. The vocabulary here is what travels — serialized as JSON
//! between the client and the daemon, with durations as whole milliseconds —
//! and what both sides decode back into typed values.

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

/// One command: what to do. The engine executes it
/// ([`exec`](bo_engine::exec)); commands travel as JSON over the wire.
/// Fields speak the model's units; the engine needs nothing translated.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    /// Insert a clip into a track, like the model's `Track::insert`: the
    /// `from .. to` span of source `uri` is placed at track-time `at` on
    /// `track` (grown to fit). `to: None` means the source's end, resolved
    /// by probing when the command runs.
    Insert {
        uri: String,
        #[serde(with = "ms")]
        from: Duration,
        #[serde(with = "ms_opt")]
        to: Option<Duration>,
        #[serde(with = "ms")]
        at: Duration,
        track: usize,
    },
    /// Start playback from the current playhead.
    Play,
    /// Hold position and silence output.
    Pause,
    /// Continue from where `pause` left off.
    Resume,
    /// Jump the playhead; a running transport is re-planned from there.
    Seek {
        #[serde(with = "ms")]
        at: Duration,
    },
    /// Stop and rewind to zero.
    Stop,
    /// Make every pending edit audible.
    Apply,
}

/// What playback is about to play ([`Command::Play`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Played {
    /// Number of tracks with clips.
    pub tracks: usize,
    /// Total clips.
    pub clips: usize,
    /// When the arrangement ends.
    #[serde(with = "ms")]
    pub end: Duration,
    /// Where playback started from.
    #[serde(with = "ms")]
    pub playhead: Duration,
}

/// What [`Command::Apply`] did. Data, shared with the engine's transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Applied {
    /// Nothing was waiting.
    Nothing,
    /// `n` edits landed on the running graph; it was not rebuilt.
    Live(usize),
    /// `live` edits landed and the rest needed a graph rebuilt from `at`.
    Rebuilt { live: usize, #[serde(with = "ms")] at: Duration },
    /// The transport is stopped: there is no graph, so every edit stays
    /// pending until it plays.
    NotPlaying,
}

/// The result of a [`Command`], as data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Outcome {
    /// A clip was inserted ([`Command::Insert`]).
    Inserted(Inserted),
    /// Playback started ([`Command::Play`]).
    Played(Played),
    /// The transport paused ([`Command::Pause`]).
    Paused {
        #[serde(with = "ms")]
        at: Duration,
    },
    /// The transport resumed ([`Command::Resume`]).
    Resumed {
        #[serde(with = "ms")]
        at: Duration,
    },
    /// The transport stopped and rewound ([`Command::Stop`]).
    Stopped,
    /// The playhead moved ([`Command::Seek`]).
    Seeked {
        #[serde(with = "ms")]
        at: Duration,
    },
    /// Pending edits were made audible ([`Command::Apply`]).
    Applied(Applied),
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

/// What an insert placed, echoed like the CLI's reply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Inserted {
    /// The track the clip landed on.
    pub track: usize,
    /// The placed clip, as placed.
    pub clip: PlacedClip,
    /// Whether the placement joined a running graph now ([`Landed::Live`])
    /// or waits for the next `apply` ([`Landed::Pending`]).
    pub landed: Landed,
}

/// One placed clip, echoed. Gain and fades always carry their defaults on
/// the wire today (a plain insert places full-gain, straight-faded); they
/// are echoed the moment a command can place anything else.
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

/// Why an insert was refused.
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
            "insert refused: track {} @ {} overlaps\nreason: clip #{} occupies [{:.3},{:.3}); \
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
    /// Text that failed to parse (a key, a control source).
    Parse(String),
    /// An open-ended insert whose source could not be measured.
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
    fn landed_is_copy_data() {
        let a = Landed::Live;
        let b = a; // Copy
        assert_eq!(a, b);
    }

    #[test]
    fn commands_and_replies_round_trip_as_json() {
        let cmd = Command::Insert {
            uri: "bed.wav".to_string(),
            from: Duration::from_secs(60),
            to: Some(Duration::from_secs(120)),
            at: Duration::from_millis(30_000),
            track: 2,
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
