//! The client: [`Bo`], a typed handle on a [`connection::Connection`],
//! forwarding commands to the session's executor.
//!
//! Today the executor is the daemon on its Unix socket — the same
//! arrangement every `bo` invocation shares. Each method is one
//! [`Command`](bo_core::command::Command), sent as typed JSON and decoded
//! back from the typed [`Reply`](bo_core::command::Reply). The command
//! protocol speaks the model's own units; this module adds the client's own
//! vocabulary on top — a [`Clip`] (what to play, as material) and a
//! [`TrackPosition`] (where on the timeline) — that expand into those units.
//!
//! # Placing a clip
//!
//! [`Bo::put`] takes the material ([`Clip`]: a source and a `from..to`
//! window, or the whole source) and the placement ([`TrackPosition`]: a
//! track and a timecode). A clip with an open end (`to: None`) is measured
//! where the arrangement lives, because only decoding the source can say
//! where it ends; a closed window touches no disk at all until play or
//! render.
//!
//! ```no_run
//! use bo::client::{Bo, Clip, Timecode, TrackIndex};
//! use std::time::Duration;
//!
//! let mut bo = Bo::new();   // the default connection: the shared daemon
//! let clip = Clip::of("bed.wav").trim("1:00-2:00".parse()?);
//! let when: Timecode = "0:30".parse()?;
//! let put = bo.put(clip, TrackIndex(0).at(when))?;
//! assert_eq!(put.track, 0);
//! # Ok::<(), bo::client::Error>(())
//! ```

use std::fmt;
use std::time::Duration;

use crate::connection::Connection;

// The command protocol, shared with the engine and the daemon.
pub use bo_core::command::{
    Applied, Command, Error, Inserted, Landed, Moved, Outcome, Overlap, PlacedClip, Played, Removed, Rendered, Reply, Routed, Set, Snapshot, Stats, SNAPSHOT_VERSION,
};
pub use bo_core::bus::BusRef;
use bo_core::command::{ClipHere, OnTrack, RouteBus};

/// A timecode: a point in time, parsed from the lenient forms the whole
/// tool speaks — `SS`, `MM:SS` or `HH:MM:SS` with an optional `.fff`
/// fraction (`"3.2"`, `"1:00"`, `"00:01:30.500"`). DAW text for a moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Timecode(pub Duration);

impl Timecode {
    /// The moment, as a duration.
    #[must_use]
    pub const fn duration(self) -> Duration {
        self.0
    }
}

impl fmt::Display for Timecode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&bo_core::time::format(self.0))
    }
}

impl std::str::FromStr for Timecode {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        bo_core::time::parse(s).map(Timecode).map_err(Error::Parse)
    }
}

impl From<Duration> for Timecode {
    fn from(d: Duration) -> Self {
        Self(d)
    }
}

impl From<Timecode> for Duration {
    fn from(t: Timecode) -> Self {
        t.0
    }
}

/// A span of a source's time, parsed as `from-to` (the CLI's slice text):
/// `"1:00-2:00"` for a closed span, `"1:00-"` to the source's end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TimecodeRange {
    /// Where the span starts, measured into the source.
    pub from: Timecode,
    /// Where it ends; `None` = the source's end.
    pub to: Option<Timecode>,
}

impl TimecodeRange {
    /// A closed span.
    #[must_use]
    pub const fn closed(from: Timecode, to: Timecode) -> Self {
        Self { from, to: Some(to) }
    }

    /// From `from` to the source's end.
    #[must_use]
    pub const fn open(from: Timecode) -> Self {
        Self { from, to: None }
    }
}

impl fmt::Display for TimecodeRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.from, match self.to {
            Some(to) => to.to_string(),
            None => String::new(),
        })
    }
}

impl std::str::FromStr for TimecodeRange {
    type Err = Error;

    /// Parse `from-to`, or `from-` for the source's end.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        let (from, to) = s.split_once('-').ok_or_else(|| {
            Error::Parse(format!("bad timecode range {s:?}: expected from-to"))
        })?;
        let from: Timecode = from.trim().parse()?;
        let to = if to.trim().is_empty() {
            None
        } else {
            Some(to.trim().parse()?)
        };
        Ok(Self { from, to })
    }
}

