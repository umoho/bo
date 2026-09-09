//! The session daemon: a host over a Unix socket, speaking only the typed
//! JSON wire — one [`Command`] per request (after a line naming the caller's
//! working directory), one JSON [`Reply`] back.
//!
//! The daemon hosts an engine [`Session`] (the only engine item it touches),
//! plus the command history snapshots are made from, and the transport
//! clock. The host-level snapshot verbs — `Snapshot`/`Load`/`Check` — are
//! intercepted here (or answered by `Session::load`), never sent to the
//! engine's raw `exec`, which refuses them with `Error::Host`.
//!
//! It is spawned on demand by the clients and the mini CLI, exits when
//! playback finishes, on a `stop`, or after `BO_IDLE_TIMEOUT` seconds of
//! silence while not playing, and removes its socket on the way out.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use bo::client::{Command, Error, Reply, SNAPSHOT_VERSION, Snapshot};
use bo::engine::Session;

/// What a daemon hosts: an engine session, the idle timeout it was started
/// with, and the arrangement commands that built it — the snapshot history.
struct Arrangement {
    session: Session,
    /// `BO_IDLE_TIMEOUT` seconds (default 600, `0` disables): a quiet,
    /// non-playing daemon exits after this long without a command.
    idle_timeout: u64,
    history: Vec<Command>,
}

impl Default for Arrangement {
    fn default() -> Self {
        Self::with_session(Session::silent())
    }
}

impl Arrangement {
    fn with_session(session: Session) -> Self {
        Self {
            session,
            idle_timeout: 600,
            history: Vec::new(),
        }
    }
}

/// Run the daemon on the session the environment asks for: rodio unless
/// `BO_BACKEND=silent`, falling back to silence without an audio device.
pub fn main(socket: &Path) -> i32 {
    daemon_main_with(socket, Session::open())
}

/// The idle timeout from `BO_IDLE_TIMEOUT` seconds (default 600; 0 disables).
fn idle_timeout() -> Duration {
    let default = Duration::from_secs(600);
    match std::env::var("BO_IDLE_TIMEOUT") {
        Ok(v) => v.parse().map(Duration::from_secs).unwrap_or(default),
        Err(_) => default,
    }
}

