//! The agent-facing CLI: a client for a playback daemon.
//!
//! The arrangement and the transport live in a long-running daemon, not in
//! files. Every command reaches the daemon over a Unix socket,
//! auto-spawning it when it is not running; the daemon exits and cleans up
//! its socket when playback finishes or is stopped.
//!
//! # Lifecycle
//!
//! ```text
//! bo put / play / ...        bo daemon (spawned on demand)
//!   connect to socket  ───►   owns: arrangement + transport + clock
//!   send one command line     replies with exit code + output
//!   print reply, exit         exits when the program is done
//! ```
//!
//! The socket lives at `$TMPDIR/bo/daemon.sock` unless `--socket <path>` says
//! otherwise. An arrangement is memory-only: it survives as long as the
//! daemon does. A stale socket left by a dead daemon is removed and replaced
//! by the next command.
//!
//! # Commands
//!
//! * `put <spec> [track]` — place a clip on a track; without `[track]` a new
//!   track is created and its index printed, so later puts can name it. With
//!   `[track]` the track is used, created on demand (up to that index).
//! * `play` — start playback from the current playhead.
//! * `pause` / `resume` — hold and continue, keeping the position.
//! * `stop` — stop and rewind; ends the daemon's session (cleanup as usual).
//! * `seek <t>` — move the playhead; a running transport re-plans.
//! * `volume <track> <v>` — set a track's gain in the mix (0..1, clamped).
//! * `mute <track> [on|off]` — mute or unmute a track (default on).
//! * `take <track> <clip>` — remove a clip; the indices are the ones `put`
//!   and `ls` print.
//! * `ls` — dump the whole arrangement.
//!
//! # Clip specs
//!
//! A spec is one compact string, `uri[@at][:from-to]`:
//!
//! * `uri` alone — the whole source at track time 0;
//! * `@at` — place the clip at track time `at` instead;
//! * `:from-to` — play only `from..to` of the source; `to` may be empty
//!   (`from-`), meaning "to the end of the source";
//! * `@at:from-to` — both.
//!
//! Timecodes are `SS`, `MM:SS` or `HH:MM:SS`, plus an optional `.fff`
//! fraction. In `@at:from-to` the `at`/`from` boundary is the rightmost colon
//! that leaves two valid timecodes, so the serialized form — every field as
//! `HH:MM:SS.fff` — round-trips exactly.
//!
//! A clip with no known end (an unsliced, unprobed source) is open-ended: it
//! blocks everything after it on the same track, and an arrangement that
//! contains one never finishes — the daemon plays until told to stop. Slice
//! what you place (`uri:from-to`) to keep arranging.

use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use bo::engine::{Player, Silent, State};
use bo::track::{Clip, Source, Track};
use clap::{Parser, Subcommand, ValueEnum};

/// bo — arrange and play a radio program.
#[derive(Debug, Parser)]
#[command(name = "bo", version, arg_required_else_help = true, max_term_width = 80)]
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
    /// Place a clip on a track.
    ///
    /// The spec is `uri[@at][:from-to]`: `at` positions the clip on the track
    /// (default 0), `from-to` slices the source (an empty `to` plays to the
    /// source's end). Timecodes are SS, MM:SS or HH:MM:SS with an optional
    /// .fff fraction. Without a track index a new track is created and its
    /// index printed; a named track is created on demand.
    Put {
        /// uri[@at][:from-to]
        spec: String,
        /// Track index; omit to create a fresh track.
        track: Option<usize>,
    },
    /// Start playback from the current playhead.
    Play,
    /// Show the whole arrangement.
    Ls,
    /// Pause the transport, keeping the position.
    Pause,
    /// Resume after a pause.
    Resume,
    /// Stop and rewind; ends the daemon's session.
    Stop,
    /// Move the playhead to a timecode (SS, MM:SS or HH:MM:SS).
    Seek {
        /// Target timecode.
        at: String,
    },
    /// Set a track's gain in the mix, 0.0 ..= 1.0 (clamped).
    Volume {
        /// Track index.
        track: usize,
        /// Gain.
        v: f32,
    },
    /// Mute or unmute a track in the mix; defaults to on.
    Mute {
        /// Track index.
        track: usize,
        /// on or off; omitted means on.
        state: Option<MuteState>,
    },
    /// Remove a clip by its indices — the reverse of `put`.
    Take {
        /// Track index.
        track: usize,
        /// Clip index.
        clip: usize,
    },
    /// Hidden: run the playback daemon (spawned by the client on demand).
    #[command(hide = true)]
    Daemon,
}