impl From<(Duration, Duration)> for TimecodeRange {
    fn from((from, to): (Duration, Duration)) -> Self {
        Self::closed(from.into(), to.into())
    }
}

/// Material to place: a source and the `from..to` window of it to play.
/// `to: None` means the source's end — resolved by probing when the clip is
/// placed (the CLI's `uri,from-`).
///
/// The client's own clip, deliberately not the model's [`Clip`] — no id, no
/// shared [`Source`](bo_core::track::Source) arc, no controls yet: this is
/// *what you want to place*, expressed plainly, and it becomes a model clip
/// where it is placed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clip {
    /// The source address.
    pub uri: String,
    /// In-point, measured into the source.
    pub from: Duration,
    /// Out-point, measured into the source; `None` = the source's end.
    pub to: Option<Duration>,
}

impl Clip {
    /// The whole of `uri` (its end resolved by probing at placement).
    #[must_use]
    pub fn of(uri: impl Into<String>) -> Self {
        Self {
            uri: uri.into(),
            from: Duration::ZERO,
            to: None,
        }
    }

    /// Trim the material to a span of its source: `"1:00-2:00"`, or
    /// `"1:00-"` for the tail.
    #[must_use]
    pub fn trim(mut self, range: TimecodeRange) -> Self {
        self.from = range.from.duration();
        self.to = range.to.map(Timecode::duration);
        self
    }

    /// Trim the material to start at `from`, running to the source's end.
    #[must_use]
    pub fn trim_to_end(mut self, from: Timecode) -> Self {
        self.from = from.duration();
        self.to = None;
        self
    }

    /// Trim the material to the source's first `to`.
    #[must_use]
    pub fn trim_from_begin(mut self, to: Timecode) -> Self {
        self.from = Duration::ZERO;
        self.to = Some(to.duration());
        self
    }
}

/// A clip as it sits on a track — what a take or move addresses. It knows
/// its track: [`ClipOnTrack::id`] addresses by the stable (per-track) id an
/// insert returned, [`ClipOnTrack::at`] by the track time it covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipOnTrack {
    /// The clip with this stable id on `on`.
    Id { on: TrackIndex, id: u64 },
    /// The clip covering this time on `on`.
    At { on: TrackIndex, at: Timecode },
}

impl ClipOnTrack {
    /// The clip with this stable id on `on`.
    #[must_use]
    pub const fn id(on: TrackIndex, id: u64) -> Self {
        Self::Id { on, id }
    }

    /// The clip covering this time on `on`.
    #[must_use]
    pub fn at(on: TrackIndex, at: impl Into<Timecode>) -> Self {
        Self::At { on, at: at.into() }
    }
}

/// A track, addressed by its index. A `put` grows the session to fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TrackIndex(pub usize);

impl TrackIndex {
    /// A position on this track: `TrackIndex(0).at(t)` — the CLI's `0@t`.
    /// Takes a parsed [`Timecode`] (`"0:30".parse()?`) or any [`Duration`].
    #[must_use]
    pub fn at(self, at: impl Into<Timecode>) -> TrackPosition {
        TrackPosition {
            track: self.0,
            at: at.into().duration(),
        }
    }
}

impl From<usize> for TrackIndex {
    fn from(track: usize) -> Self {
        Self(track)
    }
}

impl From<TrackIndex> for usize {
    fn from(track: TrackIndex) -> Self {
        track.0
    }
}

impl fmt::Display for TrackIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Where on the timeline a clip lands: a track and a timecode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackPosition {
    /// Track index, created on demand by an insert.
    pub track: usize,
    /// Position on that track.
    pub at: Duration,
}

impl From<(usize, Duration)> for TrackPosition {
    fn from((track, at): (usize, Duration)) -> Self {
        Self { track, at }
    }
}

