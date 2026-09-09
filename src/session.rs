//! A session: where an arrangement lives, and how a client reaches it.
//!
//! Today there is one kind of session: the daemon, reached over its Unix
//! socket. The daemon owns the arrangement and the transport (like the CLI's
//! session daemon: spawned on demand, self-cleaning, one arrangement that
//! every [`Bo`](crate::client::Bo) — or `bo` invocation — shares). Windows
//! is a later concern; the transport is the only thing that would change,
//! never the commands or their replies.
//!
//! The wire is one JSON object per command on a line, answered by one JSON
//! object, framed like the CLI's text protocol (`exit code` on the first
//! line, the payload after). Durations travel as whole milliseconds, so the
//! round trip is exact.
//!
//! ```
//! use bo::session::Session;
//!
//! let s = Session::default();   // the daemon on $TMPDIR/bo/daemon.sock
//! let _ = Session::at("/tmp/mine.sock");
//! # let _ = s;
//! ```

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command as ProcessCommand, Stdio};
use std::time::{Duration, Instant};
use std::{env, fs, thread};

/// A client handle to a daemon session: the Unix socket it listens on, and
/// how to reach it (spawning the daemon on demand when it is not there).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    socket: PathBuf,
}

impl Default for Session {
    fn default() -> Self {
        Self::at(default_socket())
    }
}

impl Session {
    /// The daemon on `$TMPDIR/bo/daemon.sock` — the session `bo` uses when
    /// no `--socket` says otherwise.
    #[must_use]
    pub fn default_socket() -> PathBuf {
        default_socket()
    }

    /// A session on a daemon listening at `path`. The daemon is spawned on
    /// demand by the first request, and a stale socket is cleared first.
    #[must_use]
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self {
            socket: path.into(),
        }
    }

    /// The socket path this session speaks over.
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
/// tests and embedders point a library session at the real `bo` binary —
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
    fn session_defaults_to_the_shared_socket() {
        let s = Session::default();
        assert_eq!(
            s.socket(),
            &env::temp_dir().join("bo").join("daemon.sock")
        );
    }

    #[test]
    fn session_holds_its_socket() {
        let s = Session::at("/tmp/bo-mine.sock");
        assert_eq!(s.socket(), &PathBuf::from("/tmp/bo-mine.sock"));
    }

    // A full round trip needs a real daemon and lives in tests/, where the
    // bo binary is reachable (BO_DAEMON): see session_socket.rs there.
    #[test]
    fn a_request_against_nowhere_is_a_connect_error() {
        // A socket path in a directory that cannot exist → spawn fails fast.
        let s = Session::at("/nonexistent-bo-dir/x.sock");
        let err = s.request(&serde_json::json!({ "cmd": "put" })).unwrap_err();
        assert!(err.contains("cannot") || err.contains("spawn"), "{err}");
    }
}
