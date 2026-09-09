//! A connection: how a client reaches a session.
//!
//! The session itself — the arrangement, the transport, the engine running
//! commands — lives elsewhere (today: the daemon over its Unix socket). A
//! [`Connection`] is what a client holds to talk to one: the socket path,
//! and the on-demand spawn that brings the daemon up when it is not there.
//!
//! The wire is one JSON [`Command`](bo_core::command::Command) per line,
//! answered by one JSON [`Reply`](bo_core::command::Reply), framed like the
//! CLI's text protocol (`exit code` on the first line, the payload after).
//! Durations travel as whole milliseconds, so the round trip is exact.
//! Transport may change (Windows, a pipe, in-process) — never the commands
//! or their replies.
//!
//! ```
//! use bo::connection::Connection;
//!
//! let c = Connection::default();   // the daemon on $TMPDIR/bo/daemon.sock
//! let _ = Connection::at("/tmp/mine.sock");
//! # let _ = c;
//! ```

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command as ProcessCommand, Stdio};
use std::time::{Duration, Instant};
use std::{env, fs, thread};

/// A client handle to a session over its Unix socket: where it listens, and
/// how to reach it (spawning the daemon on demand when it is not there).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connection {
    socket: PathBuf,
}

impl Default for Connection {
    fn default() -> Self {
        Self::at(default_socket())
    }
}

impl Connection {
    /// The daemon on `$TMPDIR/bo/daemon.sock` — the session `bo` uses when
    /// no `--socket` says otherwise.
    #[must_use]
    pub fn default_socket() -> PathBuf {
        default_socket()
    }

    /// A connection to a daemon listening at `path`. The daemon is spawned
    /// on demand by the first request, and a stale socket is cleared first.
    #[must_use]
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self {
            socket: path.into(),
        }
    }

    /// The socket path this connection speaks over.
    #[must_use]
    pub fn socket(&self) -> &PathBuf {
        &self.socket
    }

    /// One round trip: send `request` (a JSON command), read the reply.
    ///
    /// The request is framed like the CLI's: the client's working directory
    /// first (so relative paths resolve against the caller, never the
    /// daemon's), then the command. The reply comes back as the payload
    /// after the exit-code line. Transport trouble — a daemon that cannot be
    /// spawned or reached — is the `Err`; what the daemon *said* is the
    /// `Ok` payload, judged by the caller.
    pub fn request(&self, request: &serde_json::Value) -> Result<String, String> {
        let mut stream = connect_or_spawn(&self.socket)?;
        let cwd = env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let body = serde_json::to_string(request).map_err(|e| e.to_string())?;
        let request = format!("{cwd}\n{body}\n");
        if let Err(e) = stream.write_all(request.as_bytes()) {
            return Err(format!("cannot reach the daemon: {e}"));
        }
        let mut reply = String::new();
        if let Err(e) = stream.read_to_string(&mut reply) {
            return Err(format!("cannot read the daemon's reply: {e}"));
        }
        // Reply framing: first line is the exit code, the rest the payload.
        match reply.split_once('\n') {
            Some((_, payload)) => Ok(payload.trim_end().to_string()),
            None => Err("daemon sent no reply".to_string()),
        }
    }
}

/// Where the daemon listens by default: `$TMPDIR/bo/daemon.sock`.
fn default_socket() -> PathBuf {
    env::temp_dir().join("bo").join("daemon.sock")
}

/// Connect to the daemon, spawning it (and clearing a stale socket) when it
/// is not there. The daemon binary is `$BO_DAEMON` when set — that is how
/// tests and embedders point a library connection at the real `bo` binary —
/// otherwise the current executable (the `bo` CLI itself).
fn connect_or_spawn(socket: &PathBuf) -> Result<UnixStream, String> {
    if let Ok(stream) = UnixStream::connect(socket) {
        return Ok(stream);
    }
    let _ = fs::remove_file(socket);
    if let Some(parent) = socket.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let exe = match env::var("BO_DAEMON") {
        Ok(path) => PathBuf::from(path),
        Err(_) => env::current_exe()
            .map_err(|e| format!("cannot find own binary: {e}"))?,
    };
    let mut child = ProcessCommand::new(&exe)
        .arg("daemon")
        .arg("--socket")
        .arg(socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0) // out of the terminal's foreground group
        .spawn()
        .map_err(|e| format!("cannot spawn the daemon: {e}"))?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Ok(stream) = UnixStream::connect(socket) {
            return Ok(stream);
        }
        if let Some(status) = child.try_wait().map_err(|e| format!("daemon wait failed: {e}"))? {
            return Err(format!("daemon exited immediately ({status})"));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            return Err("daemon did not come up in time".to_string());
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_defaults_to_the_shared_socket() {
        let c = Connection::default();
        assert_eq!(
            c.socket(),
            &env::temp_dir().join("bo").join("daemon.sock")
        );
    }

    #[test]
    fn connection_holds_its_socket() {
        let c = Connection::at("/tmp/bo-mine.sock");
        assert_eq!(c.socket(), &PathBuf::from("/tmp/bo-mine.sock"));
    }

    // A full round trip needs a real daemon and lives in tests/, where the
    // bo binary is reachable (BO_DAEMON): see session_socket.rs there.
    #[test]
    fn a_request_against_nowhere_is_a_connect_error() {
        // A socket path in a directory that cannot exist → spawn fails fast.
        let c = Connection::at("/nonexistent-bo-dir/x.sock");
        let err = c.request(&serde_json::json!({ "cmd": "put" })).unwrap_err();
        assert!(err.contains("cannot") || err.contains("spawn"), "{err}");
    }
}