/// Where a clip lands: on an existing track at a timecode, or on a fresh
/// one — the CLI's bare `put` (or `put uri @pos`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Destination {
    /// An existing track at a timecode.
    Track(TrackPosition),
    /// A fresh track, appended ([`NewTrack`]).
    NewTrack(NewTrack),
}

impl From<TrackPosition> for Destination {
    fn from(position: TrackPosition) -> Self {
        Self::Track(position)
    }
}

impl From<NewTrack> for Destination {
    fn from(track: NewTrack) -> Self {
        Self::NewTrack(track)
    }
}

/// A fresh track for a clip ([`Destination::NewTrack`]). `default()` lands at the
/// playhead (the CLI's bare `put`); [`NewTrack::at`] places it at a time —
/// the CLI's `put uri @pos`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTrack {
    /// Where on the timeline the fresh track's clip lands; `None` = the
    /// playhead.
    pub at: Option<Timecode>,
}

impl NewTrack {
    /// A fresh track whose clip lands at `at`.
    #[must_use]
    pub fn at(at: impl Into<Timecode>) -> Self {
        Self { at: Some(at.into()) }
    }
}

impl Default for NewTrack {
    /// A fresh track; the clip lands at the playhead.
    fn default() -> Self {
        Self { at: None }
    }
}

/// A bus to route into: the master, an existing group bus, or a fresh one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BusIndex {
    /// The master bus — routing a track here sends it back out.
    Master,
    /// A group bus that already exists, by id.
    Group(u64),
    /// A fresh group bus ([`NewBus`]).
    New(NewBus),
}

impl BusIndex {
    /// The master bus.
    #[must_use]
    pub const fn master() -> Self {
        Self::Master
    }

    /// A group bus by id.
    #[must_use]
    pub const fn group(id: u64) -> Self {
        Self::Group(id)
    }
}

impl From<u64> for BusIndex {
    fn from(id: u64) -> Self {
        Self::Group(id)
    }
}

/// A fresh group bus to create and route into on its first mention, named
/// or not — the CLI's `route <track> <name>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewBus(Option<String>);

impl NewBus {
    /// A fresh group bus named `name` (unique, never `master`).
    #[must_use]
    pub fn with_name(name: impl Into<String>) -> Self {
        Self(Some(name.into()))
    }
}

impl Default for NewBus {
    /// A fresh, unnamed group bus.
    fn default() -> Self {
        Self(None)
    }
}

impl From<NewBus> for BusIndex {
    fn from(bus: NewBus) -> Self {
        Self::New(bus)
    }
}

/// How a render runs: an optional trimmed range, measurement, mono fold.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RenderConfig {
    /// Render only this span of the arrangement (`None` = the whole thing).
    pub trim: Option<TimecodeRange>,
    /// Measure the mix in the same pass; the numbers describe the exact
    /// sample stream the file carries.
    pub measure: bool,
    /// Fold the mix to mono, `(L+R)/2` — a broadcast or single-speaker
    /// delivery.
    pub mono: bool,
}

/// A typed client on a [`Connection`]: the arrangement lives there, commands
/// travel there, and the replies come back typed.
///
/// [`Bo::new`] is the default connection — the daemon on
/// `$TMPDIR/bo/daemon.sock`, spawned on demand — the same session the CLI
/// speaks to, so a program and a shell can work one arrangement. Any other
/// connection (another socket, later a process session) is
/// [`Bo::with_connection`].
#[derive(Debug)]
pub struct Bo {
    connection: Connection,
}

impl Default for Bo {
    fn default() -> Self {
        Self::new()
    }
}

impl Bo {
    /// A client on the default connection: the daemon on
    /// `$TMPDIR/bo/daemon.sock`, spawned on demand.
    #[must_use]
    pub fn new() -> Self {
        Self::with_connection(Connection::default())
    }

    /// A client on a connection of your own.
    #[must_use]
    pub fn with_connection(connection: Connection) -> Self {
        Self { connection }
    }