/// Mute switch value for [`Command::Mute`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum MuteState {
    On,
    Off,
}

/// A clip description before it exists: `uri[@at][:from-to]`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Spec {
    uri: String,
    at: Option<Duration>,
    from: Duration,
    to: Option<Duration>,
}

/// The arrangement a command works on: the player's stacked tracks.
#[derive(Debug, Default)]
struct Arrangement {
    player: Player<Silent>,
}

/// Where the daemon listens by default: `$TMPDIR/bo/daemon.sock`.
fn default_socket() -> PathBuf {
    std::env::temp_dir().join("bo").join("daemon.sock")
}

/// Parse `SS`, `MM:SS` or `HH:MM:SS` (optional `.fff` fraction) into a
/// duration.
fn parse_timecode(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty timecode".to_string());
    }
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() > 3 {
        return Err(format!("timecode has too many fields: {s:?}"));
    }
    let mut total = 0.0_f64;
    let mut scale = 1.0_f64;
    for part in parts.iter().rev() {
        let value: f64 = part.parse().map_err(|_| format!("bad timecode {s:?}"))?;
        total += value * scale;
        scale *= 60.0;
    }
    if !total.is_finite() || total < 0.0 {
        return Err(format!("bad timecode {s:?}"));
    }
    Ok(Duration::from_secs_f64(total))
}

/// Format a duration as `HH:MM:SS.fff`. Exact integer math, no floats.
fn format_time(d: Duration) -> String {
    let total_ms = d.as_secs().saturating_mul(1000) + u64::from(d.subsec_millis());
    let ms = total_ms % 1000;
    let s = (total_ms / 1000) % 60;
    let m = (total_ms / 60_000) % 60;
    let h = total_ms / 3_600_000;
    format!("{h:02}:{m:02}:{s:02}.{ms:03}")
}

/// Parse a clip spec into its pieces. See the module docs for the grammar.
fn parse_spec(s: &str) -> Result<Spec, String> {
    let s = s.trim();
    let mut spec = Spec {
        uri: String::new(),
        at: None,
        from: Duration::ZERO,
        to: None,
    };
    if let Some((uri, placement)) = s.split_once('@') {
        spec.uri = uri.trim().to_string();
        let placement = placement.trim();
        if placement.is_empty() {
            return Ok(spec); // "uri@" — just the uri
        }
        if let Some((lhs, rhs)) = placement.split_once('-') {
            let (at, from) = split_at_from(lhs)?;
            spec.at = at;
            spec.from = from;
            let rhs = rhs.trim();
            spec.to = if rhs.is_empty() { None } else { Some(parse_timecode(rhs)?) };
        } else {
            spec.at = Some(parse_timecode(placement)?);
        }
    } else if let Some((uri, placement)) = s.split_once(':') {
        // Slice without a position: uri:from[-to]. A ':' that does not begin a
        // slice (e.g. scheme://) is left alone.
        let placement = placement.trim();
        if let Some((from, to)) = placement.split_once('-') {
            spec.uri = uri.trim().to_string();
            spec.from = parse_timecode(from)?;
            let to = to.trim();
            spec.to = if to.is_empty() { None } else { Some(parse_timecode(to)?) };
        } else {
            spec.uri = s.to_string();
        }
    } else {
        spec.uri = s.to_string();
    }
    if spec.uri.is_empty() {
        return Err("missing uri in clip spec".to_string());
    }
    Ok(spec)
}

/// The `at`/`from` boundary inside `@at:from-to`: the rightmost colon that
/// splits `lhs` into two valid timecodes. With none, the whole `lhs` is
/// `from`.
fn split_at_from(lhs: &str) -> Result<(Option<Duration>, Duration), String> {
    let mut best: Option<(Duration, Duration)> = None;
    for (i, ch) in lhs.char_indices() {
        if ch == ':'
            && let (Ok(at), Ok(from)) = (parse_timecode(&lhs[..i]), parse_timecode(&lhs[i + 1..]))
        {
            best = Some((at, from));
        }
    }
    match best {
        Some((at, from)) => Ok((Some(at), from)),
        None => parse_timecode(lhs).map(|from| (None, from)),
    }
}

