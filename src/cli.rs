//! The agent-facing CLI: the command surface of an audio editor and mixer.
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
//!   print reply, exit         exits when playback is done
//! ```
//!
//! The socket lives at `$TMPDIR/bo/daemon.sock` unless `--socket <path>` says
//! otherwise. An arrangement is memory-only: it survives as long as the
//! daemon does. A stale socket left by a dead daemon is removed and replaced
//! by the next command.
//!
//! The daemon plays through rodio when a device is available, falling back to
//! silence (with a note on `play`) when it is not; `BO_BACKEND=silent` forces
//! the headless backend for tests and CI.
//!
//! A daemon that is not playing exits on its own after `BO_IDLE_TIMEOUT`
//! seconds of silence (default 600; 0 disables), so a forgotten `pause`
//! cannot leave a process and socket behind forever. Playing never times out.
//!
//! # Commands
//!
//! * `put [--repeat n] <spec> [where]` — place a clip, or n butt-joined
//!   copies of it, on a track; `where` is `track[@pos]`, an omitted track
//!   creates a fresh one and an omitted pos means the playhead. A track is
//!   created on demand (up to that index). A source with no out-point is
//!   probed at put time; the whole batch is refused atomically if any copy
//!   collides.
//! * `play` — start playback from the current playhead; refused with exit 1
//!   when the arrangement has nothing to play, and the reply opens with a
//!   `session:` summary of what is about to play.
//! * `pause` / `resume` — hold and continue, keeping the position.
//! * `stop` — stop and rewind; ends the daemon's session (cleanup as usual).
//! * `seek <t>` — move the playhead; a running transport re-plans. Seeking
//!   past the arrangement's end is refused.
//! * `apply` — rebuild the running transport from the current playhead, so
//!   pending mix changes take effect now.
//! * `set <var> <value>` — set an attribute: `master` (real-time), or
//!   `track.N.volume` / `track.N.muted` / `track.N.name` (arrangement data;
//!   volume and mute land on the next `play` or `apply`).
//! * `take <track> <clip>` — remove a clip; the clip is addressed by its
//!   stable id or an `@timecode` (the clip covering that moment).
//! * `render [file] [from-to]` — mix the arrangement to a wav file,
//!   offline; a range renders only that span. With `--measure` the reply
//!   also reports the mix's peak/RMS/true peak and EBU R128 loudness,
//!   folded from the exact stream the file writer consumes; omit the file
//!   to measure the whole arrangement without writing.
//! * `save <file>` / `load <file>` — write the arrangement as a script of
//!   commands, or replace it from one (transport resets with the swap).
//! * `reset` — drop every track and stop the transport: the daemon is back
//!   to its fresh state, ready for a session script to rebuild it.
//! * `check` — verify every distinct source is readable.
//! * `probe [uri]` — measure the length of a source, or of every distinct
//!   source in the arrangement; a bare uri is probed locally, no daemon.
//! * `ls` — dump the whole arrangement as machine-readable text: a `key: value`
//!   status block, then one `key=value` line per track and per clip.
//! * `at <t>` — show the mix at track time `t`: every clip covering that
//!   moment, one per track.
//!
//! # Clip specs
//!
//! A spec is one compact string, `uri[,from-to]`:
//!
//! * `uri` alone — the whole source, probed for its length;
//! * `,from-to` — play only `from..to` of the source; `to` may be empty
//!   (`from-`), meaning "to the end of the source".
//!
//! Placement is a separate argument, `track[@pos]`: `@` marks the track
//! position (default the playhead); `,` marks the slice; `:` is reserved for
//! timecodes (`SS`, `MM:SS` or `HH:MM:SS`, plus an optional `.fff` fraction).
//! A source with no out-point is probed at put time so every clip has a known
//! finite length; a source that cannot be measured is refused.

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

use bo::engine::rodio::{check_sources, probe, probe_sources, render_and_measure, render_to_file, Rodio};
use bo::engine::{Backend, BackendError, Player, Silent, State};
use bo::track::{Clip, Source, Track};
use clap::error::ErrorKind;
use clap::{Parser, Subcommand};

mod reply;

use reply::{AtLine, Ls, LsClip, LsTrack, Output, PlacedClip, ProbeResult, SetResult, Tc};

/// bo — edit and mix audio, one command at a time.
#[derive(Debug, Parser)]
#[command(
    name = "bo",
    version,
    arg_required_else_help = true,
    max_term_width = 80,
    disable_help_subcommand = true
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
    /// Place a clip on a track.
    ///
    /// The spec is `uri[,from-to]`: `from-to` slices the source (default the
    /// whole source, resolved by probing). The placement is `track[@pos]`: a
    /// track index (created on demand) and a position (default the playhead).
    /// Timecodes are SS, MM:SS or HH:MM:SS with an optional .fff fraction.
    Put {
        /// uri[,from-to]
        spec: String,
        /// Where to place it: track[@pos] — omit the track for a fresh one,
        /// omit the pos for the playhead.
        placement: Option<String>,
        /// Place this many butt-joined copies of the clip.
        #[arg(long)]
        repeat: Option<u32>,
    },
    /// Remove a clip by its id or the `@timecode` it covers.
    Take {
        /// Track index.
        track: usize,
        /// Clip id, or `@timecode`.
        clip: String,
    },
    /// Show the whole arrangement.
    Ls,
    /// Show the mix at track time `t`: every clip covering that moment,
    /// one per track.
    At {
        /// Track timecode.
        at: String,
    },
    /// Mix the arrangement to a wav file, offline; optionally only a range.
    /// With `--measure`, also report peak/RMS/LUFS; omit the file to
    /// measure the whole arrangement without writing.
    Render {
        /// Output wav path; may be omitted with `--measure`.
        file: Option<String>,
        /// Range to render: `from-to`, `from-`, or nothing for the whole
        /// arrangement.
        range: Option<String>,
        /// Report peak/RMS/true peak and EBU R128 loudness of the mix.
        #[arg(long)]
        measure: bool,
    },
    /// Write the arrangement as a script of commands.
    Save {
        /// Output script path.
        file: String,
    },
    /// Replace the arrangement from a script written by `save`.
    Load {
        /// Script path.
        file: String,
    },
    /// Drop every track and stop the transport: back to a fresh session.
    Reset,
    /// Verify every source in the arrangement is readable.
    Check,
    /// Measure the length of a source, or of every distinct source in the
    /// arrangement. A bare uri runs locally — no daemon is spawned.
    Probe {
        /// Source uri to measure; omit to probe the arrangement's sources.
        uri: Option<String>,
    },
    /// Set an attribute: `master`, or `track.N.volume` / `track.N.muted` /
    /// `track.N.name`.
    Set {
        /// Attribute path.
        var: String,
        /// Value: a gain, `true`/`false`, or a name.
        value: String,
    },
    /// Start playback from the current playhead.
    Play,
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
    /// Apply the arrangement to a running transport, so pending mix
    /// changes (volume, mute) take effect now.
    Apply,
    /// Show the grouped help.
    #[command(hide = true)]
    Help,
    /// Hidden: run the session daemon (spawned by the client on demand).
    #[command(hide = true)]
    Daemon,
}

/// A clip description before it exists: `uri[,from-to]`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Spec {
    uri: String,
    from: Duration,
    to: Option<Duration>,
}

/// The arrangement a command works on: the player's stacked tracks over the
/// daemon's runtime backend.
#[derive(Debug)]
struct Arrangement {
    player: Player<AnyBackend>,
}

impl Default for Arrangement {
    fn default() -> Self {
        Self::with_backend(AnyBackend::silent())
    }
}

impl Arrangement {
    fn with_backend(backend: AnyBackend) -> Self {
        Self {
            player: Player::new(backend),
        }
    }
}

/// The daemon's runtime backend: real audio when the device opened, silence
/// otherwise (forced by `BO_BACKEND=silent`, or when no device exists).
#[derive(Debug)]
enum AnyBackend {
    /// Headless. The `String` is why, when a device was wanted but absent.
    Silent(Silent, Option<String>),
    /// Real audio.
    Rodio(Rodio),
}

impl AnyBackend {
    /// The forced/test backend.
    fn silent() -> Self {
        Self::Silent(Silent::default(), None)
    }

    /// What the daemon should use at startup: rodio unless `BO_BACKEND=silent`
    /// says otherwise, falling back to silence when no device can be opened.
    fn for_daemon() -> Self {
        if std::env::var("BO_BACKEND").as_deref() == Ok("silent") {
            return Self::silent();
        }
        match Rodio::try_new() {
            Ok(rodio) => Self::Rodio(rodio),
            Err(e) => Self::Silent(Silent::default(), Some(format!("no audio device: {e}"))),
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Self::Silent(..) => "silent",
            Self::Rodio(..) => "rodio",
        }
    }

    /// Why playback is silent, when it is a fallback rather than a choice.
    fn note(&self) -> Option<&str> {
        match self {
            Self::Silent(_, note) => note.as_deref(),
            Self::Rodio(..) => None,
        }
    }
}

impl Backend for AnyBackend {
    fn play(&mut self, tracks: &[Track], at: Duration) -> Result<(), BackendError> {
        match self {
            Self::Silent(backend, _) => backend.play(tracks, at),
            Self::Rodio(backend) => backend.play(tracks, at),
        }
    }

    fn pause(&mut self) {
        match self {
            Self::Silent(backend, _) => backend.pause(),
            Self::Rodio(backend) => backend.pause(),
        }
    }

    fn resume(&mut self) {
        match self {
            Self::Silent(backend, _) => backend.resume(),
            Self::Rodio(backend) => backend.resume(),
        }
    }

    fn stop(&mut self) {
        match self {
            Self::Silent(backend, _) => backend.stop(),
            Self::Rodio(backend) => backend.stop(),
        }
    }

    fn set_volume(&mut self, volume: f32) {
        match self {
            Self::Silent(backend, _) => backend.set_volume(volume),
            Self::Rodio(backend) => backend.set_volume(volume),
        }
    }
}

