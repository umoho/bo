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
use serde_json::Value;

use crate::bus::BusRef;
use crate::time;
use crate::track::Fade;

/// The snapshot format this build reads and writes. Load refuses anything
/// else, so future formats can change shape without guessing.
pub const SNAPSHOT_VERSION: u32 = 1;

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

/// Address a clip on a track: by its stable id, or by the track time it
/// covers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipHere {
    /// The clip with this (per-track) stable id.
    Id(u64),
    /// The clip covering this track time.
    #[serde(with = "ms")]
    At(Duration),
}

/// One command: what to do. The engine executes it
/// ([`exec`](bo_engine::exec)); commands travel as JSON over the wire.
/// Fields speak the model's units; the engine needs nothing translated.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    /// Insert a clip into a track, like the model's `Track::insert`: the
    /// `from .. to` span of source `uri` is placed on `on`. `to: None`
    /// means the source's end, resolved by probing when the command runs.
    Insert {
        uri: String,
        #[serde(with = "ms")]
        from: Duration,
        #[serde(with = "ms_opt")]
        to: Option<Duration>,
        on: OnTrack,
    },
    /// Route a track's output into a bus — a group bus, or back to the
    /// master; a [`RouteBus::New`] creates the group on its first mention
    /// (the CLI's `route <track> <name>`). Structure: it lands at the next
    /// `apply`.
    Route { track: usize, bus: RouteBus },
    /// Take a clip off a track ([`ClipHere`]). Structure: it lands at the
    /// next `apply`.
    Remove {
        track: usize,
        clip: ClipHere,
    },
    /// Move a clip to another track, or a new position on its own,
    /// keeping its content (gain, fades) and its id when the destination
    /// allows. Atomic: refused whole if the destination is occupied.
    Move {
        track: usize,
        clip: ClipHere,
        to: OnTrack,
    },
    /// Read the arrangement — the whole tree (`""`), a subtree, or a leaf
    /// ([`Command`]'s state zone and structure, as JSON).
    Get { path: String },
    /// The session's snapshot: the history of arrangement commands that
    /// built it, and the playhead. Host-level (the daemon logs the history).
    Snapshot,
    /// Replace the arrangement from a snapshot, atomically: the history is
    /// staged first, so a failing script leaves the session untouched.
    Load { snapshot: Snapshot },
    /// Patch the arrangement's state zone: a leaf scalar, or a merge over a
    /// strip ([`Set`]). Structure is edited by the other verbs only.
    Set { path: String, patcher: Value },
    /// Mix the arrangement — or a range of it — to a wav file, optionally
    /// measuring the mix or folding it to mono.
    Render {
        file: String,
        #[serde(with = "ms_opt")]
        from: Option<Duration>,
        #[serde(with = "ms_opt")]
        to: Option<Duration>,
        measure: bool,
        mono: bool,
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

/// Which track a clip lands on: an existing one at a timecode, or a fresh
/// one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OnTrack {
    /// The track with this index (grown to fit if it does not exist), at
    /// this track time.
    Track {
        index: usize,
        #[serde(with = "ms")]
        at: Duration,
    },
    /// A fresh track, appended — at `at`, or the playhead when `None`.
    New {
        #[serde(with = "ms_opt")]
        at: Option<Duration>,
    },
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

/// Where a route sends a track: an existing bus, or a new group bus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "bus", rename_all = "snake_case")]
pub enum RouteBus {
    /// The master bus — route back out.
    Master,
    /// A group bus that already exists, by id.
    Group(u64),
    /// A fresh group bus, named by `name` (the CLI's first-mention create).
    New { name: Option<String> },
}

/// What a route did, echoed ([`Command::Route`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Routed {
    /// The routed track.
    pub track: usize,
    /// Where its output now points.
    pub bus: BusRef,
    /// Structure lands at the next `apply` ([`Landed::Pending`]).
    pub landed: Landed,
}

/// What a take removed, echoed ([`Command::Remove`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Removed {
    /// The track the clip was on.
    pub track: usize,
    /// The removed clip, as it was.
    pub clip: PlacedClip,
    /// Structure lands at the next `apply` ([`Landed::Pending`]).
    pub landed: Landed,
}