/// Exit codes: 2 for misuse, 1 for a refused operation.
fn usage(msg: impl Into<String>) -> (i32, String) {
    (2, msg.into())
}

fn fail(msg: impl Into<String>) -> (i32, String) {
    (1, msg.into())
}

/// Parse the whole command line, including the global `--socket` option.
fn parse_full(args: &[String]) -> Result<Cli, clap::Error> {
    let argv = std::iter::once("bo".to_string()).chain(args.iter().cloned());
    Cli::try_parse_from(argv)
}

/// Parse a command line (the client's subcommand, or a line on the socket)
/// into a subcommand.
fn parse_command(args: &[String]) -> Result<Command, clap::Error> {
    let argv = std::iter::once("bo".to_string()).chain(args.iter().cloned());
    Cli::try_parse_from(argv).map(|cli| cli.command)
}

/// Run a parsed subcommand against the arrangement.
fn dispatch(a: &mut Arrangement, command: Command) -> Result<String, (i32, String)> {
    match command {
        Command::Put { spec, track } => put_command(a, &spec, track),
        Command::Play => play_command(a),
        Command::Ls => Ok(format_arrangement(a)),
        Command::Pause => {
            a.player.pause();
            Ok(format!("paused at {}\n", format_time(a.player.playhead())))
        }
        Command::Resume => {
            a.player.resume().map_err(|e| fail(e.to_string()))?;
            Ok(format!("playing from {}\n", format_time(a.player.playhead())))
        }
        Command::Stop => {
            a.player.stop();
            Ok("stopped\n".to_string())
        }
        Command::Seek { at } => {
            let t = parse_timecode(&at).map_err(usage)?;
            a.player.seek(t).map_err(|e| fail(e.to_string()))?;
            Ok(format!("playhead at {}\n", format_time(t)))
        }
        Command::Volume { track, v } => {
            let t = a
                .player
                .tracks_mut()
                .get_mut(track)
                .ok_or_else(|| fail(format!("no track {track}")))?;
            t.set_volume(v);
            Ok(format!("track {track} volume {:.2}\n", t.volume()))
        }
        Command::Mute { track, state } => {
            let muted = matches!(state, None | Some(MuteState::On));
            let t = a
                .player
                .tracks_mut()
                .get_mut(track)
                .ok_or_else(|| fail(format!("no track {track}")))?;
            t.set_muted(muted);
            let word = if muted { "muted" } else { "unmuted" };
            Ok(format!("track {track} {word}\n"))
        }
        Command::Take { track, clip } => {
            let removed = a.player.tracks_mut().get_mut(track).and_then(|t| t.remove(clip));
            match removed {
                Some(_) => Ok(format!("removed track {track} clip #{clip}\n")),
                None => Err(fail(format!("no clip {track}#{clip}"))),
            }
        }
        Command::Daemon => Err(fail("the daemon runs standalone, not over the socket")),
    }
}

/// The whole arrangement as text: transport line, then one block per track.
fn format_arrangement(a: &Arrangement) -> String {
    let p = &a.player;
    let mut out = format!(
        "player: {} | playhead {} | volume {:.2}\n",
        p.state(),
        format_time(p.playhead()),
        p.volume()
    );
    if p.tracks().is_empty() {
        out.push_str("no tracks\n");
        return out;
    }
    for (ti, t) in p.tracks().iter().enumerate() {
        let dur = t.duration().map(format_time).unwrap_or_else(|| "inf".into());
        let noun = if t.len() == 1 { "clip" } else { "clips" };
        let mute = if t.muted() { "  muted" } else { "" };
        let head = match t.name() {
            Some(name) => format!("track {ti} {name:?}  volume {:.2}{mute}  {} {noun}", t.volume(), t.len()),
            None => format!("track {ti}  volume {:.2}{mute}  {} {noun}", t.volume(), t.len()),
        };
        let _ = writeln!(out, "{head}  -> {dur}");
        for (ci, c) in t.clips().iter().enumerate() {
            let end = c.end().map(format_time).unwrap_or_else(|| "inf".into());
            let src_to = c.to.or(c.source.duration).map(format_time).unwrap_or_else(|| "inf".into());
            let _ = writeln!(
                out,
                "  #{ci}  {}  {} -> {}  (src {} -> {})",
                c.source.uri,
                format_time(c.at),
                end,
                format_time(c.from),
                src_to
            );
        }
    }
    out
}