    /// Place a clip: the material `clip` lands at `to`.
    ///
    /// A refused put — a clip with no end whose source cannot be measured,
    /// or a placement that collides with a resident clip — is an `Err` and
    /// leaves the session exactly as it was. The track is grown to fit. A
    /// clip placed past the end of a running track's queue joins that queue
    /// as it is placed; anything else waits for an `apply` — see
    /// [`Inserted::landed`].
    pub fn put(&mut self, clip: Clip, to: impl Into<Destination>) -> Result<Inserted, Error> {
        let Clip { uri, from, to: to_in } = clip;
        let on = match to.into() {
            Destination::Track(position) => OnTrack::Track {
                index: position.track,
                at: position.at,
            },
            Destination::NewTrack(track) => OnTrack::New {
                at: track.at.map(Timecode::duration),
            },
        };
        match self.exec(Command::Insert { uri, from, to: to_in, on })? {
            Outcome::Inserted(inserted) => Ok(inserted),
            other => Err(unexpected(&other)),
        }
    }

    /// Take a clip off a track ([`ClipOnTrack::id`] or
    /// [`ClipOnTrack::at`]). Structure: it lands at the next `apply`.
    pub fn take(&mut self, clip: ClipOnTrack) -> Result<Removed, Error> {
        match self.exec(self.remove_command(clip))? {
            Outcome::Removed(removed) => Ok(removed),
            other => Err(unexpected(&other)),
        }
    }