/// Where the daemon listens by default: `$TMPDIR/bo/daemon.sock`.
fn default_socket() -> PathBuf {
    std::env::temp_dir().join("bo").join("daemon.sock")
}

/// Hand-written top-level help: clap renders subcommands as one flat list,
/// so grouping (Arrangement / Mix / Transport) and the examples live here.
/// Keep in sync with [`Command`] when the surface changes.
const HELP: &str = "\
bo — edit and mix audio, one command at a time

USAGE
  bo [--socket PATH] <command> [args...]

COMMANDS

Arrangement:
  put <spec> [where]       place a clip; where = track[@pos] — omit the track
                           for a fresh one and the pos for the playhead
                           (--repeat n places n butt-joined copies)
  take <track> <clip>      remove a clip — by its id, or the @timecode it
                           covers
  ls                       dump the arrangement; a key: value status block,
                           then one key=value line per track and clip
  at <t>                   show what plays at track time t
  render [file] [from-to]  mix the arrangement to a wav file; a range
                           renders only that span (from- to the end)
                           --measure reports peak/RMS/true peak and EBU
                           R128 loudness; omit the file to measure only
  save <file>              write the arrangement as a script
  load <file>              replace the arrangement from a script
  reset                    drop every track and stop; back to a fresh
                           session
  check                    verify every source is readable
  probe [uri]              measure a source's length; without a uri, every
                           source in the arrangement

Mix:
  set master <v>           set the master gain, 0..1 (real-time)
  set track.N.volume <v>   set a track's gain, 0..1 (apply to land)
  set track.N.muted <b>    mute (true) or restore (false) a track
  set track.N.name <name>  label a track

Transport:
  play                     start playback from the current playhead
                           (refused when there is nothing to play)
  pause                    hold position
  resume                   continue after a pause
  stop                     stop, rewind, end the session
  seek <t>                 move the playhead (refused past the end)
  apply                    rebuild the running transport, so pending mix
                           changes take effect now

OPTIONS
  --socket PATH            unix socket the daemon listens on
                           (default: $TMPDIR/bo/daemon.sock)
  -h, --help               show this help
  -V, --version            print version

CLIP SPEC
  uri[,from-to]            from-to = slice of the source (default: whole,
                           resolved by probing); `,` marks the slice
  track[@pos]              track = index (created on demand), pos = where on
                           the track (default: playhead); `@` marks position
  Timecodes: SS, MM:SS or HH:MM:SS, optional .fff fraction. `:` is reserved
  for timecodes.

  A source with no out-point is probed at put time and its whole length is
  used, so every clip has a known finite end; a source that cannot be
  measured is refused (run `bo probe <uri>`). Spans are half-open: clips
  may butt-join (one ends exactly where the next starts).

EXAMPLES
  bo put bed.wav,00:00:00-00:00:30
  bo put voice.wav,00:00:00-00:00:30 1@00:00:00
  bo set track.0.volume 0.4  # duck the bed under the voice
  bo apply                 # make the change audible now
  bo play
  bo ls
  bo stop                  # end the session; daemon cleans up
";

/// Names of the user-facing subcommands: `bo <name> --help` must keep
/// clap's own per-command help, while `bo --help` shows the grouped [`HELP`].
const SUBCOMMAND_NAMES: [&str; 17] = [
    "put", "take", "ls", "at", "render", "save", "load", "reset", "check", "probe", "play", "pause",
    "resume", "stop", "seek", "apply", "set",
];

/// Tokenize a wire or script line: whitespace-separated words with
/// shell-style quoting (`'...'`, `"..."`, backslash escapes). Wraps
/// `shlex::split`; an unterminated quote is an error, not silence.
fn tokenize(line: &str) -> Result<Vec<String>, String> {
    shlex::split(line).ok_or_else(|| "unterminated quote".to_string())
}

/// Serialize one argument for the wire or a script, quoting it when it
/// contains whitespace or quote characters. Round-trips through
/// [`tokenize`].
fn quote_arg(arg: &str) -> String {
    // NUL cannot appear in argv; this branch is unreachable in practice.
    shlex::try_quote(arg).map_or_else(|_| arg.to_string(), |q| q.into_owned())
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

/// Format a duration as `HH:MM:SS.fff` (delegates to [`reply::Tc`]).
fn format_time(d: Duration) -> String {
    Tc(d).to_string()
}

/// The client's working directory, as a string; empty when it cannot be read.
fn current_cwd() -> String {
    std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Resolve a path against `cwd`: absolute paths and scheme-bearing URIs
/// (`http://…`) are left alone, and so is anything when `cwd` is empty. Every
/// relative local path in a command resolves against the cwd of the `bo`
/// invocation that issued it, never the daemon's.
fn absolutize(path: &str, cwd: &str) -> String {
    let p = std::path::Path::new(path);
    if p.is_absolute() || path.contains("://") || cwd.is_empty() {
        return path.to_string();
    }
    std::path::Path::new(cwd)
        .join(p)
        .to_string_lossy()
        .into_owned()
}

/// Parse a clip spec into its pieces: `uri[,from-to]`. `,` marks the slice;
/// `:` is reserved for timecodes. Position is not part of the spec — it lives
/// in the placement argument (`track[@pos]`).
fn parse_spec(s: &str) -> Result<Spec, String> {
    let s = s.trim();
    let mut spec = Spec {
        uri: String::new(),
        from: Duration::ZERO,
        to: None,
    };
    match s.split_once(',') {
        Some((uri, slice)) => {
            spec.uri = uri.trim().to_string();
            let (from, to) = parse_slice(slice)?;
            spec.from = from;
            spec.to = to;
        }
        None => spec.uri = s.to_string(),
    }
    if spec.uri.is_empty() {
        return Err("missing uri in clip spec".to_string());
    }
    Ok(spec)
}

/// Parse the `from-to` part of a slice: `from-to`, or `from-` to the source's
/// end.
fn parse_slice(s: &str) -> Result<(Duration, Option<Duration>), String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("missing slice after ','".to_string());
    }
    match s.split_once('-') {
        Some((from, to)) => {
            let from = parse_timecode(from)?;
            let to = if to.trim().is_empty() {
                None
            } else {
                Some(parse_timecode(to)?)
            };
            Ok((from, to))
        }
        None => Err(format!("bad slice {s:?}: expected from-to")),
    }
}