/// The daemon over a specific session.
fn daemon_main_with(socket: &Path, session: Session) -> i32 {
    if let Some(parent) = socket.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        eprintln!("bo: cannot create {}: {e}", parent.display());
        return 1;
    }
    let listener = match UnixListener::bind(socket) {
        Ok(listener) => listener,
        Err(e) => {
            // A live daemon already owns the socket; the client will find it.
            eprintln!("bo: daemon cannot bind {}: {e}", socket.display());
            return 1;
        }
    };
    let state = Arc::new(Mutex::new(Arrangement::with_session(session)));
    let exit = Arc::new(AtomicBool::new(false));
    // Every served command resets this clock; a non-playing daemon that stays
    // quiet for `BO_IDLE_TIMEOUT` seconds cleans itself up.
    let idle = Arc::new(Mutex::new(Instant::now()));
    let timeout = idle_timeout();
    state.lock().unwrap().idle_timeout = timeout.as_secs();

    let serve_state = state.clone();
    let serve_exit = exit.clone();
    let serve_idle = idle.clone();
    let serve = thread::spawn(move || serve_loop(listener, serve_state, serve_exit, serve_idle));
    let _ = serve;

    // Clock loop: advance the playhead by real elapsed time; end the session
    // on completion, `stop`, or an idle timeout while not playing.
    let mut last = Instant::now();
    loop {
        if exit.load(Ordering::Relaxed) {
            break;
        }
        let now = Instant::now();
        let dt = now - last;
        last = now;
        let quit = {
            let mut a = state.lock().unwrap();
            a.session.advance(dt);
            if a.session.is_playing() {
                a.session.is_finished()
            } else if timeout > Duration::ZERO
                && now.duration_since(*idle.lock().unwrap()) > timeout
            {
                // Quiet and not playing: the session looks abandoned.
                true
            } else {
                false
            }
        };
        if quit {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    let _ = std::fs::remove_file(socket);
    0
}

/// Accept connections and answer each command on its own thread. The clock
/// loop owns the transport; handlers only lock it briefly.
fn serve_loop(
    listener: UnixListener,
    state: Arc<Mutex<Arrangement>>,
    exit: Arc<AtomicBool>,
    idle: Arc<Mutex<Instant>>,
) {
    for connection in listener.incoming() {
        let Ok(mut stream) = connection else { continue };
        let state = state.clone();
        let exit = exit.clone();
        let idle = idle.clone();
        thread::spawn(move || {
            let mut reader = BufReader::new(&mut stream);
            let mut cwd = String::new();
            let Ok(_) = reader.read_line(&mut cwd) else { return };
            let mut line = String::new();
            let Ok(_) = reader.read_line(&mut line) else { return };
            let cwd = cwd.trim_end().to_string();
            let (code, output, ends_session) = handle_command(&state, line.trim_end(), &cwd);
            *idle.lock().unwrap() = Instant::now();
            let _ = stream.write_all(format!("{code}\n{output}").as_bytes());
            // The reply is on the wire before the session may end, so the
            // client never sees a truncated response.
            if ends_session {
                exit.store(true, Ordering::Relaxed);
            }
        });
    }
}

/// One JSON command: run it and frame the typed reply. The exit code stays
/// `0` — success and refusal both live in the reply's `ok` field. The third
/// value says whether this command ends the daemon's session (`stop`).
fn handle_command(state: &Mutex<Arrangement>, line: &str, cwd: &str) -> (i32, String, bool) {
    // Relative paths resolve against the caller's cwd, never the daemon's.
    let mut command = match serde_json::from_str::<Command>(line) {
        Ok(command) => command,
        Err(e) => {
            return (
                0,
                json_reply(&Reply::Err(Error::Parse(e.to_string()))),
                false,
            )
        }
    };
    let mut a = state.lock().unwrap();
    use Command as Cmd;
    let ends_session = matches!(command, Cmd::Stop);
    let reply = match &command {
        // Host-level: the daemon holds the history; the session holds the
        // transport.
        Cmd::Snapshot => {
            let snapshot = Snapshot {
                version: SNAPSHOT_VERSION,
                history: a.history.clone(),
                playhead: a.session.playhead(),
            };
            Reply::Ok(bo::client::Outcome::Snapshot(snapshot))
        }
        Cmd::Load { snapshot } => match a.session.load(snapshot) {
            Ok(()) => {
                a.history = snapshot.history.clone();
                Reply::Ok(bo::client::Outcome::Loaded)
            }
            Err(e) => Reply::Err(e),
        },
        Cmd::Check { snapshot } => {
            if snapshot.version != SNAPSHOT_VERSION {
                return (
                    0,
                    json_reply(&Reply::Err(Error::Version(format!(
                        "snapshot version {} — this build reads {}",
                        snapshot.version, SNAPSHOT_VERSION
                    )))),
                    false,
                );
            }
            let mut staged = Session::silent();
            let mut problems = Vec::new();
            for (i, command) in snapshot.history.iter().enumerate() {
                if let Err(e) = staged.exec(command.clone()) {
                    problems.push(format!("#{} {}", i + 1, e));
                }
            }
            let reply = if problems.is_empty() {
                Reply::Ok(bo::client::Outcome::Checked)
            } else {
                Reply::Err(Error::Check(problems.join("\n")))
            };
            return (0, json_reply(&reply), false);
        }
        // A fresh session forgets its history too.
        Cmd::Reset => match a.session.exec(command) {
            Ok(outcome) => {
                a.history.clear();
                Reply::Ok(outcome)
            }
            Err(e) => Reply::Err(e),
        },
        _ => {
            match &mut command {
                Cmd::Insert { uri, .. } => *uri = absolutize(uri, cwd),
                Cmd::Render { file, .. } => *file = absolutize(file, cwd),
                _ => {}
            }
            let executed = command.clone();
            match a.session.exec(command) {
                Ok(outcome) => {
                    if is_mutator(&executed) {
                        a.history.push(executed);
                    }
                    Reply::Ok(outcome)
                }
                Err(e) => Reply::Err(e),
            }
        }
    };
    (0, json_reply(&reply), ends_session)
}

/// The arrangement commands a snapshot records: everything that edits the
/// session. Transport, queries, renders and snapshots do not count.
fn is_mutator(command: &Command) -> bool {
    use Command as Cmd;
    matches!(
        command,
        Cmd::Insert { .. }
            | Cmd::Remove { .. }
            | Cmd::Move { .. }
            | Cmd::Route { .. }
            | Cmd::Set { .. }
    )
}

/// Resolve a path against the caller's cwd: absolute paths and scheme-bearing
/// URIs (`http://…`) are left alone. Every relative local path in a command
/// resolves against the cwd of the invocation that issued it, never the
/// daemon's.
fn absolutize(path: &str, cwd: &str) -> String {
    let p = Path::new(path);
    if p.is_absolute() || path.contains("://") || cwd.is_empty() {
        return path.to_string();
    }
    Path::new(cwd).join(p).to_string_lossy().into_owned()
}

/// Serialize a typed reply for the wire.
fn json_reply(reply: &Reply) -> String {
    serde_json::to_string(reply).unwrap_or_else(|e| {
        serde_json::to_string(&Reply::Err(Error::Daemon(e.to_string()))).unwrap_or_default()
    })
}

/// The default socket, for a `bo daemon` with no explicit path.
pub(crate) fn default_socket() -> PathBuf {
    bo::connection::Connection::default_socket()
}