    /// Move a clip to another track, or a new position on its own, keeping
    /// its content and (when the destination allows) its id. Atomic:
    /// refused whole if the destination is occupied. Structure: it lands at
    /// the next `apply`.
    pub fn r#move(
        &mut self,
        clip: ClipOnTrack,
        to: impl Into<Destination>,
    ) -> Result<Moved, Error> {
        let Command::Remove { track, clip } = self.remove_command(clip) else {
            unreachable!("remove_command always builds Remove")
        };
        let to = match to.into() {
            Destination::Track(position) => OnTrack::Track {
                index: position.track,
                at: position.at,
            },
            Destination::NewTrack(track) => OnTrack::New {
                at: track.at.map(Timecode::duration),
            },
        };
        match self.exec(Command::Move { track, clip, to })? {
            Outcome::Moved(moved) => Ok(moved),
            other => Err(unexpected(&other)),
        }
    }

    /// The [`Command::Remove`] a [`ClipOnTrack`] addresses.
    fn remove_command(&self, clip: ClipOnTrack) -> Command {
        match clip {
            ClipOnTrack::Id { on, id } => Command::Remove {
                track: on.0,
                clip: ClipHere::Id(id),
            },
            ClipOnTrack::At { on, at } => Command::Remove {
                track: on.0,
                clip: ClipHere::At(at.duration()),
            },
        }
    }

    /// Patch the state zone: a leaf scalar (`track.0.volume`), or a merge
    /// over a strip (`track.0` with `{"volume":…,"muted":…}`) — missing
    /// keys untouched, unknown keys refused. Structure is edited by the
    /// verbs, not here.
    pub fn set(&mut self, path: &str, patcher: serde_json::Value) -> Result<Set, Error> {
        match self.exec(Command::Set {
            path: path.to_string(),
            patcher,
        })? {
            Outcome::Set(set) => Ok(set),
            other => Err(unexpected(&other)),
        }
    }

    /// Mix the arrangement to a wav file at `output`, per `settings` —
    /// optionally only a trimmed range, measured, or folded to mono.
    pub fn render(
        &mut self,
        output: impl AsRef<std::path::Path>,
        settings: RenderConfig,
    ) -> Result<Rendered, Error> {
        let file = output.as_ref().to_string_lossy().into_owned();
        let (from, to) = match settings.trim {
            Some(trim) => (Some(trim.from.duration()), trim.to.map(Timecode::duration)),
            None => (None, None),
        };
        match self.exec(Command::Render {
            file,
            from,
            to,
            measure: settings.measure,
            mono: settings.mono,
        })? {
            Outcome::Rendered(rendered) => Ok(rendered),
            other => Err(unexpected(&other)),
        }
    }

    /// Save the session as a snapshot at `path`: the arrangement commands
    /// that built it (logged by the daemon) plus the playhead.
    pub fn save(&mut self, path: impl AsRef<std::path::Path>) -> Result<Snapshot, Error> {
        let snapshot = match self.exec(Command::Snapshot)? {
            Outcome::Snapshot(snapshot) => snapshot,
            other => return Err(unexpected(&other)),
        };
        let file = std::fs::File::create(path.as_ref())
            .map_err(|e| Error::Value(format!("cannot write {}: {e}", path.as_ref().display())))?;
        serde_json::to_writer(file, &snapshot).map_err(|e| Error::Value(e.to_string()))?;
        Ok(snapshot)
    }

    /// Read and validate a snapshot at `path` without touching the session:
    /// the daemon stages the history and reports every problem. Refuses
    /// versions this build does not read.
    pub fn check(&mut self, path: impl AsRef<std::path::Path>) -> Result<(), Error> {
        let text = std::fs::read_to_string(path.as_ref())
            .map_err(|e| Error::Value(format!("cannot read {}: {e}", path.as_ref().display())))?;
        let snapshot: Snapshot = serde_json::from_str(&text)
            .map_err(|e| Error::Value(format!("bad snapshot {}: {e}", path.as_ref().display())))?;
        if snapshot.version != SNAPSHOT_VERSION {
            return Err(Error::Version(format!(
                "snapshot version {} — this build reads {}",
                snapshot.version, SNAPSHOT_VERSION
            )));
        }
        match self.exec(Command::Check { snapshot })? {
            Outcome::Checked => Ok(()),
            other => Err(unexpected(&other)),
        }
    }

    /// Replace the session from a snapshot at `path`, atomically: the
    /// history is staged by the daemon, so a failing script leaves the
    /// session untouched. Refuses versions this build does not read.
    pub fn load(&mut self, path: impl AsRef<std::path::Path>) -> Result<(), Error> {
        let text = std::fs::read_to_string(path.as_ref())
            .map_err(|e| Error::Value(format!("cannot read {}: {e}", path.as_ref().display())))?;
        let snapshot: Snapshot = serde_json::from_str(&text)
            .map_err(|e| Error::Value(format!("bad snapshot {}: {e}", path.as_ref().display())))?;
        if snapshot.version != SNAPSHOT_VERSION {
            return Err(Error::Version(format!(
                "snapshot version {} — this build reads {}",
                snapshot.version, SNAPSHOT_VERSION
            )));
        }
        match self.exec(Command::Load { snapshot })? {
            Outcome::Loaded => Ok(()),
            other => Err(unexpected(&other)),
        }
    }

    /// Read the arrangement — the whole tree (`""`), a subtree, or a leaf,
    /// as JSON.
    pub fn get(&mut self, path: &str) -> Result<serde_json::Value, Error> {
        match self.exec(Command::Get {
            path: path.to_string(),
        })? {
            Outcome::Tree(value) => Ok(value),
            other => Err(unexpected(&other)),
        }
    }

    /// Start playback from the current playhead.
    pub fn play(&mut self) -> Result<Played, Error> {
        match self.exec(Command::Play)? {
            Outcome::Played(played) => Ok(played),
            other => Err(unexpected(&other)),
        }
    }

    /// Hold position and silence output; the position is where it stopped.
    pub fn pause(&mut self) -> Result<Duration, Error> {
        match self.exec(Command::Pause)? {
            Outcome::Paused { at } => Ok(at),
            other => Err(unexpected(&other)),
        }
    }

    /// Continue from where [`Bo::pause`] left off.
    pub fn resume(&mut self) -> Result<Duration, Error> {
        match self.exec(Command::Resume)? {
            Outcome::Resumed { at } => Ok(at),
            other => Err(unexpected(&other)),
        }
    }

    /// Drop every track and group bus: back to a fresh session.
    pub fn reset(&mut self) -> Result<(), Error> {
        match self.exec(Command::Reset)? {
            Outcome::Reset => Ok(()),
            other => Err(unexpected(&other)),
        }
    }

    /// Stop and rewind to zero.
    pub fn stop(&mut self) -> Result<(), Error> {
        match self.exec(Command::Stop)? {
            Outcome::Stopped => Ok(()),
            other => Err(unexpected(&other)),
        }
    }

    /// Jump the playhead.
    pub fn seek(&mut self, at: Duration) -> Result<(), Error> {
        match self.exec(Command::Seek { at })? {
            Outcome::Seeked { .. } => Ok(()),
            other => Err(unexpected(&other)),
        }
    }

    /// Make every pending edit audible.
    pub fn apply(&mut self) -> Result<Applied, Error> {
        match self.exec(Command::Apply)? {
            Outcome::Applied(applied) => Ok(applied),
            other => Err(unexpected(&other)),
        }
    }

    /// Route a track's output into a bus — the master, an existing group
    /// bus, or a fresh one ([`NewBus`], created and routed on its first
    /// mention). Structure: it lands at the next `apply`.
    pub fn route(
        &mut self,
        track: TrackIndex,
        to: impl Into<BusIndex>,
    ) -> Result<Routed, Error> {
        let bus = match to.into() {
            BusIndex::Master => RouteBus::Master,
            BusIndex::Group(id) => RouteBus::Group { id },
            BusIndex::New(bus) => RouteBus::New { name: bus.0 },
        };
        match self.exec(Command::Route {
            track: track.0,
            bus,
        })? {
            Outcome::Routed(routed) => Ok(routed),
            other => Err(unexpected(&other)),
        }
    }

    /// Send one [`Command`] to the session's executor and decode the typed
    /// reply. The daemon runs the very same command through the engine's
    /// `exec`; this is the client's half of that round trip.
    pub fn exec(&mut self, command: Command) -> Result<Outcome, Error> {
        let body = serde_json::to_value(&command).map_err(|e| Error::Daemon(e.to_string()))?;
        let reply = self.connection.request(&body).map_err(Error::Daemon)?;
        match serde_json::from_str::<Reply>(&reply)
            .map_err(|e| Error::Daemon(format!("bad daemon reply: {e}")))?
        {
            Reply::Ok(outcome) => Ok(outcome),
            Reply::Err(e) => Err(e),
        }
    }
}