/// Parse a placement: `track[@pos]`. `track` is a track index (created on
/// demand); `pos` is a track timecode. Either may be omitted: an omitted
/// track means a fresh one, an omitted pos means the playhead.
fn parse_placement(s: &str) -> Result<(Option<usize>, Option<Duration>), String> {
    let s = s.trim();
    if s.is_empty() {
        return Ok((None, None));
    }
    match s.split_once('@') {
        Some((track, pos)) => {
            let track = if track.is_empty() {
                None
            } else {
                Some(
                    track
                        .parse::<usize>()
                        .map_err(|_| format!("bad track {track:?}"))?,
                )
            };
            let pos = if pos.is_empty() {
                None
            } else {
                Some(parse_timecode(pos)?)
            };
            Ok((track, pos))
        }
        None => {
            let track = s.parse::<usize>().map_err(|_| format!("bad track {s:?}"))?;
            Ok((Some(track), None))
        }
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
fn dispatch(a: &mut Arrangement, command: Command, cwd: &str) -> Result<Output, (i32, String)> {
    match command {
        Command::Put { spec, placement, repeat } => put_command(a, &spec, placement.as_deref(), repeat, cwd),
        Command::Play => play_command(a),
        Command::Ls => Ok(Output::Ls(arrangement_view(a))),
        Command::At { at } => at_command(a, &at),
        Command::Pause => {
            a.player.pause();
            Ok(Output::Paused { at: a.player.playhead() })
        }
        Command::Resume => {
            a.player.resume().map_err(|e| fail(e.to_string()))?;
            Ok(Output::Resumed { at: a.player.playhead() })
        }
        Command::Stop => {
            a.player.stop();
            Ok(Output::Stopped)
        }
        Command::Seek { at } => {
            let t = parse_timecode(&at).map_err(usage)?;
            if t > a.player.duration() {
                return Err(fail(format!(
                    "refused: cannot seek to {} — the arrangement ends at {}",
                    format_time(t),
                    format_time(a.player.duration())
                )));
            }
            a.player.seek(t).map_err(|e| fail(e.to_string()))?;
            Ok(Output::Seeked { at: t })
        }
        Command::Apply => apply_command(a),
        Command::Set { var, value } => set_command(a, &var, &value),
        Command::Take { track, clip } => take_command(a, track, &clip),
        Command::Render {
            file,
            range,
            measure,
        } => {
            let file = file.map(|f| absolutize(&f, cwd));
            let (from, to) = match range {
                Some(r) => parse_range(&r).map_err(usage)?,
                None => (Duration::ZERO, None),
            };
            match (file, measure) {
                // A bare measure of the whole arrangement.
                (None, true) => {
                    if a.player.duration() == Duration::ZERO {
                        return Err(fail("no clips: nothing to measure"));
                    }
                    let (duration, stats) =
                        render_and_measure(a.player.tracks(), None, from, to, a.player.volume())
                            .map_err(|e| fail(format!("render failed: {e}")))?;
                    Ok(Output::Rendered {
                        file: None,
                        duration,
                        stats: Some(stats),
                    })
                }
                (Some(file), measure) => {
                    // With --measure, a bare timecode in the file slot is
                    // almost certainly a mistyped range.
                    if measure
                        && file.contains('-')
                        && parse_range(&file).is_ok()
                    {
                        return Err(usage(format!(
                            "{file:?} looks like a range; `render --measure` measures the whole \
                             arrangement — to measure a range, write it: `render out.wav {file} \
                             --measure`"
                        )));
                    }
                    let (duration, stats) = if measure {
                        render_and_measure(
                            a.player.tracks(),
                            Some(std::path::Path::new(&file)),
                            from,
                            to,
                            a.player.volume(),
                        )
                        .map(|(d, m)| (d, Some(m)))
                        .map_err(|e| fail(format!("render failed: {e}")))?
                    } else {
                        let d = render_to_file(
                            a.player.tracks(),
                            &file,
                            from,
                            to,
                            a.player.volume(),
                        )
                        .map_err(|e| fail(format!("render failed: {e}")))?;
                        (d, None)
                    };
                    Ok(Output::Rendered {
                        file: Some(file),
                        duration,
                        stats,
                    })
                }
                (None, false) => Err(usage(
                    "render needs a wav path, or --measure to measure without writing",
                )),
            }
        }
        Command::Save { file } => {
            let file = absolutize(&file, cwd);
            std::fs::write(&file, serialize(a))
                .map_err(|e| fail(format!("cannot write {file}: {e}")))?;
            Ok(Output::Saved { file })
        }
        Command::Load { file } => {
            let file = absolutize(&file, cwd);
            let text = std::fs::read_to_string(&file)
                .map_err(|e| fail(format!("cannot read {file}: {e}")))?;
            // Stage the script in a silent arrangement so a failing script
            // leaves the current one untouched; then commit only the
            // arrangement data (tracks, master) into the live player. The
            // audio backend must survive a load — swapping the whole
            // arrangement once replaced the daemon's backend with a bare
            // silent one, so the next play was silently inaudible.
            let mut staged = Arrangement::default();
            run_script(&mut staged, &text, &file, cwd).map_err(fail)?;
            let volume = staged.player.volume();
            let tracks: Vec<Track> = staged.player.tracks().to_vec();
            a.player.reset();
            a.player.set_volume(volume);
            a.player.tracks_mut().extend(tracks);
            Ok(Output::Loaded { file })
        }
        Command::Reset => reset_command(a),
        Command::Check => {
            let clips: usize = a.player.tracks().iter().map(Track::len).sum();
            let problems = check_sources(a.player.tracks());
            if problems.is_empty() {
                Ok(Output::Check { clips, problems })
            } else {
                let out = Output::Check { clips, problems };
                Err((1, out.to_string()))
            }
        }
        Command::Probe { uri } => match uri {
            None => probe_arrangement(a),
            Some(uri) => probe_uri(&absolutize(&uri, cwd)),
        },
        // Help is handled locally by the client; this arm keeps a stray
        // "help" line over the socket harmless.
        Command::Help => Ok(Output::Text(HELP)),
        Command::Daemon => Err(fail("the daemon runs standalone, not over the socket")),
    }
}

/// The arrangement as data for `ls`: a `key: value` status block (state,
/// playhead, end, backend, master volume, track count), then one line per
/// track and per clip of `key=value` tokens. Row labels are stable
/// (`player`-level keys, `track N:`, `clip N:`), so parsers can grep by
/// prefix and keys never move position.
fn arrangement_view(a: &Arrangement) -> Ls {
    let p = &a.player;
    Ls {
        state: p.state(),
        playhead: p.playhead(),
        end: p.duration(),
        backend: p.backend().name(),
        volume: p.volume(),
        tracks: p
            .tracks()
            .iter()
            .map(|t| LsTrack {
                name: t.name().map(str::to_string),
                volume: t.volume(),
                muted: t.muted(),
                end: t.duration(),
                clips: t
                    .clips()
                    .iter()
                    .map(|c| LsClip {
                        id: c.id,
                        uri: c.source.uri.clone(),
                        at: c.at,
                        end: c.end(),
                        from: c.from,
                        src_to: c.to,
                    })
                    .collect(),
            })
            .collect(),
    }
}

/// `at <t>`: the mix at track time `t` — every clip covering that moment,
/// one per track, or a `silent at ...` line when nothing plays there.
fn at_command(a: &Arrangement, at_arg: &str) -> Result<Output, (i32, String)> {
    let t = parse_timecode(at_arg).map_err(usage)?;
    let active: Vec<AtLine> = a
        .player
        .tracks()
        .iter()
        .enumerate()
        .filter_map(|(ti, track)| {
            track.clips().iter().find(|c| c.covers(t)).map(|c| AtLine {
                track: ti,
                id: c.id,
                uri: c.source.uri.clone(),
                at: c.at,
                end: c.end(),
            })
        })
        .collect();
    Ok(Output::At { at: t, active })
}

/// `take <track> <clip>`: remove a clip from a track. The clip is addressed
/// by its stable id, or by an `@timecode` — the clip covering that moment,
/// unique per track by the non-overlap invariant.
fn take_command(
    a: &mut Arrangement,
    track_index: usize,
    clip_arg: &str,
) -> Result<Output, (i32, String)> {
    let t = a
        .player
        .tracks_mut()
        .get_mut(track_index)
        .ok_or_else(|| fail(format!("no track {track_index}")))?;
    let id = if let Ok(id) = clip_arg.parse::<u64>() {
        Some(id)
    } else if let Some(tc) = clip_arg.strip_prefix('@') {
        let at = parse_timecode(tc).map_err(usage)?;
        t.clip_at(at).map(|c| c.id)
    } else {
        return Err(usage(format!(
            "clip must be an id or @timecode, got {clip_arg:?}"
        )));
    };
    match id.and_then(|id| t.remove(id)) {
        Some(clip) => Ok(Output::Removed {
            track: track_index,
            id: clip.id,
            uri: clip.source.uri.clone(),
        }),
        None => Err(fail(format!("no clip {track_index}#{clip_arg}"))),
    }
}

/// `put <spec> [track[@pos]]`: place a clip, creating the track when needed.
///
/// The identifier in the output is the contract: an implicit put creates a
/// fresh track and prints its index; an explicit one names a track, created on
/// demand so that repeated puts rebuild the same layout. With `--repeat n`,
/// places n butt-joined copies as ordinary clips; the whole batch is checked
/// before any insert, so a collision refuses everything.
fn put_command(
    a: &mut Arrangement,
    spec_arg: &str,
    placement_arg: Option<&str>,
    repeat: Option<u32>,
    cwd: &str,
) -> Result<Output, (i32, String)> {
    let repeat = repeat.unwrap_or(1);
    if repeat == 0 {
        return Err(usage("repeat must be at least 1"));
    }
    let mut spec = parse_spec(spec_arg).map_err(usage)?;
    // Resolve the source against this command's cwd: the daemon is an
    // implementation detail and must not affect where relative paths land.
    spec.uri = absolutize(&spec.uri, cwd);
    // A clip with no out-point plays to the source's end; resolve that end
    // now by probing, so every clip has a known finite length. Unmeasurable
    // sources are refused here instead of becoming a clip that blocks the
    // track and can never play.
    let to = match spec.to {
        Some(to) => to,
        None => probe(&spec.uri).map_err(|e| {
            fail(format!(
                "refused: cannot place {}: {e}; run: bo probe {}",
                spec.uri, spec.uri
            ))
        })?,
    };
    let (want_track, want_at) = match placement_arg {
        Some(p) => parse_placement(p).map_err(usage)?,
        None => (None, None),
    };
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
    // Placement: an explicit pos wins; otherwise drop at the playhead.
    let base_at = want_at.unwrap_or_else(|| a.player.playhead());
    let slice_len = to.saturating_sub(spec.from);
    if repeat > 1 && slice_len == Duration::ZERO {
        return Err(usage("cannot repeat a zero-length slice"));
    }
    let clips: Vec<Clip> = (0..repeat)
        .map(|i| Clip::sliced(source.clone(), spec.from, to).at(base_at + slice_len * i))
        .collect();
    // Atomic: verify every copy fits before inserting any, so a collision
    // leaves no trace. The daemon holds the arrangement lock throughout.
    let track = &a.player.tracks()[track_index];
    for (i, c) in clips.iter().enumerate() {
        if let Some(conflict) = track.clips().iter().find(|x| x.overlaps(c)) {
            // Say where the conflict actually sits and where the clip could
            // go instead: half-open spans, so a butt-join against an end is
            // legal and the report says so by listing that end as free.
            let span = format!(
                "[{:.3},{:.3})",
                conflict.at.as_secs_f64(),
                conflict.end().as_secs_f64()
            );
            let next = track.next_free_start(c.at, c.duration());
            let hint = format!("next free start {:.3}s", next.as_secs_f64());
            return Err(fail(format!(
                "refused: copy {i} at {:.3}s collides with clip #{} {span}; {hint}",
                c.at.as_secs_f64(),
                conflict.id,
            )));
        }
    }
    let mut placed_clips = Vec::new();
    for c in clips {
        let id = a.player.tracks_mut()[track_index]
            .insert(c)
            .expect("pre-checked: the insert cannot collide");
        let placed = a.player.tracks()[track_index]
            .clips()
            .iter()
            .find(|x| x.id == id)
            .expect("the inserted clip is in the track");
        placed_clips.push(PlacedClip {
            id,
            uri: placed.source.uri.clone(),
            at: placed.at,
            from: placed.from,
            to: placed.to,
        });
    }
    Ok(Output::Put {
        track: track_index,
        clips: placed_clips,
    })
}

/// `play`: refuse an arrangement with nothing to play, then start playback
/// from the current playhead. The session line tells the caller what is
/// about to play; the daemon's clock loop advances the playhead and exits
/// when playback is done.
fn play_command(a: &mut Arrangement) -> Result<Output, (i32, String)> {
    // An empty arrangement (or one whose clips are all zero-length) would
    // finish instantly: refuse before touching the transport, so the daemon
    // neither fakes success nor tears itself down.
    if a.player.duration() == Duration::ZERO {
        return Err(fail("no clips: nothing to play"));
    }
    let tracks = a.player.tracks().iter().filter(|t| !t.is_empty()).count();
    let clips: usize = a.player.tracks().iter().map(Track::len).sum();
    let end = a.player.duration();
    let backend = a.player.backend().name();
    a.player.play().map_err(|e| fail(e.to_string()))?;
    let playhead = a.player.playhead();
    let note = a.player.backend().note().map(str::to_string);
    Ok(Output::Session {
        tracks,
        clips,
        end,
        backend,
        playhead,
        note,
    })
}

/// `apply`: rebuild the running transport from the current playhead, so
/// pending mix changes (volume, mute) take effect now.
fn apply_command(a: &mut Arrangement) -> Result<Output, (i32, String)> {
    if !a.player.is_playing() {
        return Ok(Output::Applied { rebuilt: None });
    }
    a.player.apply().map_err(|e| fail(e.to_string()))?;
    Ok(Output::Applied {
        rebuilt: Some(a.player.playhead()),
    })
}

/// `reset`: drop every track and stop the transport — the daemon is back to
/// its fresh state, ready for a session script to rebuild the arrangement.
fn reset_command(a: &mut Arrangement) -> Result<Output, (i32, String)> {
    let tracks = a.player.tracks().len();
    a.player.reset();
    Ok(Output::Reset { tracks })
}

/// `set <var> <value>`: set an attribute. `master` is real-time — the backend
/// is told immediately. `track.N.volume` / `track.N.muted` / `track.N.name`
/// are arrangement data that land on the next `play` or `apply`.
fn set_command(a: &mut Arrangement, var: &str, value: &str) -> Result<Output, (i32, String)> {
    match var {
        "master" => {
            let v: f32 = value
                .parse()
                .map_err(|_| usage(format!("bad gain {value:?}")))?;
            a.player.set_volume(v);
            Ok(Output::Set(SetResult::Master { v: a.player.volume() }))
        }
        _ => {
            let (index, prop) = var
                .strip_prefix("track.")
                .and_then(|rest| rest.split_once('.'))
                .ok_or_else(|| usage(format!("unknown var {var:?}")))?;
            let index: usize = index
                .parse()
                .map_err(|_| usage(format!("bad track {index:?}")))?;
            let t = a
                .player
                .tracks_mut()
                .get_mut(index)
                .ok_or_else(|| fail(format!("no track {index}")))?;
            match prop {
                "volume" => {
                    let v: f32 = value
                        .parse()
                        .map_err(|_| usage(format!("bad gain {value:?}")))?;
                    t.set_volume(v);
                    Ok(Output::Set(SetResult::TrackVolume { i: index, v: t.volume() }))
                }
                "muted" => {
                    let b = parse_bool(value).map_err(usage)?;
                    t.set_muted(b);
                    Ok(Output::Set(SetResult::TrackMuted { i: index, muted: b }))
                }
                "name" => {
                    t.set_name(value.to_string());
                    Ok(Output::Set(SetResult::TrackName {
                        i: index,
                        name: value.to_string(),
                    }))
                }
                _ => Err(usage(format!("unknown property {prop:?} on a track"))),
            }
        }
    }
}

/// Parse a boolean value: `true`/`false` (or `1`/`0`).
fn parse_bool(s: &str) -> Result<bool, String> {
    match s.trim() {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        _ => Err(format!("bad boolean {s:?}: true or false")),
    }
}

/// `probe <uri>`: measure one source. Used both locally (no daemon) and over
/// the wire.
fn probe_uri(uri: &str) -> Result<Output, (i32, String)> {
    match probe(uri) {
        Ok(d) => Ok(Output::Probed {
            uri: uri.to_string(),
            duration: d,
        }),
        Err(e) => Err(fail(e)),
    }
}

/// `probe` with no uri: measure every distinct source in the arrangement.
/// Lists each source's length; exit 1 if any source cannot be measured.
fn probe_arrangement(a: &Arrangement) -> Result<Output, (i32, String)> {
    let sources: Vec<ProbeResult> = probe_sources(a.player.tracks())
        .into_iter()
        .map(|(uri, outcome)| ProbeResult { uri, outcome })
        .collect();
    let has_problem = sources.iter().any(|s| s.outcome.is_err());
    let out = Output::ProbedMany { sources };
    if has_problem {
        Err((1, out.to_string()))
    } else {
        Ok(out)
    }
}

/// Parse a render range: `from-to`, `from-` (to the end), or empty for the
/// whole arrangement.
fn parse_range(s: &str) -> Result<(Duration, Option<Duration>), String> {
    let s = s.trim();
    if s.is_empty() {
        return Ok((Duration::ZERO, None));
    }
    if let Some((from, to)) = s.split_once('-') {
        let from = parse_timecode(from)?;
        let to = if to.trim().is_empty() {
            None
        } else {
            Some(parse_timecode(to)?)
        };
        if let Some(to) = to
            && to < from
        {
            return Err(format!("bad range {s:?}: to before from"));
        }
        Ok((from, to))
    } else {
        Err(format!("bad range {s:?}: expected from-to"))
    }
}

/// The command line the client puts on the wire, re-serialized from the
/// already-parsed subcommand. String arguments are quoted so that paths and
/// names with whitespace survive [`handle_line`]'s tokenizer.
fn command_line(command: &Command) -> String {
    match command {
        Command::Put { spec, placement, repeat } => {
            let mut line = String::from("put");
            if let Some(n) = repeat {
                let _ = write!(line, " --repeat {n}");
            }
            let _ = write!(line, " {}", quote_arg(spec));
            if let Some(p) = placement {
                let _ = write!(line, " {}", quote_arg(p));
            }
            line
        }
        Command::Play => "play".to_string(),
        Command::Ls => "ls".to_string(),
        Command::At { at } => format!("at {}", quote_arg(at)),
        Command::Pause => "pause".to_string(),
        Command::Resume => "resume".to_string(),
        Command::Stop => "stop".to_string(),
        Command::Seek { at } => format!("seek {}", quote_arg(at)),
        Command::Apply => "apply".to_string(),
        Command::Set { var, value } => format!("set {} {}", quote_arg(var), quote_arg(value)),
        Command::Take { track, clip } => format!("take {track} {}", quote_arg(clip)),
        Command::Render {
            file,
            range,
            measure,
        } => {
            let mut line = String::from("render");
            if let Some(f) = file {
                let _ = write!(line, " {}", quote_arg(f));
            }
            if let Some(r) = range {
                let _ = write!(line, " {}", quote_arg(r));
            }
            if *measure {
                line.push_str(" --measure");
            }
            line
        }
        Command::Save { file } => format!("save {}", quote_arg(file)),
        Command::Load { file } => format!("load {}", quote_arg(file)),
        Command::Reset => "reset".to_string(),
        Command::Check => "check".to_string(),
        Command::Probe { uri } => match uri {
            Some(uri) => format!("probe {}", quote_arg(uri)),
            None => "probe".to_string(),
        },
        Command::Help => unreachable!("help is handled locally, never sent"),
        Command::Daemon => unreachable!("the daemon is spawned, not sent"),
    }
}

/// The arrangement as a script: the commands that rebuild it. Every line is
/// a valid command, so `load` runs the file through the same parse and
/// dispatch. URIs and names are quoted so whitespace round-trips.
fn serialize(a: &Arrangement) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# bo arrangement v1");
    let _ = writeln!(out, "set master {}", a.player.volume());
    for (ti, t) in a.player.tracks().iter().enumerate() {
        if t.clips().is_empty() {
            continue; // empty tracks carry nothing worth saving
        }
        for c in t.clips() {
            let _ = writeln!(
                out,
                "put {},{}-{} {ti}@{}",
                quote_arg(&c.source.uri),
                format_time(c.from),
                format_time(c.to),
                format_time(c.at)
            );
        }
        if let Some(name) = t.name() {
            let _ = writeln!(out, "set track.{ti}.name {}", quote_arg(name));
        }
        let _ = writeln!(out, "set track.{ti}.volume {}", t.volume());
        if t.muted() {
            let _ = writeln!(out, "set track.{ti}.muted true");
        }
    }
    out
}

/// Execute a script (a `save`d arrangement) into the arrangement.
///
/// Stops at the first failing line and reports `src:line: message`; what ran
/// before the failure stays applied. `load` runs into a fresh arrangement,
/// so a failing script leaves the live one untouched.
fn run_script(a: &mut Arrangement, text: &str, src: &str, cwd: &str) -> Result<(), String> {
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let args = tokenize(line).map_err(|e| format!("{src}:{}: {e}", n + 1))?;
        let command = parse_command(&args)
            .map_err(|code| format!("{src}:{}: parse failed (exit {code})", n + 1))?;
        dispatch(a, command, cwd).map_err(|(_, msg)| format!("{src}:{}: {msg}", n + 1))?;
    }
    Ok(())
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
            // Top-level help is our grouped text; a subcommand's own --help
            // (some subcommand name on the line) stays clap's.
            if matches!(
                e.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
            ) && !args.iter().any(|a| SUBCOMMAND_NAMES.contains(&a.as_str()))
            {
                println!("{HELP}");
                return 0;
            }
            let _ = e.print();
            return e.exit_code();
        }
    };
    let cwd = current_cwd();
    let socket = match cli.socket {
        Some(s) => std::path::PathBuf::from(absolutize(&s.to_string_lossy(), &cwd)),
        None => default_socket(),
    };
    match cli.command {
        Command::Help => {
            println!("{HELP}");
            0
        }
        Command::Daemon => daemon_main(&socket),
        Command::Probe { uri: Some(uri) } => probe_client(&absolutize(&uri, &cwd)),
        command => client_main(&socket, &command),
    }
}

