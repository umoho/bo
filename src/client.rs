//! The client: [`Bo`], a typed handle on a [`connection::Connection`], forwarding
//! commands to the session's executor.
//!
//! Today the executor is the daemon on its Unix socket — the same
//! arrangement every `bo` invocation shares. Each method is one
//! [`Command`](bo_core::command::Command), sent as typed JSON and decoded
//! back from the typed [`Reply`](bo_core::command::Reply); the command
//! protocol lives in `bo_core::command`, shared with the engine that
//! executes it.
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
//! let mut bo = Bo::new();   // the default session: the shared daemon
//! // The 1:00–2:00 window of the file, on track 0 at 30 s in:
//! let put = bo.put("bed.wav", "1:00-2:00".parse()?, TrackRef(0).at(Duration::from_secs(30)))?;
//! assert_eq!(put.track, 0);
//! # Ok::<(), bo::client::Error>(())
//! ```

use std::time::Duration;

use crate::connection::Connection;

// The command protocol, shared with the engine and the daemon; re-exported
// here so `bo::client::Slice` reads as before.
pub use bo_core::command::{
    Applied, Command, Error, Landed, Outcome, Overlap, PlacedClip, Played, Put, Reply, Slice,
    TrackPos, TrackRef,
};

/// The reply was not the one this call asked for.
fn unexpected(outcome: &Outcome) -> Error {
    Error::Daemon(format!("unexpected daemon reply: {outcome:?}"))
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

    /// Place a clip: the `from..to` window `slice` of source `uri`, on
    /// `on.track` at track-time `on.at`.
    ///
    /// A refused put — a slice with no end whose source cannot be measured,
    /// or a placement that collides with a resident clip — is an `Err` and
    /// leaves the session exactly as it was. The track is grown to fit. A
    /// clip placed past the end of a running track's queue joins that queue
    /// as it is placed; anything else waits for an `apply` — see
    /// [`Put::landed`].
    pub fn put(&mut self, uri: &str, slice: Slice, on: TrackPos) -> Result<Put, Error> {
        match self.exec(Command::Put {
            uri: uri.to_string(),
            slice,
            on,
        })? {
            Outcome::Put(put) => Ok(put),
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