/// `put <spec> [track]`: place a clip, creating the track when needed.
///
/// The identifier in the output is the contract: an implicit put creates a
/// fresh track and prints its index; an explicit one names a track, created on
/// demand so that repeated puts rebuild the same layout.
fn put_command(
    a: &mut Arrangement,
    spec_arg: &str,
    want_track: Option<usize>,
) -> Result<String, (i32, String)> {
    let spec = parse_spec(spec_arg).map_err(usage)?;
    let track_index = match want_track {
        Some(i) => {
            while a.player.tracks().len() <= i {
                a.player.add_track(Track::new());
            }
            i
        }
        None => a.player.add_track(Track::new()),
    };
    let source = Arc::new(Source::new(spec.uri.clone()));
    let clip = Clip::sliced(source, spec.from, spec.to);
    let clip = match spec.at {
        Some(at) => clip.at(at),
        None => clip,
    };
    let idx = match a.player.tracks_mut()[track_index].insert(clip) {
        Ok(i) => i,
        Err((_, overlap)) => return Err(fail(format!("refused: {overlap}"))),
    };
    let placed = &a.player.tracks()[track_index].clips()[idx];
    let open = if placed.duration().is_none() { " (open-ended)" } else { "" };
    Ok(format!(
        "ok: track {track_index} clip #{idx} {} @ {}{open}\n",
        placed.source.uri,
        format_time(placed.at)
    ))
}

/// `play`: start playback from the current playhead. The daemon's clock loop
/// advances the playhead and exits when the program is done.
fn play_command(a: &mut Arrangement) -> Result<String, (i32, String)> {
    a.player.play().map_err(|e| fail(e.to_string()))?;
    Ok(format!("playing from {}\n", format_time(a.player.playhead())))
}