/// `probe <uri>` runs in the client: measuring a file needs no daemon or
/// device, so a bare uri is answered locally and spawns nothing.
fn probe_client(uri: &str) -> i32 {
    match probe_uri(uri) {
        Ok(out) => {
            print!("{out}");
            0
        }
        Err((code, msg)) => {
            eprintln!("bo: {msg}");
            code
        }
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
    let cwd = current_cwd();
    if let Err(e) = stream.write_all(format!("{cwd}\n{line}\n").as_bytes()) {
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
/// (cleaning up the socket) when playback finishes or is stopped.
fn daemon_main(socket: &Path) -> i32 {
    daemon_main_with(socket, AnyBackend::for_daemon())
}

/// The idle timeout from `BO_IDLE_TIMEOUT` seconds (default 600; 0 disables):
/// a non-playing daemon that receives no commands for this long exits and
/// cleans up its socket.
fn idle_timeout() -> Duration {
    let default = Duration::from_secs(600);
    match std::env::var("BO_IDLE_TIMEOUT") {
        Ok(v) => v.parse().map(Duration::from_secs).unwrap_or(default),
        Err(_) => default,
    }
}

/// The daemon over a specific backend; tests pass a silent one so no audio
/// device is ever opened.
fn daemon_main_with(socket: &Path, backend: AnyBackend) -> i32 {
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
    let state = Arc::new(Mutex::new(Arrangement::with_backend(backend)));
    let exit = Arc::new(AtomicBool::new(false));
    // Every served command resets this clock; a non-playing daemon that stays
    // quiet for `BO_IDLE_TIMEOUT` seconds cleans itself up.
    let idle = Arc::new(Mutex::new(Instant::now()));
    let timeout = idle_timeout();

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
            a.player.advance(dt);
            if a.player.state() == State::Playing {
                a.player.is_finished()
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

/// Accept connections and handle each command on its own thread. The clock
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
            let (code, output, ends_session) = handle_line(&state, line.trim_end(), &cwd);
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

/// One command over the wire: parse, dispatch, and frame the reply. The third
/// value says whether this command ends the daemon's session.
fn handle_line(state: &Mutex<Arrangement>, line: &str, cwd: &str) -> (i32, String, bool) {
    let args = match tokenize(line) {
        Ok(args) => args,
        Err(e) => return (2, format!("bo: {e}\n"), false),
    };
    let command = match parse_command(&args) {
        Ok(command) => command,
        Err(e) => return (e.exit_code(), format!("{}\n", e.to_string().trim_end()), false),
    };
    let ends_session = matches!(command, Command::Stop);
    let mut a = state.lock().unwrap();
    match dispatch(&mut a, command, cwd) {
        Ok(out) => (0, out.to_string(), ends_session),
        Err((code, msg)) => (code, format!("bo: {msg}\n"), ends_session),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bo::engine::{BackendEvent, State};
    use rodio::Source as _;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn run_ok(a: &mut Arrangement, args: &[&str]) -> String {
        let v: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        match parse_command(&v).map_err(|e| (e.exit_code(), String::new())).and_then(|c| dispatch(a, c, "")) {
            Ok(out) => out.to_string(),
            Err((code, msg)) => panic!("command {args:?} failed ({code}): {msg}"),
        }
    }

    fn run_err(a: &mut Arrangement, args: &[&str]) -> (i32, String) {
        let v: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        parse_command(&v)
            .map_err(|e| (e.exit_code(), String::new()))
            .and_then(|c| dispatch(a, c, ""))
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

    /// A tiny mono PCM wav with a sine at `amp` amplitude.
    fn write_test_wav(path: &std::path::Path, seconds: f32, amp: f32) {
        let rate = 44_100u32;
        let n = (rate as f32 * seconds) as usize;
        let mut data = Vec::with_capacity(n * 2);
        for i in 0..n {
            let v = (amp
                * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / rate as f32).sin()
                * 32767.0) as i16;
            data.extend_from_slice(&v.to_le_bytes());
        }
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&rate.to_le_bytes());
        wav.extend_from_slice(&(rate * 2).to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
        wav.extend_from_slice(&data);
        std::fs::write(path, wav).unwrap();
    }

    fn send(socket: &Path, line: &str) -> String {
        let mut stream = UnixStream::connect(socket).unwrap();
        stream.write_all(format!("\n{line}\n").as_bytes()).unwrap();
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
    fn wire_words_parse_with_shell_quoting() {
        assert_eq!(tokenize("put a.wav 0").unwrap(), ["put", "a.wav", "0"]);
        assert_eq!(
            tokenize("put '/tmp/Bo FM.wav',00:00:00-00:00:10 0").unwrap(),
            ["put", "/tmp/Bo FM.wav,00:00:00-00:00:10", "0"]
        );
        assert_eq!(tokenize("set track.3.name \"bed soft\"").unwrap(), ["set", "track.3.name", "bed soft"]);
        assert_eq!(tokenize("put a\\ b.wav").unwrap(), ["put", "a b.wav"]);
        assert!(tokenize("put 'unterminated").is_err());
        // quote round-trips any argument, including quotes and backslashes.
        for arg in [
            "plain.wav",
            "/tmp/Bo FM.wav",
            "it's",
            "a\"b\\c",
            "bed-soft",
            "00:00:00.000",
        ] {
            let quoted = quote_arg(arg);
            assert_eq!(
                tokenize(&quoted).unwrap(),
                vec![arg.to_string()],
                "quote({arg:?}) = {quoted:?}"
            );
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
        assert_eq!(
            (s.uri.as_str(), s.from, s.to),
            ("a.wav", Duration::ZERO, None)
        );

        let s = parse_spec("a.wav,00:00:30-00:00:45").unwrap();
        assert_eq!(
            (s.uri.as_str(), s.from, s.to),
            ("a.wav", Duration::from_secs(30), Some(Duration::from_secs(45)))
        );

        let s = parse_spec("a.wav,00:00:30-").unwrap();
        assert_eq!(s.to, None);

        let s = parse_spec("my-file.wav,00:30-00:45").unwrap();
        assert_eq!(s.uri, "my-file.wav");
        assert_eq!(s.from, Duration::from_secs(30));

        assert!(parse_spec("").is_err());
        assert!(parse_spec("a.wav,bogus").is_err());
        assert!(parse_spec("a.wav,").is_err());
    }

    #[test]
    fn placements_parse() {
        assert_eq!(parse_placement("0").unwrap(), (Some(0), None));
        assert_eq!(
            parse_placement("0@00:00:10").unwrap(),
            (Some(0), Some(Duration::from_secs(10)))
        );
        assert_eq!(
            parse_placement("@00:00:10").unwrap(),
            (None, Some(Duration::from_secs(10)))
        );
        assert_eq!(parse_placement("0@").unwrap(), (Some(0), None));
        assert_eq!(parse_placement("").unwrap(), (None, None));
        assert!(parse_placement("abc").is_err());
        assert!(parse_placement("0@bogus").is_err());
    }

    #[test]
    fn relative_paths_resolve_against_the_command_cwd() {
        assert_eq!(absolutize("a.wav", "/srv"), "/srv/a.wav");
        assert_eq!(absolutize("/abs/a.wav", "/srv"), "/abs/a.wav");
        assert_eq!(absolutize("http://x/y.mp3", "/srv"), "http://x/y.mp3");
        // An empty cwd leaves relative paths relative (used by unit tests).
        assert_eq!(absolutize("a.wav", ""), "a.wav");

        // dispatch threads the cwd through to put, so the stored uri is
        // absolute regardless of the daemon's own cwd.
        let mut a = Arrangement::default();
        let args: Vec<String> = ["put".into(), "a.wav,0-1".into()].to_vec();
        let cmd = parse_command(&args).unwrap();
        dispatch(&mut a, cmd, "/srv").unwrap();
        assert_eq!(a.player.tracks()[0].clips()[0].source.uri, "/srv/a.wav");
    }

    #[test]
    fn put_creates_tracks_and_returns_identifiers() {
        let mut a = Arrangement::default();
        let out = run_ok(&mut a, &["put", "a.wav,00:00:00-00:00:10"]);
        assert!(out.contains("track 0"), "{out}");
        let out = run_ok(&mut a, &["put", "b.wav,00:00:00-00:00:10"]);
        assert!(out.contains("track 1"), "each implicit put gets a fresh track: {out}");
        assert_eq!(a.player.tracks().len(), 2);

        // The printed identifier names a track for later puts.
        let out = run_ok(&mut a, &["put", "c.wav,00:00:00-00:00:05", "0@00:00:10"]);
        assert!(out.contains("track 0"), "{out}");
        let t = &a.player.tracks()[0];
        assert_eq!(t.clips().len(), 2);
        assert_eq!(t.clips()[1].at, Duration::from_secs(10), "butt-joined on the named track");
    }

    #[test]
    fn a_named_track_is_created_on_demand() {
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", "a.wav,00:00:00-00:00:10", "3"]);
        assert_eq!(a.player.tracks().len(), 4, "tracks up to the index are created");
        assert_eq!(a.player.tracks()[3].clips().len(), 1);
        assert!(a.player.tracks()[0].is_empty());
    }

    #[test]
    fn refused_put_names_the_conflict_span_and_next_free_start() {
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", "a.wav,00:00:00-00:00:10"]);
        // Landing on the resident clip: the report shows its half-open span
        // and that a butt-join at its end is the next legal start.
        let (code, msg) = run_err(&mut a, &["put", "b.wav,00:00:00-00:00:10", "0@00:00:05"]);
        assert_eq!(code, 1);
        assert!(msg.contains("clip #0 [0.000,10.000)"), "{msg}");
        assert!(msg.contains("next free start 10.000s"), "{msg}");
    }

    #[test]
    fn put_without_a_slice_probes_and_refuses_unmeasurable_sources() {
        let mut a = Arrangement::default();
        let (code, msg) = run_err(&mut a, &["put", "live.wav"]);
        assert_eq!(code, 1);
        assert!(msg.contains("run: bo probe live.wav"), "{msg}");
        assert_eq!(a.player.tracks().len(), 0, "a refused put leaves no track");
    }

    #[test]
    fn play_starts_transport() {
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", "a.wav,00:00:00-00:00:10"]);
        let out = run_ok(&mut a, &["play"]);
        assert!(out.contains("playing from 00:00:00.000"), "{out}");
        assert_eq!(a.player.state(), State::Playing);
    }

    #[test]
    fn play_refuses_an_empty_arrangement() {
        let mut a = Arrangement::default();
        let (code, msg) = run_err(&mut a, &["play"]);
        assert_eq!(code, 1);
        assert!(msg.contains("no clips: nothing to play"), "{msg}");
        assert_eq!(a.player.state(), State::Stopped, "the transport is untouched");

        // An arrangement whose clips are all zero-length is equally empty.
        run_ok(&mut a, &["put", "a.wav,00:00:00-00:00:00"]);
        let (code, msg) = run_err(&mut a, &["play"]);
        assert_eq!(code, 1);
        assert!(msg.contains("no clips"), "{msg}");
    }

    #[test]
    fn play_reports_the_session_before_starting() {
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", "a.wav,00:00:00-00:00:10"]);
        run_ok(&mut a, &["put", "b.wav,00:00:00-00:00:05", "0@00:00:10"]);
        run_ok(&mut a, &["put", "c.wav,00:00:00-00:00:03"]);
        let out = run_ok(&mut a, &["play"]);
        let session = out.lines().next().unwrap();
        assert!(session.contains("session: 2 tracks | 3 clips"), "{out}");
        assert!(session.contains("ends 00:00:15.000"), "{out}");
        assert!(session.contains("backend"), "{out}");
        assert!(
            out.find("session:").unwrap() < out.find("playing from").unwrap(),
            "the session line comes first: {out}"
        );
    }

    #[test]
    fn play_reports_a_silent_fallback_note() {
        let mut a = Arrangement::with_backend(AnyBackend::Silent(
            Silent::default(),
            Some("no audio device: x".to_string()),
        ));
        run_ok(&mut a, &["put", "a.wav,00:00:00-00:00:10"]);
        let out = run_ok(&mut a, &["play"]);
        assert!(out.contains("(no audio device: x)"), "{out}");
        assert!(run_ok(&mut a, &["ls"]).contains("backend: silent"), "ls names the backend");
    }

    #[test]
    fn ls_shows_the_arrangement() {
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", "a.wav,00:00:00-00:00:10"]);
        run_ok(&mut a, &["put", "b.wav,00:00:00-00:00:05", "0@00:00:10"]);
        let out = run_ok(&mut a, &["ls"]);
        assert!(out.contains("state: stopped"), "{out}");
        for key in ["playhead:", "end:", "backend:", "volume:", "tracks:"] {
            assert!(out.contains(key), "missing {key}: {out}");
        }
        assert!(out.contains("track 0: volume=1.00 clips=2"), "{out}");
        assert!(out.contains("a.wav") && out.contains("b.wav"), "{out}");
        let out = run_ok(&mut a, &["ls"]);
        assert!(out.contains("at=00:00:10.000 end=00:00:15.000"), "butt-joined clip: {out}");
    }

    #[test]
    fn set_writes_track_volume_and_master() {
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", "a.wav,00:00:00-00:00:10"]);
        let out = run_ok(&mut a, &["set", "track.0.volume", "0.5"]);
        assert!(out.contains("track 0 volume 0.50"), "{out}");
        assert_eq!(a.player.tracks()[0].volume(), 0.5);
        run_ok(&mut a, &["set", "track.0.volume", "2.5"]);
        assert_eq!(a.player.tracks()[0].volume(), 1.0, "clamped");
        let (code, msg) = run_err(&mut a, &["set", "track.9.volume", "0.5"]);
        assert_eq!(code, 1);
        assert!(msg.contains("no track 9"), "{msg}");
        assert!(run_ok(&mut a, &["ls"]).contains("volume=1.00"), "ls shows the gain");

        // Master is real-time and clamped too.
        let out = run_ok(&mut a, &["set", "master", "0.78"]);
        assert!(out.contains("master 0.78"), "{out}");
        assert_eq!(a.player.volume(), 0.78);
        let (code, _) = run_err(&mut a, &["set", "master", "abc"]);
        assert_eq!(code, 2);
        let (code, _) = run_err(&mut a, &["set", "track.0.volume", "abc"]);
        assert_eq!(code, 2);
    }

    #[test]
    fn put_repeat_places_butt_joined_copies() {
        let mut a = Arrangement::default();
        let out = run_ok(&mut a, &["put", "--repeat", "3", "crackle.wav,00:00:00-00:00:12"]);
        assert_eq!(out.matches("ok: track 0 clip #").count(), 3, "{out}");
        let t = &a.player.tracks()[0];
        assert_eq!(t.clips().len(), 3);
        assert_eq!(t.clips()[0].at, Duration::ZERO);
        assert_eq!(t.clips()[1].at, Duration::from_secs(12));
        assert_eq!(t.clips()[2].at, Duration::from_secs(24));
        assert_eq!(
            (t.clips()[0].id, t.clips()[1].id, t.clips()[2].id),
            (0, 1, 2),
            "each copy is an ordinary clip with its own id"
        );
        // Copies are independent: removing one leaves the others.
        let out = run_ok(&mut a, &["take", "0", "1"]);
        assert!(out.contains("removed track 0 clip #1"), "{out}");
        assert_eq!(a.player.tracks()[0].clips().len(), 2);
    }

    #[test]
    fn put_repeat_is_atomic_and_refuses_bad_input() {
        let mut a = Arrangement::default();
        // a occupies 15s..25s; copy 3 of a 5s slice lands at 15s and collides.
        run_ok(&mut a, &["put", "a.wav,00:00:00-00:00:10", "@00:00:15"]);
        let (code, msg) = run_err(&mut a, &["put", "--repeat", "4", "b.wav,00:00:00-00:00:05", "0"]);
        assert_eq!(code, 1);
        assert!(msg.contains("copy 3") && msg.contains("collides"), "{msg}");
        assert_eq!(a.player.tracks()[0].clips().len(), 1, "no trace");

        // A source that cannot be measured is refused.
        let (code, msg) = run_err(&mut a, &["put", "--repeat", "2", "live.wav"]);
        assert_eq!(code, 1);
        assert!(msg.contains("run: bo probe live.wav"), "{msg}");

        // Zero copies and zero-length slices are nonsense.
        let (code, _) = run_err(&mut a, &["put", "--repeat", "0", "c.wav,00:00:00-00:00:10"]);
        assert_eq!(code, 2);
        let (code, _) = run_err(&mut a, &["put", "--repeat", "2", "c.wav,00:00:00-00:00:00"]);
        assert_eq!(code, 2);
    }

    #[test]
    fn at_shows_the_clips_covering_a_timecode() {
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", "a.wav,00:00:00-00:00:10"]);
        run_ok(&mut a, &["put", "b.wav,00:00:00-00:00:05", "0@00:00:10"]);
        run_ok(&mut a, &["put", "c.wav,00:00:00-00:00:03"]);

        let out = run_ok(&mut a, &["at", "00:00:01.000"]);
        assert!(out.contains("track 0: clip=0"), "{out}");
        assert!(out.contains("track 1: clip=0"), "{out}");
        let out = run_ok(&mut a, &["at", "00:00:12.000"]);
        assert!(out.contains("track 0: clip=1"), "{out}");
        assert!(!out.contains("track 1"), "{out}");
        let out = run_ok(&mut a, &["at", "00:00:20.000"]);
        assert!(out.contains("silent at 00:00:20.000"), "{out}");
        let (code, _) = run_err(&mut a, &["at", "bogus"]);
        assert_eq!(code, 2);
    }

    #[test]
    fn apply_rebuilds_a_running_transport_only() {
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", "a.wav,00:00:00-00:00:10"]);
        let plays = |a: &Arrangement| -> usize {
            match a.player.backend() {
                AnyBackend::Silent(s, _) => s
                    .events
                    .iter()
                    .filter(|e| **e == BackendEvent::Play)
                    .count(),
                AnyBackend::Rodio(_) => unreachable!("tests use the silent backend"),
            }
        };

        // Stopped: a note, and nothing is rebuilt.
        let out = run_ok(&mut a, &["apply"]);
        assert!(out.contains("not playing"), "{out}");
        assert_eq!(plays(&a), 0);

        // Playing: rebuild from the current playhead; playhead untouched.
        run_ok(&mut a, &["play"]);
        run_ok(&mut a, &["seek", "00:00:04"]);
        run_ok(&mut a, &["set", "track.0.volume", "0.5"]);
        let out = run_ok(&mut a, &["apply"]);
        assert!(out.contains("rebuilt from 00:00:04.000"), "{out}");
        assert_eq!(plays(&a), 3, "play + seek + apply");
        assert_eq!(
            a.player.playhead(),
            Duration::from_secs(4),
            "apply does not move the playhead"
        );
    }

    #[test]
    fn reset_clears_every_track_and_transport() {
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", "a.wav,00:00:00-00:00:10"]);
        run_ok(&mut a, &["put", "b.wav,00:00:00-00:00:05"]);
        run_ok(&mut a, &["set", "track.0.name", "bed"]);
        run_ok(&mut a, &["play"]);
        let out = run_ok(&mut a, &["reset"]);
        assert!(out.contains("reset: 2 tracks removed"), "{out}");
        assert_eq!(a.player.tracks().len(), 0);
        assert_eq!(a.player.state(), State::Stopped);
        assert_eq!(a.player.playhead(), Duration::ZERO);
    }

    #[test]
    fn take_addresses_clips_by_id_and_timecode() {
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", "a.wav,00:00:00-00:00:10"]);
        run_ok(&mut a, &["put", "b.wav,00:00:00-00:00:05", "0@00:00:10"]);
        run_ok(&mut a, &["put", "c.wav,00:00:00-00:00:03"]);

        // By timecode: the clip covering 00:00:12 on track 0 is b (id 1).
        let out = run_ok(&mut a, &["take", "0", "@00:00:12"]);
        assert!(out.contains("removed track 0 clip #1 b.wav"), "{out}");

        // By id: the remaining a on track 0 is id 0.
        let out = run_ok(&mut a, &["take", "0", "0"]);
        assert!(out.contains("removed track 0 clip #0 a.wav"), "{out}");

        // A timecode nothing covers is a miss.
        let (code, msg) = run_err(&mut a, &["take", "0", "@00:00:30"]);
        assert_eq!(code, 1);
        assert!(msg.contains("no clip 0#@00:00:30"), "{msg}");

        // Neither an id nor a timecode is a usage error.
        let (code, _) = run_err(&mut a, &["take", "0", "xyz"]);
        assert_eq!(code, 2);
    }

    #[test]
    fn take_removes_a_clip_by_its_identifiers() {
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", "a.wav,00:00:00-00:00:10"]);
        run_ok(&mut a, &["put", "b.wav,00:00:00-00:00:05", "0@00:00:10"]);
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
    fn set_mutes_and_restores_a_track() {
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", "a.wav,00:00:00-00:00:10"]);
        let out = run_ok(&mut a, &["set", "track.0.muted", "true"]);
        assert!(out.contains("track 0 muted"), "{out}");
        assert!(a.player.tracks()[0].muted());
        let out = run_ok(&mut a, &["set", "track.0.muted", "false"]);
        assert!(out.contains("track 0 unmuted"), "{out}");
        assert!(!a.player.tracks()[0].muted());
        assert!(!run_ok(&mut a, &["ls"]).contains("muted"), "no marker when unmuted");
        run_ok(&mut a, &["set", "track.0.muted", "true"]);
        assert!(run_ok(&mut a, &["ls"]).contains("muted"), "ls shows the marker");
        let (code, msg) = run_err(&mut a, &["set", "track.9.muted", "true"]);
        assert_eq!(code, 1);
        assert!(msg.contains("no track 9"), "{msg}");
        let (code, msg) = run_err(&mut a, &["set", "track.9.muted", "false"]);
        assert_eq!(code, 1);
        assert!(msg.contains("no track 9"), "{msg}");
        let (code, _) = run_err(&mut a, &["set", "track.0.muted", "yes"]);
        assert_eq!(code, 2, "bad boolean is a usage error");
    }

    #[test]
    fn transport_commands_drive_the_state_machine() {
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", "a.wav,00:00:00-00:00:10"]);
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
    fn seek_past_the_end_is_refused() {
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", "a.wav,00:00:00-00:00:10"]);
        let (code, msg) = run_err(&mut a, &["seek", "99"]);
        assert_eq!(code, 1);
        assert!(msg.contains("ends at 00:00:10.000"), "{msg}");
        // The refusal leaves the playhead untouched.
        assert_eq!(a.player.playhead(), Duration::ZERO);

        // Seeking exactly to the end is allowed: it is the finished position.
        run_ok(&mut a, &["seek", "00:00:10"]);
        assert_eq!(a.player.playhead(), Duration::from_secs(10));
    }

    #[test]
    fn transport_commands_work_over_the_wire_and_stop_ends_the_session() {
        let dir = temp_dir();
        let socket = dir.join("d.sock");
        let handle = {
            let socket = socket.clone();
            thread::spawn(move || daemon_main_with(&socket, AnyBackend::silent()))
        };
        wait_until("socket", || UnixStream::connect(&socket).is_ok());

        send(&socket, "put a.wav,00:00:00-00:00:10");
        let reply = send(&socket, "ls");
        assert!(reply.contains("track 0: volume=1.00 clips=1") && reply.contains("a.wav"), "{reply}");

        let reply = send(&socket, "set track.0.volume 0.5");
        assert!(reply.contains("track 0 volume 0.50"), "{reply}");
        let reply = send(&socket, "set track.0.muted true");
        assert!(reply.contains("track 0 muted"), "{reply}");
        let reply = send(&socket, "set track.0.muted false");
        assert!(reply.contains("track 0 unmuted"), "{reply}");
        let reply = send(&socket, "take 0 0");
        assert!(reply.contains("removed track 0 clip #0"), "{reply}");
        let reply = send(&socket, "ls");
        assert!(!reply.contains("a.wav"), "the clip is gone: {reply}");

        // save the (now empty) arrangement, restore it, and re-fill a clip.
        let script = dir.join("prog.bo");
        let sp = script.to_string_lossy().into_owned();
        let reply = send(&socket, &format!("save {sp}"));
        assert!(reply.contains("saved"), "{reply}");
        send(&socket, "put a.wav,00:00:00-00:00:10");
        let reply = send(&socket, &format!("load {sp}"));
        assert!(reply.contains("loaded"), "{reply}");
        let reply = send(&socket, "ls");
        assert!(!reply.contains("a.wav"), "load replaced the arrangement: {reply}");

        // Refill the empty arrangement, name the track, and check the source.
        send(&socket, "put a.wav,00:00:00-00:00:10");
        let reply = send(&socket, "set track.0.name bed");
        assert!(reply.contains("named \"bed\""), "{reply}");
        // a.wav does not exist, so check must report it.
        let reply = send(&socket, "check");
        assert_eq!(reply.lines().next().unwrap(), "1", "{reply}");
        assert!(reply.contains("cannot open a.wav"), "{reply}");

        let reply = send(&socket, "seek 00:00:05");
        assert!(reply.contains("playhead at 00:00:05.000"), "{reply}");
        let reply = send(&socket, "pause");
        assert!(reply.contains("paused at"), "{reply}");
        let reply = send(&socket, "resume");
        assert!(reply.contains("playing from"), "{reply}");
        // A bad timecode is refused with exit 2, session unaffected.
        let reply = send(&socket, "seek bogus");
        assert_eq!(reply.lines().next().unwrap(), "2", "{reply}");
        // An unterminated quote is a protocol error, also exit 2.
        let reply = send(&socket, "put 'unterminated");
        assert_eq!(reply.lines().next().unwrap(), "2", "{reply}");
        assert!(reply.contains("unterminated quote"), "{reply}");

        // stop ends the session: reply first, then cleanup.
        let reply = send(&socket, "stop");
        assert!(reply.contains("stopped"), "{reply}");
        wait_until("cleanup", || !socket.exists());
        assert_eq!(handle.join().unwrap(), 0);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn render_exports_a_range_of_the_arrangement() {
        let dir = temp_dir();
        let src = dir.join("a.wav");
        write_test_wav(&src, 0.5, 0.5);
        let spec = format!("{},00:00:00-00:00:00.500", src.to_string_lossy());
        let out = dir.join("out.wav");
        let out_s = out.to_string_lossy().into_owned();

        let mut a = Arrangement::default();
        // Two butt-joined clips: 0..0.5s and 0.5..1.0s.
        let put = parse_command(&["put".to_string(), spec.clone()]).unwrap();
        dispatch(&mut a, put, "").unwrap();
        let put2 = parse_command(&["put".to_string(), format!("{},00:00:00-00:00:00.500", src.to_string_lossy()), "@00:00:00.500".to_string()]).unwrap();
        dispatch(&mut a, put2, "").unwrap();

        // A 0.25s window from 0.25s: half of the first clip only.
        let render = parse_command(&["render".to_string(), out_s.clone(), "00:00:00.250-00:00:00.500".to_string()]).unwrap();
        let reply = dispatch(&mut a, render, "").unwrap().to_string();
        assert!(reply.contains("00:00:00.250"), "rendered span: {reply}");
        let decoder = rodio::Decoder::new(std::io::BufReader::new(std::fs::File::open(&out).unwrap())).unwrap();
        let total = decoder.total_duration().unwrap();
        assert!((total.as_secs_f64() - 0.25).abs() < 0.05, "rendered {total:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn render_exports_the_arrangement_to_wav() {
        let dir = temp_dir();
        let src = dir.join("a.wav");
        write_test_wav(&src, 0.2, 0.5);
        let spec = format!("{},00:00:00-00:00:00.200", src.to_string_lossy());
        let out = dir.join("out.wav");
        let out_s = out.to_string_lossy().into_owned();

        let mut a = Arrangement::default();
        let put = parse_command(&["put".to_string(), spec]).unwrap();
        dispatch(&mut a, put, "").unwrap();
        let render = parse_command(&["render".to_string(), out_s.clone()]).unwrap();
        let reply = dispatch(&mut a, render, "").unwrap().to_string();
        assert!(reply.contains("rendered") && reply.contains("00:00:00.200"), "{reply}");
        assert!(out.exists() && out.metadata().unwrap().len() > 1000, "a real wav was written");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn set_names_a_track() {
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", "a.wav,00:00:00-00:00:10"]);
        let out = run_ok(&mut a, &["set", "track.0.name", "bed"]);
        assert!(out.contains("track 0 named \"bed\""), "{out}");
        assert!(run_ok(&mut a, &["ls"]).contains("track 0: name=bed"), "ls shows the label");
        let (code, msg) = run_err(&mut a, &["set", "track.9.name", "x"]);
        assert_eq!(code, 1);
        assert!(msg.contains("no track 9"), "{msg}");
    }

    #[test]
    fn serialize_round_trips_names_volume_and_mute() {
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", "bed.wav,00:00:05-00:00:25"]);
        run_ok(&mut a, &["put", "ding.wav,00:00:01-00:00:02", "1@00:00:00"]);
        run_ok(&mut a, &["set", "track.0.name", "bed"]);
        run_ok(&mut a, &["set", "track.0.volume", "0.5"]);
        run_ok(&mut a, &["set", "track.1.muted", "true"]);
        run_ok(&mut a, &["set", "master", "0.78"]);

        let script = serialize(&a);
        assert!(script.contains("set track.0.name bed"), "{script}");
        assert!(script.contains("set track.0.volume 0.5"), "{script}");
        assert!(script.contains("set track.1.muted true"), "{script}");
        assert!(script.contains("set master 0.78"), "master is saved: {script}");
        let mut fresh = Arrangement::default();
        run_script(&mut fresh, &script, "test", "").unwrap();
        assert_eq!(serialize(&fresh), script, "the script rebuilds the same arrangement");
    }

    #[test]
    fn save_and_load_round_trip_via_commands() {
        let dir = temp_dir();
        let file = dir.join("prog.bo");
        let path = file.to_string_lossy().into_owned();
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", "a.wav,00:00:00-00:00:10"]);
        run_ok(&mut a, &["set", "track.0.name", "bed"]);
        run_ok(&mut a, &["set", "track.0.volume", "0.5"]);
        run_ok(&mut a, &["set", "master", "0.78"]);
        run_ok(&mut a, &["save", &path]);

        let mut b = Arrangement::default();
        run_ok(&mut b, &["load", &path]);
        assert_eq!(serialize(&b), serialize(&a));
        assert_eq!(b.player.volume(), 0.78, "master survives the round trip");

        // A failing script leaves the live arrangement untouched.
        std::fs::write(&file, "put a.wav,00:00:00-00:00:10 0\nput b.wav,00:00:00-00:00:10 0@00:00:05\n")
            .unwrap();
        let (code, msg) = run_err(&mut b, &["load", &path]);
        assert_eq!(code, 1);
        assert!(msg.contains("refused"), "{msg}");
        assert_eq!(b.player.tracks()[0].clips().len(), 1, "failed load left the arrangement alone");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_keeps_the_live_audio_backend() {
        // Loading used to swap the whole arrangement in, replacing the
        // daemon's audio backend with a bare silent one — the next play
        // reported "backend silent" and made no sound. The backend's note
        // is the marker: a real daemon carries it (or rodio itself).
        let mut a = Arrangement::with_backend(AnyBackend::Silent(
            Silent::default(),
            Some("no audio device: x".to_string()),
        ));
        run_ok(&mut a, &["put", "a.wav,00:00:00-00:00:10"]);
        let dir = temp_dir();
        let script = dir.join("p.bo");
        let path = script.to_string_lossy().into_owned();
        std::fs::write(&script, "set master 0.5\n").unwrap();
        run_ok(&mut a, &["load", &path]);
        assert_eq!(
            a.player.backend().note(),
            Some("no audio device: x"),
            "the live backend survives a load"
        );
        assert_eq!(a.player.volume(), 0.5, "master came across");
        assert_eq!(a.player.tracks().len(), 0, "the old arrangement was replaced");
        assert_eq!(a.player.state(), State::Stopped, "transport resets with the load");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn render_measure_only_reports_levels_without_writing() {
        let dir = temp_dir();
        let src = dir.join("a.wav");
        write_test_wav(&src, 1.0, 0.5);
        let spec = format!("{},00:00:00-00:00:01", src.to_string_lossy());
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", &spec]);

        let render = parse_command(&["render".to_string(), "--measure".to_string()]).unwrap();
        let reply = dispatch(&mut a, render, "").unwrap().to_string();
        assert!(reply.starts_with("measure: 00:00:01.000"), "{reply}");
        assert!(reply.contains("peak: -6.0 dBFS"), "{reply}");
        assert!(reply.contains("rms: -9.0 dBFS"), "{reply}");
        assert!(reply.contains("true_peak:"), "{reply}");
        assert!(reply.contains("loudest_1s:"), "{reply}");
        assert!(reply.contains("note: span under 3s"), "under 3 s, no LUFS: {reply}");
        assert!(!reply.contains("integrated:"), "no LUFS under 3 s: {reply}");
        // Nothing was written.
        assert!(!dir.join("out.wav").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn render_with_measure_writes_and_reports() {
        let dir = temp_dir();
        let src = dir.join("a.wav");
        write_test_wav(&src, 0.5, 0.5);
        let spec = format!("{},00:00:00-00:00:00.500", src.to_string_lossy());
        let out = dir.join("out.wav");
        let out_s = out.to_string_lossy().into_owned();
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", &spec]);
        let render = parse_command(&[
            "render".to_string(),
            out_s.clone(),
            "--measure".to_string(),
        ])
        .unwrap();
        let reply = dispatch(&mut a, render, "").unwrap().to_string();
        assert!(reply.starts_with("rendered"), "{reply}");
        assert!(reply.contains("rms:"), "{reply}");
        assert!(out.exists(), "the file was still written");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn measure_only_refuses_an_empty_arrangement() {
        let mut a = Arrangement::default();
        let render = parse_command(&["render".to_string(), "--measure".to_string()]).unwrap();
        let (code, msg) = dispatch(&mut a, render, "").unwrap_err();
        assert_eq!(code, 1);
        assert!(msg.contains("no clips: nothing to measure"), "{msg}");
    }

    #[test]
    fn render_without_file_or_measure_is_usage() {
        let mut a = Arrangement::default();
        let render = parse_command(&["render".to_string()]).unwrap();
        let (code, msg) = dispatch(&mut a, render, "").unwrap_err();
        assert_eq!(code, 2);
        assert!(msg.contains("--measure"), "{msg}");
        // A bare range in the file slot is caught with guidance.
        run_ok(&mut a, &["put", "a.wav,00:00:00-00:00:10"]);
        let render = parse_command(&["render".to_string(), "0-3".to_string(), "--measure".to_string()])
            .unwrap();
        let (code, msg) = dispatch(&mut a, render, "").unwrap_err();
        assert_eq!(code, 2);
        assert!(msg.contains("looks like a range"), "{msg}");
    }

    #[test]
    fn check_reports_unreadable_sources() {
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", "/nonexistent.wav,00:00:00-00:00:10"]);
        let (code, msg) = run_err(&mut a, &["check"]);
        assert_eq!(code, 1);
        assert!(msg.contains("cannot open /nonexistent.wav"), "{msg}");
        assert!(msg.contains("1 problem"), "{msg}");
    }

    #[test]
    fn check_verifies_real_sources() {
        let dir = temp_dir();
        let src = dir.join("a.wav");
        write_test_wav(&src, 0.2, 0.5);
        let spec = format!("{},00:00:00-00:00:00.200", src.to_string_lossy());
        let mut a = Arrangement::default();
        let put = parse_command(&["put".to_string(), spec]).unwrap();
        dispatch(&mut a, put, "").unwrap();
        let out = run_ok(&mut a, &["check"]);
        assert!(out.contains("all sources ok"), "{out}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn probe_measures_sources_locally_and_in_the_arrangement() {
        let dir = temp_dir();
        let src = dir.join("a.wav");
        write_test_wav(&src, 0.2, 0.5);
        let path = src.to_string_lossy().into_owned();

        let mut a = Arrangement::default();
        let out = run_ok(&mut a, &["probe", path.as_str()]);
        assert!(out.contains("probe:") && out.contains("00:00:00.200"), "{out}");
        assert_eq!(a.player.tracks().len(), 0, "a bare probe touches nothing");

        run_ok(&mut a, &["put", path.as_str()]);
        let out = run_ok(&mut a, &["probe"]);
        assert!(out.contains("probe: 1 source"), "{out}");
        assert!(out.contains("00:00:00.200"), "{out}");

        // A source that cannot be opened is reported, and fails the probe.
        run_ok(&mut a, &["put", "/nonexistent.wav,00:00:00-00:00:10"]);
        let (code, msg) = run_err(&mut a, &["probe"]);
        assert_eq!(code, 1);
        assert!(msg.contains("2 sources"), "{msg}");
        assert!(msg.contains("cannot open /nonexistent.wav"), "{msg}");

        let (code, msg) = run_err(&mut a, &["probe", "/missing.wav"]);
        assert_eq!(code, 1);
        assert!(msg.contains("cannot open /missing.wav"), "{msg}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn top_level_help_is_grouped_and_subcommand_help_stays_clap() {
        assert!(HELP.contains("Arrangement:"), "grouped: {HELP}");
        assert!(HELP.contains("Mix:"), "grouped: {HELP}");
        assert!(HELP.contains("Transport:"), "grouped: {HELP}");
        assert!(HELP.contains("EXAMPLES"), "examples: {HELP}");
        assert_eq!(run(vec![]), 0, "bare invocation shows the grouped help");
        assert_eq!(run(vec!["--help".into()]), 0);
        assert_eq!(run(vec!["help".into()]), 0);
        assert_eq!(run(vec!["put".into(), "--help".into()]), 0, "subcommand help stays clap's");
    }

    #[test]
    fn clap_help_and_usage_exit_codes() {
        assert_eq!(parse_full(&["--help".into()]).unwrap_err().exit_code(), 0);
        assert_eq!(parse_full(&["put".into(), "--help".into()]).unwrap_err().exit_code(), 0);
        assert_eq!(parse_full(&["nope".into()]).unwrap_err().exit_code(), 2);
        assert_eq!(parse_full(&["put".into()]).unwrap_err().exit_code(), 2);
        assert_eq!(parse_full(&["put".into(), "a.wav".into(), "0".into(), "1".into()]).unwrap_err().exit_code(), 2);
    }

    #[test]
    fn daemon_serves_commands_and_exits_when_playback_finishes() {
        let dir = temp_dir();
        let socket = dir.join("d.sock");

        let handle = {
            let socket = socket.clone();
            thread::spawn(move || daemon_main_with(&socket, AnyBackend::silent()))
        };
        wait_until("socket", || UnixStream::connect(&socket).is_ok());

        let reply = send(&socket, "put a.wav,00:00:00-00:00:00.200");
        assert!(reply.contains("ok: track 0 clip #0"), "{reply}");
        let reply = send(&socket, "put b.wav,00:00:00-00:00:00.200 0@00:00:00.200");
        assert!(reply.contains("ok: track 0 clip #1"), "{reply}");

        // A refused command still gets a framed reply and exit code.
        let reply = send(&socket, "nope");
        assert_eq!(reply.lines().next().unwrap(), "2", "{reply}");

        // A 0.4s arrangement: play it, and the daemon cleans up on completion.
        let reply = send(&socket, "play");
        assert!(reply.contains("playing from"), "{reply}");
        wait_until("cleanup", || !socket.exists());
        assert_eq!(handle.join().unwrap(), 0);

        std::fs::remove_dir_all(&dir).ok();
    }
}