/// The reply was not the one this call asked for.
fn unexpected(outcome: &Outcome) -> Error {
    Error::Daemon(format!("unexpected daemon reply: {outcome:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;


    #[test]
    fn timecode_text_parses_and_formats() {
        let t: Timecode = "1:00".parse().unwrap();
        assert_eq!(t.duration(), Duration::from_secs(60));
        assert_eq!(t.to_string(), "00:01:00.000");
        let r: TimecodeRange = "1:00-2:00".parse().unwrap();
        assert_eq!(r.from.duration(), Duration::from_secs(60));
        assert_eq!(r.to.map(Timecode::duration), Some(Duration::from_secs(120)));
        assert_eq!("1:00-".parse::<TimecodeRange>().unwrap().to, None);
        assert!("1:00".parse::<TimecodeRange>().is_err(), "needs a -");
        let c = Clip::of("a.wav").trim(r);
        assert_eq!(c.from, Duration::from_secs(60));
        assert_eq!(c.to, Some(Duration::from_secs(120)));
    }

    #[test]
    fn material_and_placement_expand_into_a_flat_command() {
        let clip = Clip::of("a.wav").trim(TimecodeRange::from((Duration::ZERO, Duration::from_secs(5))));
        let to = TrackPosition::from((3, Duration::from_secs(9)));
        let cmd = Command::Insert {
            uri: clip.uri,
            from: clip.from,
            to: clip.to,
            on: OnTrack::Track { index: to.track, at: to.at },
        };
        assert_eq!(cmd, Command::Insert {
            uri: "a.wav".to_string(),
            from: Duration::ZERO,
            to: Some(Duration::from_secs(5)),
            on: OnTrack::Track {
                index: 3,
                at: Duration::from_secs(9),
            },
        });
    }
}
