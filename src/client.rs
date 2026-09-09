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
//! use bo::client::{Bo, Clip, TrackRef};
//! use std::time::Duration;
//!
//! let mut bo = Bo::new();   // the default connection: the shared daemon
//! let clip = Clip::of("bed.wav").trim(Duration::from_secs(60), Duration::from_secs(120));
//! let put = bo.put(clip, TrackRef(0).at(Duration::from_secs(30)))?;
//! assert_eq!(put.track, 0);
//! # Ok::<(), bo::client::Error>(())
//! ```

use std::fmt;
use std::time::Duration;

use crate::connection::Connection;

// The command protocol, shared with the engine and the daemon.
pub use bo_core::command::{
    Applied, Command, Error, Inserted, Landed, Outcome, Overlap, PlacedClip, Played, Reply,
};

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

    /// Trim the material to the `from .. to` span of its source.
    #[must_use]
    pub const fn trim(mut self, from: Duration, to: Duration) -> Self {
        self.from = from;
        self.to = Some(to);
        self
    }

    /// Trim the material to start at `from`, running to the source's end.
    #[must_use]
    pub const fn trim_to_end(mut self, from: Duration) -> Self {
        self.from = from;
        self.to = None;
        self
    }

    /// Trim the material to the source's first `to`.
    #[must_use]
    pub const fn trim_from_begin(mut self, to: Duration) -> Self {
        self.from = Duration::ZERO;
        self.to = Some(to);
        self
    }
}

/// A track, addressed by its index. A `put` grows the session to fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TrackRef(pub usize);

impl TrackRef {
    /// A position on this track: `TrackRef(0).at(t)` — the CLI's `0@t`.
    #[must_use]
    pub const fn at(self, at: Duration) -> TrackPosition {
        TrackPosition {
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
    pub fn put(&mut self, clip: Clip, to: TrackPosition) -> Result<Inserted, Error> {
        let Clip { uri, from, to: to_in } = clip;
        match self.exec(Command::Insert {
            uri,
            from,
            to: to_in,
            at: to.at,
            track: to.track,
        })? {
            Outcome::Inserted(inserted) => Ok(inserted),
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
    fn material_and_placement_expand_into_a_flat_command() {
        let clip = Clip::of("a.wav").trim(Duration::ZERO, Duration::from_secs(5));
        let to = TrackPosition::from((3, Duration::from_secs(9)));
        let cmd = Command::Insert {
            uri: clip.uri,
            from: clip.from,
            to: clip.to,
            at: to.at,
            track: to.track,
        };
        assert_eq!(cmd, Command::Insert {
            uri: "a.wav".to_string(),
            from: Duration::ZERO,
            to: Some(Duration::from_secs(5)),
            at: Duration::from_secs(9),
            track: 3,
        });
    }
}
