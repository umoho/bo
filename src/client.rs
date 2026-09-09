//! The client: [`Bo`], a typed handle on a [`connection::Connection`],
//! forwarding commands to the session's executor.
//!
//! Today the executor is the daemon on its Unix socket — the same
//! arrangement every `bo` invocation shares. Each method is one
//! [`Command`](bo_core::command::Command), sent as typed JSON and decoded
//! back from the typed [`Reply`](bo_core::command::Reply). The command
//! protocol speaks the model's own units; this module adds the ergonomics a
//! human or script types — [`Slice`], [`TrackRef`]`(i).at(t)` — that expand
//! into those units.
//!
//! # Placing a clip
//!
//! [`Bo::put`] takes three plain things: a source address (a file the caller
//! manages — bo does not open or probe it unless it must), a [`Slice`] (the
//! `from..to` window into that source, or the whole source), and a
//! [`TrackPos`] (a track and a timecode). A clip with an open end (a slice
//! whose `to` is `None`) is measured where the arrangement lives, because
//! only decoding the source can say where it ends; a closed slice touches no
//! disk at all until play or render.
//!
//! ```no_run
//! use bo::client::{Bo, Slice, TrackRef};
//! use std::time::Duration;
//!
//! let mut bo = Bo::new();   // the default connection: the shared daemon
//! // The 1:00–2:00 window of the file, on track 0 at 30 s in:
//! let put = bo.put("bed.wav", "1:00-2:00".parse()?, TrackRef(0).at(Duration::from_secs(30)))?;
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

/// A `from..to` window into a source: where a clip starts reading and where
/// it stops. `to: None` means the source's end — resolved by probing when
/// the clip is placed (the CLI's `uri,from-`). Client ergonomics: commands
/// carry the window as plain `from`/`to` fields.
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
        write!(f, "{}-{}", bo_core::time::format(self.from), match self.to {
            Some(to) => bo_core::time::format(to),
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
        let from = bo_core::time::parse(from).map_err(Error::Parse)?;
        let to = if to.trim().is_empty() {
            None
        } else {
            Some(bo_core::time::parse(to).map_err(Error::Parse)?)
        };
        Ok(Self { from, to })
    }
}

impl From<(Duration, Duration)> for Slice {
    fn from((from, to): (Duration, Duration)) -> Self {
        Self::window(from, to)
    }
}

/// A track, addressed by its index. An insert grows the session to fit.
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
    /// Track index, created on demand by an insert.
    pub track: usize,
    /// Position on that track.
    pub at: Duration,
}

impl From<(usize, Duration)> for TrackPos {
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

    /// Insert a clip: the `from..to` window `slice` of source `uri`, on
    /// `on.track` at track-time `on.at`.
    ///
    /// A refused insert — a slice with no end whose source cannot be
    /// measured, or a placement that collides with a resident clip — is an
    /// `Err` and leaves the session exactly as it was. The track is grown to
    /// fit. A clip placed past the end of a running track's queue joins that
    /// queue as it is placed; anything else waits for an `apply` — see
    /// [`Inserted::landed`].
    pub fn put(&mut self, uri: &str, slice: Slice, on: TrackPos) -> Result<Inserted, Error> {
        match self.exec(Command::Insert {
            uri: uri.to_string(),
            from: slice.from,
            to: slice.to,
            at: on.at,
            track: on.track,
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
    fn sugar_expands_into_a_flat_command() {
        let on = TrackRef(3).at(Duration::from_secs(9));
        assert_eq!(
            TrackPos::from((3, Duration::from_secs(9))),
            on
        );
        let s = Slice::whole();
        let cmd = Command::Insert {
            uri: "a.wav".to_string(),
            from: s.from,
            to: s.to,
            at: on.at,
            track: on.track,
        };
        assert_eq!(cmd, Command::Insert {
            uri: "a.wav".to_string(),
            from: Duration::ZERO,
            to: None,
            at: Duration::from_secs(9),
            track: 3,
        });
    }
}