/// What a patch changed, echoed ([`Command::Set`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Set {
    /// The path that was patched.
    pub path: String,
    /// The canonical value now at that path.
    pub patched: Value,
    /// How the edit reached the sound.
    pub landed: Landed,
}

/// What a move did, echoed ([`Command::Move`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Moved {
    /// The track the clip left.
    pub from_track: usize,
    /// The track it landed on.
    pub to_track: usize,
    /// The moved clip, as moved (its actual id).
    pub clip: PlacedClip,
    /// Structure lands at the next `apply` ([`Landed::Pending`]).
    pub landed: Landed,
}

/// Levels of a rendered span, when measured ([`Command::Render`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stats {
    /// The span actually measured, milliseconds.
    pub span_ms: u64,
    /// Sample peak over all channels, dBFS.
    pub peak_db: f32,
    /// True peak (4× oversampled), dBFS.
    pub true_peak_db: f32,
    /// RMS over all samples, dBFS.
    pub rms_db: f32,
    /// Integrated loudness, LUFS; none under 3 s or for all-silence.
    pub integrated_lufs: Option<f32>,
    /// Loudest 400 ms block, LUFS.
    pub momentary_max_lufs: Option<f32>,
    /// Loudest 3 s window, LUFS.
    pub short_term_max_lufs: Option<f32>,
    /// Loudness range, LU.
    pub lra: Option<f32>,
}

/// What a render produced, echoed ([`Command::Render`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rendered {
    /// Where the wav went.
    pub file: String,
    /// How long the rendered range is.
    pub duration_ms: u64,
    /// Levels, when the render measured them.
    pub stats: Option<Stats>,
}

/// A session snapshot: the arrangement-building commands that made it, and
/// the playhead it had stopped at.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    /// The format version ([`SNAPSHOT_VERSION`]).
    pub version: u32,
    /// The arrangement commands, in the order they ran.
    pub history: Vec<Command>,
    /// The playhead, milliseconds.
    #[serde(with = "ms")]
    pub playhead: Duration,
}

/// The result of a [`Command`], as data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Outcome {
    /// A clip was inserted ([`Command::Insert`]).
    Inserted(Inserted),
    /// A clip was taken ([`Command::Remove`]).
    Removed(Removed),
    /// A clip was moved ([`Command::Move`]).
    Moved(Moved),
    /// The arrangement read ([`Command::Get`]).
    Tree(serde_json::Value),
    /// The arrangement was patched ([`Command::Set`]).
    Set(Set),
    /// A range was rendered ([`Command::Render`]).
    Rendered(Rendered),
    /// A snapshot read ([`Command::Snapshot`]).
    Snapshot(Snapshot),
    /// An arrangement was loaded ([`Command::Load`]).
    Loaded,
    /// A track was routed ([`Command::Route`]).
    Routed(Routed),
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
            "placement refused: track {} @ {} overlaps\nreason: clip #{} occupies [{:.3},{:.3}); \
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
    /// A command addressed a track that does not exist.
    NoTrack(usize),
    /// A command addressed a group bus that does not exist.
    NoBus(u64),
    /// A take or move could not find the clip it addressed.
    NoClip(String),
    /// A [`Command::Get`] path led nowhere.
    Path(String),
    /// A patch value was refused (type, range, an unknown key).
    Value(String),
    /// A render was refused.
    Render(String),
    /// A snapshot's version is not one this build reads.
    Version(String),
    /// A host-level command reached the engine (it is handled by the host).
    Host(String),
    /// A bus-name rule was refused ('master' reserved, a duplicate name).
    Bus(String),
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
            Self::NoTrack(track) => write!(f, "no track {track}"),
            Self::NoBus(id) => write!(f, "no bus {id}"),
            Self::NoClip(what) => write!(f, "no clip {what}"),
            Self::Path(path) => write!(f, "no such path {path:?}"),
            Self::Value(msg) => f.write_str(msg),
            Self::Render(msg) => f.write_str(msg),
            Self::Version(msg) => f.write_str(msg),
            Self::Host(msg) => f.write_str(msg),
            Self::Bus(msg) => f.write_str(msg),
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
            on: OnTrack::Track {
                index: 2,
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
