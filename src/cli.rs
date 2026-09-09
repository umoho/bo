//! The mini CLI: `bo play | pause | resume | seek <t> | stop | load <file>`.
//!
//! The shell face is deliberately small. Arrangement editing is code, not
//! text — it lives in the typed clients (Rust `bo::client::Bo`, Python
//! `pybo`), which share the daemon session with this CLI over the same Unix
//! socket. The CLI keeps what a terminal is for: auditioning a finished
//! session (`play`/`pause`/`resume`/`seek`/`stop`) and restoring one from a
//! snapshot the clients wrote (`load <file>`, a `save`d `.bo` snapshot).
//!
//! Every invocation is one thin translation onto the typed wire: parse the
//! text, call the same `Bo` method a client would, render the typed reply
//! back as one `ok:` / `err:` line. No arrangement logic lives here. The
//! daemon (`bo daemon`, hidden) is spawned on demand and cleaned up by
//! `stop`, by playback finishing, or after an idle timeout.
//!
//! Exit codes: `0` ok, `1` refused, `2` usage.

use std::path::PathBuf;
use std::time::Duration;

use bo::client::{Bo, Played};
use bo::connection::Connection;
use clap::{Parser, Subcommand};

/// bo — the audio editor's mini CLI: transport and restore.
#[derive(Debug, Parser)]
#[command(
    name = "bo",
    version,
    arg_required_else_help = true,
    max_term_width = 80
)]
struct Cli {
    /// Unix socket the daemon listens on.
    #[arg(long, global = true)]
    socket: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

/// The one command an invocation runs.
#[derive(Debug, Subcommand)]
enum Command {
    /// Start playback from the current playhead. Refused when the
    /// arrangement has nothing to play.
    Play,
    /// Hold position and silence output.
    Pause,
    /// Continue after a pause.
    Resume,
    /// Move the playhead to a timecode (SS, MM:SS or HH:MM:SS).
    Seek {
        /// Target timecode.
        at: String,
    },
    /// Stop and rewind; ends the daemon's session (cleanup as usual).
    Stop,
    /// Replace the arrangement from a snapshot written by a client's save.
    Load {
        /// Snapshot path (.bo).
        file: String,
    },
    /// Hidden: run the session daemon (spawned by clients on demand).
    #[command(hide = true)]
    Daemon,
}

/// Entry point for `main`: parse, translate, print the reply, return the
/// exit code.
pub fn run(args: Vec<String>) -> i32 {
    let cli = match Cli::try_parse_from(std::iter::once("bo".to_string()).chain(args)) {
        Ok(cli) => cli,
        Err(e) => {
            let _ = e.print();
            return e.exit_code();
        }
    };
    match cli.command {
        Command::Daemon => {
            let socket = match cli.socket {
                Some(path) => absolutize(path),
                None => crate::daemon::default_socket(),
            };
            crate::daemon::main(&socket)
        }
        command => client_main(cli.socket, command),
    }
}

/// One client round trip: reach the daemon (spawning it on demand), run the
/// verb through the same `Bo` a client uses, print the rendered reply.
fn client_main(socket: Option<PathBuf>, command: Command) -> i32 {
    let mut bo = match socket {
        Some(path) => Bo::with_connection(Connection::at(absolutize(path))),
        None => Bo::new(),
    };
    let result = match command {
        Command::Play => run_play(&mut bo),
        Command::Pause => {
            bo.pause().map(|at| println!("ok: paused at {}", time(at)))
        }
        Command::Resume => {
            bo.resume().map(|at| println!("ok: playing from {}", time(at)))
        }
        Command::Seek { at } => match timecode(&at) {
            Ok(at) => bo
                .seek(at)
                .map(|()| println!("ok: playhead at {}", time(at))),
            Err(e) => return usage(&e),
        },
        Command::Stop => bo.stop().map(|()| println!("ok: stopped")),
        Command::Load { file } => {
            bo.load(&file).map(|()| println!("ok: loaded '{file}'"))
        }
        Command::Daemon => unreachable!("the daemon is run by run()"),
    };
    match result {
        Ok(()) => 0,
        Err(e) => fail(&e.to_string()),
    }
}

/// `play`: refuse an arrangement with nothing to play (a silence that would
/// finish instantly), then start from the current playhead and say what is
/// about to play.
fn run_play(bo: &mut Bo) -> Result<(), bo::client::Error> {
    let played: Played = bo.play()?;
    if played.clips == 0 {
        return Err(bo::client::Error::Value(
            "no clips: nothing to play".to_string(),
        ));
    }
    println!(
        "ok: {} track{}, {} clip{}, ends {}, playing from {}",
        played.tracks,
        plural(played.tracks),
        played.clips,
        plural(played.clips),
        time(played.end),
        time(played.playhead),
    );
    Ok(())
}

/// A usage error: exit 2.
fn usage(msg: &str) -> i32 {
    println!("err: {msg}");
    2
}

/// A refused operation: exit 1.
fn fail(msg: &str) -> i32 {
    println!("err: {msg}");
    1
}

/// `""` for one, `"s"` otherwise.
fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// A duration as the reply's timecode.
fn time(d: Duration) -> String {
    bo::time::format(d)
}

/// Parse a lenient timecode (`SS`, `MM:SS`, `HH:MM:SS`, optional `.fff`).
fn timecode(s: &str) -> Result<Duration, String> {
    bo::time::parse(s).map_err(|e| format!("bad timecode {s:?}: {e}"))
}

/// Resolve a path against the current directory (the daemon and the socket
/// are addressed from where the invocation ran).
fn absolutize(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        return path;
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(path),
        Err(_) => path,
    }
}