/// The command line the client puts on the wire, re-serialized from the
/// already-parsed subcommand.
fn command_line(command: &Command) -> String {
    match command {
        Command::Put { spec, track } => match track {
            Some(t) => format!("put {spec} {t}"),
            None => format!("put {spec}"),
        },
        Command::Play => "play".to_string(),
        Command::Ls => "ls".to_string(),
        Command::Pause => "pause".to_string(),
        Command::Resume => "resume".to_string(),
        Command::Stop => "stop".to_string(),
        Command::Seek { at } => format!("seek {at}"),
        Command::Volume { track, v } => format!("volume {track} {v}"),
        Command::Mute { track, state } => match state {
            Some(MuteState::On) => format!("mute {track} on"),
            Some(MuteState::Off) => format!("mute {track} off"),
            None => format!("mute {track}"),
        },
        Command::Take { track, clip } => format!("take {track} {clip}"),
        Command::Daemon => unreachable!("the daemon is spawned, not sent"),
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Entry point for `main`: parse the command line, hand it to the daemon
/// (spawning one if needed), print the reply, return its exit code. Returns
/// the exit code.
pub fn run(args: Vec<String>) -> i32 {
    let cli = match parse_full(&args) {
        Ok(cli) => cli,
        Err(e) => {
            let _ = e.print();
            return e.exit_code();
        }
    };
    let socket = cli.socket.unwrap_or_else(default_socket);
    match cli.command {
        Command::Daemon => daemon_main(&socket),
        command => client_main(&socket, &command),
    }
}

/// One client round trip: reach the daemon, send the command, print the
/// reply.
fn client_main(socket: &Path, command: &Command) -> i32 {
    let mut stream = match connect_or_spawn(socket) {
        Ok(stream) => stream,
        Err(msg) => {
            eprintln!("bo: {msg}");
            return 1;
        }
    };
    let line = command_line(command);
    if let Err(e) = stream.write_all(format!("{line}\n").as_bytes()) {
        eprintln!("bo: cannot reach the daemon: {e}");
        return 1;
    }
    let mut reply = String::new();
    if let Err(e) = stream.read_to_string(&mut reply) {
        eprintln!("bo: cannot read the daemon's reply: {e}");
        return 1;
    }
    // Reply framing: first line is the exit code, the rest the output.
    match reply.split_once('\n') {
        Some((code, out)) => {
            print!("{out}");
            code.trim().parse().unwrap_or(1)
        }
        None => {
            print!("{reply}");
            1
        }
    }
}

/// Connect to the daemon, spawning it (and clearing a stale socket) when it
/// is not there.
fn connect_or_spawn(socket: &Path) -> Result<UnixStream, String> {
    if let Ok(stream) = UnixStream::connect(socket) {
        return Ok(stream);
    }
    let _ = std::fs::remove_file(socket);
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let exe = std::env::current_exe().map_err(|e| format!("cannot find own binary: {e}"))?;
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

// ---------------------------------------------------------------------------
// Daemon
// ---------------------------------------------------------------------------

/// The daemon: bind the socket, serve commands, advance the clock, and exit
/// (cleaning up the socket) when the program finishes or is stopped.
fn daemon_main(socket: &Path) -> i32 {
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
    let state = Arc::new(Mutex::new(Arrangement::default()));
    let exit = Arc::new(AtomicBool::new(false));

    let serve_state = state.clone();
    let serve_exit = exit.clone();
    let serve = thread::spawn(move || serve_loop(listener, serve_state, serve_exit));
    let _ = serve;

    // Clock loop: advance the playhead by real elapsed time and watch for
    // completion. `stop` (via the exit flag) ends the session the same way.
    let mut last = Instant::now();
    loop {
        if exit.load(Ordering::Relaxed) {
            break;
        }
        let now = Instant::now();
        let dt = now - last;
        last = now;
        let finished = {
            let mut a = state.lock().unwrap();
            a.player.advance(dt);
            // Only a played session completes: a freshly spawned daemon that
            // has not been told to play must not tear itself down.
            a.player.state() == State::Playing && a.player.is_finished()
        };
        if finished {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    let _ = std::fs::remove_file(socket);
    0
}

/// Accept connections and handle each command on its own thread. The clock
/// loop owns the transport; handlers only lock it briefly.
fn serve_loop(listener: UnixListener, state: Arc<Mutex<Arrangement>>, exit: Arc<AtomicBool>) {
    for connection in listener.incoming() {
        let Ok(mut stream) = connection else { continue };
        let state = state.clone();
        let exit = exit.clone();
        thread::spawn(move || {
            let mut line = String::new();
            let mut reader = BufReader::new(&mut stream);
            let Ok(_) = reader.read_line(&mut line) else { return };
            let (code, output, ends_session) = handle_line(&state, line.trim_end());
            let _ = stream.write_all(format!("{code}\n{output}").as_bytes());
            // The reply is on the wire before the session may end, so the
            // client never sees a truncated response.
            if ends_session {
                exit.store(true, Ordering::Relaxed);
            }
        });
    }
}

/// One command over the wire: parse, dispatch, and frame the reply. The third
/// value says whether this command ends the daemon's session.
fn handle_line(state: &Mutex<Arrangement>, line: &str) -> (i32, String, bool) {
    let args: Vec<String> = line.split_whitespace().map(str::to_string).collect();
    let command = match parse_command(&args) {
        Ok(command) => command,
        Err(e) => return (e.exit_code(), format!("{}\n", e.to_string().trim_end()), false),
    };
    let ends_session = matches!(command, Command::Stop);
    let mut a = state.lock().unwrap();
    match dispatch(&mut a, command) {
        Ok(out) => (0, out, ends_session),
        Err((code, msg)) => (code, format!("bo: {msg}\n"), ends_session),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bo::engine::State;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn run_ok(a: &mut Arrangement, args: &[&str]) -> String {
        let v: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        match parse_command(&v).map_err(|e| (e.exit_code(), String::new())).and_then(|c| dispatch(a, c)) {
            Ok(out) => out,
            Err((code, msg)) => panic!("command {args:?} failed ({code}): {msg}"),
        }
    }

    fn run_err(a: &mut Arrangement, args: &[&str]) -> (i32, String) {
        let v: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        parse_command(&v)
            .map_err(|e| (e.exit_code(), String::new()))
            .and_then(|c| dispatch(a, c))
            .unwrap_err()
    }

    fn temp_dir() -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "bo-cli-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn send(socket: &Path, line: &str) -> String {
        let mut stream = UnixStream::connect(socket).unwrap();
        stream.write_all(format!("{line}\n").as_bytes()).unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        reply
    }

    fn wait_until<F: Fn() -> bool>(what: &str, check: F) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !check() {
            assert!(Instant::now() < deadline, "timeout waiting for {what}");
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn timecodes_parse_and_format() {
        assert_eq!(parse_timecode("5").unwrap(), Duration::from_secs(5));
        assert_eq!(parse_timecode("01:30").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_timecode("01:30.5").unwrap(), Duration::from_millis(90_500));
        assert_eq!(parse_timecode("00:01:00").unwrap(), Duration::from_secs(60));
        assert_eq!(parse_timecode("1:02:03.25").unwrap(), Duration::from_millis(3_723_250));
        assert!(parse_timecode("").is_err());
        assert!(parse_timecode("1:2:3:4").is_err());
        assert!(parse_timecode("bogus").is_err());
        assert!(parse_timecode("-5").is_err());

        assert_eq!(format_time(Duration::ZERO), "00:00:00.000");
        assert_eq!(format_time(Duration::from_secs(90)), "00:01:30.000");
        assert_eq!(format_time(Duration::from_millis(90_500)), "00:01:30.500");
        for ms in [0u64, 5, 59, 60, 3661, 90_500, 3_723_250] {
            let d = Duration::from_millis(ms);
            assert_eq!(parse_timecode(&format_time(d)).unwrap(), d, "round trip of {d:?}");
        }
    }

    #[test]
    fn clip_specs_parse() {
        let s = parse_spec("a.wav").unwrap();
        assert_eq!((s.uri.as_str(), s.at, s.from, s.to), ("a.wav", None, Duration::ZERO, None));

        let s = parse_spec("a.wav@00:30").unwrap();
        assert_eq!((s.uri.as_str(), s.at), ("a.wav", Some(Duration::from_secs(30))));

        let s = parse_spec("a.wav:00:00:30-00:00:45").unwrap();
        assert_eq!(
            (s.uri.as_str(), s.at, s.from, s.to),
            ("a.wav", None, Duration::from_secs(30), Some(Duration::from_secs(45)))
        );

        let s = parse_spec("a.wav@00:01:00:00:00:30-00:00:45").unwrap();
        assert_eq!(s.at, Some(Duration::from_secs(60)));
        assert_eq!(s.from, Duration::from_secs(30));
        assert_eq!(s.to, Some(Duration::from_secs(45)));

        let s = parse_spec("a.wav@00:01:00:00:00:30-").unwrap();
        assert_eq!(s.to, None);

        let s = parse_spec("my-file.wav:00:30-00:45").unwrap();
        assert_eq!(s.uri, "my-file.wav");
        assert_eq!(s.from, Duration::from_secs(30));

        let s = parse_spec("a.wav@").unwrap();
        assert_eq!(s.uri, "a.wav");
        assert_eq!(s.at, None);

        assert!(parse_spec("").is_err());
        assert!(parse_spec("a.wav@bogus").is_err());
    }

    #[test]
    fn put_creates_tracks_and_returns_identifiers() {
        let mut a = Arrangement::default();
        let out = run_ok(&mut a, &["put", "a.wav:00:00:00-00:00:10"]);
        assert!(out.contains("track 0"), "{out}");
        let out = run_ok(&mut a, &["put", "b.wav:00:00:00-00:00:10"]);
        assert!(out.contains("track 1"), "each implicit put gets a fresh track: {out}");
        assert_eq!(a.player.tracks().len(), 2);

        // The printed identifier names a track for later puts.
        let out = run_ok(&mut a, &["put", "c.wav@00:00:10:00:00:00-00:00:05", "0"]);
        assert!(out.contains("track 0"), "{out}");
        let t = &a.player.tracks()[0];
        assert_eq!(t.clips().len(), 2);
        assert_eq!(t.clips()[1].at, Duration::from_secs(10), "butt-joined on the named track");
    }

    #[test]
    fn a_named_track_is_created_on_demand() {
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", "a.wav:00:00:00-00:00:10", "3"]);
        assert_eq!(a.player.tracks().len(), 4, "tracks up to the index are created");
        assert_eq!(a.player.tracks()[3].clips().len(), 1);
        assert!(a.player.tracks()[0].is_empty());
    }

    #[test]
    fn open_ended_put_blocks_the_track_and_is_refused() {
        let mut a = Arrangement::default();
        let out = run_ok(&mut a, &["put", "live.wav"]);
        assert!(out.contains("(open-ended)"), "{out}");
        let (code, msg) = run_err(&mut a, &["put", "b.wav@00:00:05:00:00:00-00:00:10", "0"]);
        assert_eq!(code, 1);
        assert!(msg.contains("collides"), "{msg}");
        assert_eq!(a.player.tracks()[0].clips().len(), 1, "a refused insert leaves no trace");
    }

    #[test]
    fn play_starts_transport() {
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", "a.wav:00:00:00-00:00:10"]);
        let out = run_ok(&mut a, &["play"]);
        assert!(out.contains("playing from 00:00:00.000"), "{out}");
        assert_eq!(a.player.state(), State::Playing);
    }

    #[test]
    fn ls_shows_the_arrangement() {
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", "a.wav:00:00:00-00:00:10"]);
        run_ok(&mut a, &["put", "b.wav@00:00:10:00:00:00-00:00:05", "0"]);
        let out = run_ok(&mut a, &["ls"]);
        assert!(out.contains("player: stopped"), "{out}");
        assert!(out.contains("track 0  volume 1.00  2 clips"), "{out}");
        assert!(out.contains("a.wav") && out.contains("b.wav"), "{out}");
        let out = run_ok(&mut a, &["ls"]);
        assert!(out.contains("00:00:10.000 -> 00:00:15.000"), "butt-joined clip: {out}");
    }

    #[test]
    fn volume_sets_a_tracks_gain_in_the_mix() {
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", "a.wav:00:00:00-00:00:10"]);
        let out = run_ok(&mut a, &["volume", "0", "0.5"]);
        assert!(out.contains("track 0 volume 0.50"), "{out}");
        assert_eq!(a.player.tracks()[0].volume(), 0.5);
        run_ok(&mut a, &["volume", "0", "2.5"]);
        assert_eq!(a.player.tracks()[0].volume(), 1.0, "clamped");
        let (code, msg) = run_err(&mut a, &["volume", "9", "0.5"]);
        assert_eq!(code, 1);
        assert!(msg.contains("no track 9"), "{msg}");
        assert!(run_ok(&mut a, &["ls"]).contains("volume 1.00"), "ls shows the gain");
    }

    #[test]
    fn take_removes_a_clip_by_its_identifiers() {
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", "a.wav:00:00:00-00:00:10"]);
        run_ok(&mut a, &["put", "b.wav@00:00:10:00:00:00-00:00:05", "0"]);
        let out = run_ok(&mut a, &["take", "0", "0"]);
        assert!(out.contains("removed track 0 clip #0"), "{out}");
        assert_eq!(a.player.tracks()[0].clips().len(), 1);
        assert_eq!(
            a.player.tracks()[0].clips()[0].source.uri,
            "b.wav",
            "later clips shift up"
        );
        let (code, msg) = run_err(&mut a, &["take", "0", "9"]);
        assert_eq!(code, 1);
        assert!(msg.contains("no clip 0#9"), "{msg}");
        let (code, _) = run_err(&mut a, &["take", "9", "0"]);
        assert_eq!(code, 1);
    }

    #[test]
    fn mute_toggles_a_track_in_the_mix() {
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", "a.wav:00:00:00-00:00:10"]);
        let out = run_ok(&mut a, &["mute", "0"]);
        assert!(out.contains("track 0 muted"), "{out}");
        assert!(a.player.tracks()[0].muted());
        let out = run_ok(&mut a, &["mute", "0", "off"]);
        assert!(out.contains("track 0 unmuted"), "{out}");
        assert!(!a.player.tracks()[0].muted());
        assert!(!run_ok(&mut a, &["ls"]).contains("muted"), "no marker when unmuted");
        run_ok(&mut a, &["mute", "0"]);
        assert!(run_ok(&mut a, &["ls"]).contains("muted"), "ls shows the marker");
        let (code, msg) = run_err(&mut a, &["mute", "9"]);
        assert_eq!(code, 1);
        assert!(msg.contains("no track 9"), "{msg}");
    }

    #[test]
    fn transport_commands_drive_the_state_machine() {
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", "a.wav:00:00:00-00:00:10"]);
        run_ok(&mut a, &["play"]);
        assert_eq!(a.player.state(), State::Playing);
        run_ok(&mut a, &["pause"]);
        assert_eq!(a.player.state(), State::Paused);
        run_ok(&mut a, &["resume"]);
        assert_eq!(a.player.state(), State::Playing);
        let out = run_ok(&mut a, &["seek", "00:00:07"]);
        assert!(out.contains("playhead at 00:00:07.000"), "{out}");
        assert_eq!(a.player.playhead(), Duration::from_secs(7));
        run_ok(&mut a, &["stop"]);
        assert_eq!(a.player.state(), State::Stopped);
        assert_eq!(a.player.playhead(), Duration::ZERO, "stop rewinds");
    }

    #[test]
    fn transport_commands_work_over_the_wire_and_stop_ends_the_session() {
        let dir = temp_dir();
        let socket = dir.join("d.sock");
        let handle = {
            let socket = socket.clone();
            thread::spawn(move || daemon_main(&socket))
        };
        wait_until("socket", || UnixStream::connect(&socket).is_ok());

        send(&socket, "put a.wav:00:00:00-00:00:10");
        let reply = send(&socket, "ls");
        assert!(reply.contains("track 0  volume 1.00  1 clip") && reply.contains("a.wav"), "{reply}");

        let reply = send(&socket, "volume 0 0.5");
        assert!(reply.contains("track 0 volume 0.50"), "{reply}");
        let reply = send(&socket, "mute 0");
        assert!(reply.contains("track 0 muted"), "{reply}");
        let reply = send(&socket, "mute 0 off");
        assert!(reply.contains("track 0 unmuted"), "{reply}");
        let reply = send(&socket, "take 0 0");
        assert!(reply.contains("removed track 0 clip #0"), "{reply}");
        let reply = send(&socket, "ls");
        assert!(!reply.contains("a.wav"), "the clip is gone: {reply}");

        let reply = send(&socket, "seek 00:00:05");
        assert!(reply.contains("playhead at 00:00:05.000"), "{reply}");
        let reply = send(&socket, "pause");
        assert!(reply.contains("paused at"), "{reply}");
        let reply = send(&socket, "resume");
        assert!(reply.contains("playing from"), "{reply}");
        // A bad timecode is refused with exit 2, session unaffected.
        let reply = send(&socket, "seek bogus");
        assert_eq!(reply.lines().next().unwrap(), "2", "{reply}");

        // stop ends the session: reply first, then cleanup.
        let reply = send(&socket, "stop");
        assert!(reply.contains("stopped"), "{reply}");
        wait_until("cleanup", || !socket.exists());
        assert_eq!(handle.join().unwrap(), 0);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn clap_help_and_usage_exit_codes() {
        assert_eq!(parse_full(&["--help".into()]).unwrap_err().exit_code(), 0);
        assert_eq!(parse_full(&["put".into(), "--help".into()]).unwrap_err().exit_code(), 0);
        assert_eq!(parse_full(&["nope".into()]).unwrap_err().exit_code(), 2);
        assert_eq!(parse_full(&["put".into()]).unwrap_err().exit_code(), 2);
        assert_eq!(parse_full(&["put".into(), "a.wav".into(), "x".into()]).unwrap_err().exit_code(), 2);
    }

    #[test]
    fn daemon_serves_commands_and_exits_when_playback_finishes() {
        let dir = temp_dir();
        let socket = dir.join("d.sock");

        let handle = {
            let socket = socket.clone();
            thread::spawn(move || daemon_main(&socket))
        };
        wait_until("socket", || UnixStream::connect(&socket).is_ok());

        let reply = send(&socket, "put a.wav:00:00:00-00:00:00.200");
        assert!(reply.contains("ok: track 0 clip #0"), "{reply}");
        let reply = send(&socket, "put b.wav@00:00:00.200:00:00:00-00:00:00.200 0");
        assert!(reply.contains("ok: track 0 clip #1"), "{reply}");

        // A refused command still gets a framed reply and exit code.
        let reply = send(&socket, "nope");
        assert_eq!(reply.lines().next().unwrap(), "2", "{reply}");

        // A 0.4s program: play it, and the daemon cleans up on completion.
        let reply = send(&socket, "play");
        assert!(reply.contains("playing from"), "{reply}");
        wait_until("cleanup", || !socket.exists());
        assert_eq!(handle.join().unwrap(), 0);

        std::fs::remove_dir_all(&dir).ok();
    }
}
