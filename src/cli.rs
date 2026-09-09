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
//! * `put [--repeat n] [--gain g] [--fade-in t] [--fade-in-from v]
//!   [--fade-out t] [--fade-out-to v] <spec> [where]` — place a clip, or n
//!   butt-joined copies of it, on a track; `where` is `track[@pos]`, an
//!   omitted track creates a fresh one and an omitted pos means the playhead.
//!   A track is created on demand (up to that index). A source with no
//!   out-point is probed at put time; the whole batch is refused atomically
//!   if any copy collides.
//! * `play` — start playback from the current playhead; refused with exit 1
//!   when the arrangement has nothing to play, and the reply opens with a
//!   `session:` summary of what is about to play.
//! * `pause` / `resume` — hold and continue, keeping the position.
//! * `stop` — stop and rewind; ends the daemon's session (cleanup as usual).
//! * `seek <t>` — move the playhead; a running transport re-plans. Seeking
//!   past the arrangement's end is refused.
//! * `apply` — make every pending arrangement edit audible. Most edits land
//!   as they are made: a gain or a fade goes straight into the graph that is
//!   playing it, and a clip placed past the end of a track's queue is
//!   appended to it. `apply` is for what a running graph cannot take — a clip
//!   taken or moved — and rebuilds the graph from where the audio really is.
//! * `set <var> <value>` — set an attribute and land it; a bare `set`
//!   lists every current var and value. The key space is one registry:
//!   `master` (always real-time), `track.N.volume/pan/muted/name`,
//!   `bus.N.volume/muted/name`, and `clip.N.N.gain/pan/fade_in/fade_in_from/
//!   fade_out/fade_out_to/fade_shape/pan_control/gain_control`. A plain value
//!   is a token (a gain, a timecode, a boolean, a name); only the two
//!   `*_control` properties take a one-line JSON source (`none` unplugs).
//!   Gains, pans and fades land on the running graph; a name is a label, and
//!   an edit made while nothing plays lands at the next `play`.
//! * `take <track> <clip>` — remove a clip; the clip is addressed by its
//!   stable id or an `@timecode` (the clip covering that moment).
//! * `move <track> <clip> <dest>` — move a clip to another track, or to a
//!   new position on its own; `dest` is `[track]@[pos]`, an omitted track
//!   meaning the source track and an omitted pos the playhead. The move
//!   keeps the clip's gain, fades and — when the id is free on the
//!   destination — its id, and is refused whole if the destination is
//!   occupied.
//! * `render [file] [from-to]` — mix the arrangement to a wav file,
//!   offline; a range renders only that span. With `--measure` the reply
//!   also reports the mix's peak/RMS/true peak and EBU R128 loudness,
//!   folded from the exact stream the file writer consumes; omit the file
//!   to measure the whole arrangement without writing.
//! * `save <file>` / `load <file>` — write the arrangement as a script of
//!   commands, or replace it from one (transport resets with the swap).
//! * `reset` — drop every track and stop the transport: the daemon is back
//!   to its fresh state, ready for a session script to rebuild it.
//! * `check` — verify every distinct source is readable. An unreadable
//!   source is a problem (exit 1); a source whose length the container
//!   cannot state rates at most a note, since no clip depends on a measured
//!   length — open-ended puts probed theirs when placed, and every clip
//!   carries a finite out-point.
//! * `probe [uri]` — measure the length and channel count of a source, or of
//!   every distinct source in the arrangement; a bare uri is probed locally,
//!   no daemon. A source whose container states no length is decoded to its
//!   end and the reply marks it `estimated`.
//! * `ls` — dump the arrangement: an `ok:` reply, a session line
//!   (`stopped, playhead at …, '…' backend, end=…, master=…,
//!   idle_timeout=…`), then one track block per track with indented
//!   `clip #id …` signature lines.
//! * `at <t>` — show the mix at track time `t`: every clip covering that
//!   moment, one per track.
//!
//! Every reply opens with `ok: …` or `err: …`; timecodes are `HH:MM:SS.fff`
//! strings, gains two decimals, and absent means default. `bo <command>
//! --help` documents each command's reply shape.
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
//! finite length — estimated by decoding to the end when the container states
//! no length (see `probe`); a source that cannot be opened or decoded is
//! refused.
//!
//! In-points are honored exactly: a clip reads the source from `from` — plus
//! the playhead offset when playback enters it mid-way — the same in live
//! play and in offline render, sample-accurate for any decodable format.

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

use bo::bus::{BusRef, Group};
use bo::engine::rodio::{
    measure, probe, probe_sources, render_and_measure, render_and_measure_mono, render_to_file,
    render_to_file_mono, Probing, SourceLength,
};
use bo::engine::{Applied, Change, Landed, Player, State};
use bo::engine::session::Runtime;
use bo::control::ControlSource;
use bo::track::{Clip, Fade, FadeShape, Source, Track};
use clap::error::ErrorKind;
use clap::{Parser, Subcommand};

mod reply;

use reply::{
    ApplyReport, AtLine, BusLabel, Ls, LsBus, LsClip, LsTrack, Output, PlacedClip, ProbeResult,
    RoutedBus, Tc,
};

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
    /// `--gain`, `--fade-in`, `--fade-in-from`, `--fade-out` and
    /// `--fade-out-to` set the clip's gain and fade envelope; the fade shape
    /// is set with `set clip…fade_shape`.
    Put {
        /// uri[,from-to]
        spec: String,
        /// Where to place it: track[@pos] — omit the track for a fresh one,
        /// omit the pos for the playhead.
        placement: Option<String>,
        /// Place this many butt-joined copies of the clip.
        #[arg(long)]
        repeat: Option<u32>,
        /// Set the clip's gain, 0..=1.
        #[arg(long)]
        gain: Option<f32>,
        /// Fade in over this long.
        #[arg(long)]
        fade_in: Option<String>,
        /// The level the fade-in starts from, 0..=1 (default 0).
        #[arg(long)]
        fade_in_from: Option<f32>,
        /// Fade out over this long.
        #[arg(long)]
        fade_out: Option<String>,
        /// The level the fade-out ends at, 0..=1 (default 0).
        #[arg(long)]
        fade_out_to: Option<f32>,
    },
    /// Remove a clip by its id or the `@timecode` it covers.
    Take {
        /// Track index.
        track: usize,
        /// Clip id, or `@timecode`.
        clip: String,
    },
    /// Move a clip to another track, or to a new position on its own.
    /// Keeps the clip's gain, fades and — when the id is free on the
    /// destination — its id.
    Move {
        /// Track the clip is on now.
        track: usize,
        /// Clip id, or `@timecode`.
        clip: String,
        /// Destination: `[track]@[pos]`. An omitted track means the source
        /// track; an omitted pos means the playhead.
        dest: String,
    },
    /// Route a track's output into a group bus — several tracks share one
    /// strip (its volume, its mute) before the master hears them, a radio
    /// "music bus" or "voice bus". The bus is created by its first mention
    /// and named by it; bus names are unique; `master` routes the track back
    /// out. Routing is structure: a running mix takes it on the next apply.
    Route {
        /// Track index.
        track: usize,
        /// Target: a bus name (created on first mention), or `master`.
        #[arg(allow_hyphen_values = true)]
        bus: String,
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
        /// Fold the mix to mono (`(L+R)/2`) — a broadcast or single-speaker
        /// delivery. With `--measure`, the levels describe the mono fold.
        #[arg(long)]
        mono: bool,
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
    /// Verify every source in the arrangement is readable. Unreadable
    /// sources are problems; a source whose length the container cannot
    /// state rates at most a note.
    Check,
    /// Measure the length of a source, or of every distinct source in the
    /// arrangement. A bare uri runs locally — no daemon is spawned.
    /// Lengths the container cannot state are decoded to their end and
    /// marked estimated.
    Probe {
        /// Source uri to measure; omit to probe the arrangement's sources.
        uri: Option<String>,
    },
    /// Set an attribute, or — with no arguments at all — list every
    /// current var and value. `master`, or `track.N.<prop>` /
    /// `bus.N.<prop>` / `clip.T.C.<prop>`.
    Set {
        /// Attribute path; omit to list every current var and value.
        var: Option<String>,
        /// Value: a gain, a pan, `true`/`false`, a name, or (for the two
        /// `*_control` properties) a one-line JSON source. A pan may lead
        /// with `-`, so values are read as-is rather than as flags.
        #[arg(allow_hyphen_values = true)]
        value: Option<String>,
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
    /// Show the grouped help; `bo help <command>` shows that command's
    /// usage and an example of its reply.
    #[command(hide = true)]
    Help {
        /// Command to document: put, take, ls, …
        topic: Option<String>,
    },
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
    player: Player<Runtime>,
    /// `BO_IDLE_TIMEOUT` seconds, set by the daemon at startup (default
    /// 600, `0` disables). Reported on `ls` so a quiet session cannot
    /// silently time out.
    idle_timeout: u64,
    /// The arrangement-building commands the typed path ran, in order —
    /// the daemon's snapshot history.
    history: Vec<bo_core::command::Command>,
}

impl Default for Arrangement {
    fn default() -> Self {
        Self::with_backend(Runtime::silent())
    }
}

impl Arrangement {
    fn with_backend(backend: Runtime) -> Self {
        Self {
            player: Player::new(backend),
            idle_timeout: 600,
            history: Vec::new(),
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
bo - edit and mix audio, one command at a time

USAGE
  bo [--socket PATH] <command> [args...]

COMMANDS

Arrangement:
  put <spec> [where]       place a clip; where = track[@pos] - omit the track
                           for a fresh one and the pos for the playhead
                           (--repeat n places n butt-joined copies)
                           (--gain/--fade-in/--fade-in-from/--fade-out/
                           --fade-out-to set the clip's gain and fades)
  take <track> <clip>      remove a clip - by its id, or the @timecode it
                           covers
  move <track> <clip> <dest>
                           move a clip to another track, or to a new
                           position on its own; keeps its gain, fades and
                           (when free on the destination) its id. dest =
                           [track]@[pos] - track omitted: the same track,
                           pos omitted: the playhead
  route <track> <bus>      route a track's output into a group bus - several
                           tracks share one strip (its volume, its mute)
                           before the master hears them, a radio music bus
                           or voice bus. The bus is created by its first
                           mention and named by it; bus names are unique;
                           'master' routes the track back out. Routing is
                           structure: a running mix takes it on the next
                           apply
  ls                       dump the arrangement: an ok: reply, a session
                           line, then one track block per track with clip
                           signature lines
  at <t>                   show what plays at track time t
  render [file] [from-to]  mix the arrangement to a wav file; a range
                           renders only that span (from- to the end)
                           --measure reports peak/RMS/true peak and EBU
                           R128 loudness; omit the file to measure only
                           --mono folds the mix to one channel ((L+R)/2),
                           for broadcast or a single speaker; with
                           --measure the levels describe the mono fold
  save <file>              write the arrangement as a script
  load <file>              replace the arrangement from a script
  reset                    drop every track and stop; back to a fresh
                           session
  check                    verify every source is readable; unreadable
                           sources are problems (exit 1), sources with no
                           measurable length rate at most a note
  probe [uri]              measure a source's length and channels; a length
                           the container cannot state is decoded and marked
                           estimated; without a uri, every source in the
                           arrangement

Mix:
  set <var> <value>        set an attribute and land it on the running mix
                           (playing or paused; a name is only a label). With
                           no var at all, `bo set` lists every current var
                           and value - the key space below, one row each:
                           master, track.N.volume/pan/muted/name,
                           bus.N.volume/muted/name, and clip.T.C.gain/pan/
                           fade_in/fade_in_from/fade_out/fade_out_to/
                           fade_shape/pan_control/gain_control.
                           A plain value is a token: a gain, a timecode, a
                           boolean, a name. Only the two *_control properties
                           take a one-line JSON object - a curve
                           '{\"type\":\"curve\",\"0\":1,\"3.2\":-1}', an lfo
                           '{\"type\":\"lfo\",\"shape\":\"sine\",\"rate\":1,\"depth\":0.5}'
                           or a sidechain '{\"type\":\"sidechain\",\"bus\":\"group.0\"}'
                           - and \"none\" unplugs. Timecodes may be typed as
                           bare seconds (3.2, 0.005).

Transport:
  play                     start playback from the current playhead
                           (refused when there is nothing to play)
  pause                    hold position
  resume                   continue after a pause
  stop                     stop, rewind, end the session
  seek <t>                 move the playhead (refused past the end)
  apply                    make pending edits audible: what a running mix
                           cannot take itself (a clip taken or moved) is
                           what rebuilds it

OPTIONS
  --socket PATH            unix socket the daemon listens on
                           (default: $TMPDIR/bo/daemon.sock)
  -h, --help               show this help
  -V, --version            print version
  help <command>           show a command's usage and its reply example

OUTPUT
  Every reply opens with ok: ... or err: ... (exit codes: 0 ok, 1 refused,
  2 usage). Timecodes are HH:MM:SS.fff strings; gains two decimals; absent
  means default. An edit a running mix could not take itself adds one
  note: line, and `ls` counts what is waiting as pending=N. A clip is one
  signature line, a track a header over its clips:

    clip #<id> '<uri>' <from>-<to> @ <at> [gain=..] [fade_in=..] ... [curve=..]
    track <n> '<name>'|untitled vol=.. pan=.. end=.. [muted] [bus=#<id> '<name>']
    bus #<id> '<name>'|untitled vol=.. tracks=.. [muted]   # group buses

  bo help <command> shows that command's reply shape with a real example.

CLIP SPEC
  uri[,from-to]            from-to = slice of the source (default: whole,
                           resolved by probing); `,` marks the slice
  track[@pos]              track = index (created on demand), pos = where on
                           the track (default: playhead); `@` marks position
  Timecodes: SS, MM:SS or HH:MM:SS, optional .fff fraction. `:` is reserved
  for timecodes.

  A source with no out-point is probed at put time and its whole length is
  used, so every clip has a known finite end; when the container states no
  length the file is decoded to its end (see `probe` - the reply marks it
  `estimated`). A source that cannot be opened or decoded is refused (run
  `bo probe <uri>`). Spans are half-open: clips may butt-join (one ends
  exactly where the next starts). In-points are exact: reading starts at
  `from` (plus the playhead offset when entering mid-clip),
  sample-accurate in both play and render.

EXAMPLES
  bo put bed.wav,00:00:00-00:00:30
  bo put voice.wav,00:00:00-00:00:30 1@00:00:00
  bo play
  bo set track.0.volume 0.4  # duck the bed; lands as it is set
  bo route 0 music           # bed under one strip: route a track into a bus
  bo route 1 music
  bo set bus.0.volume 0.4    # duck the whole music bus (on the next apply)
  bo put outro.wav,0-10 0@00:00:30   # keep queueing while it plays
  bo apply                 # for what a running mix cannot take itself
  bo ls
  bo stop                  # end the session; daemon cleans up
";

/// Names of the user-facing subcommands: `bo <name> --help` must keep
/// clap's own per-command help, while `bo --help` shows the grouped [`HELP`].
const SUBCOMMAND_NAMES: [&str; 19] = [
    "put", "take", "move", "route", "ls", "at", "render", "save", "load", "reset", "check",
    "probe", "play", "pause", "resume", "stop", "seek", "apply", "set",
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
/// duration (delegates to [`bo::time::parse`]).
fn parse_timecode(s: &str) -> Result<Duration, String> {
    bo::time::parse(s)
}

/// Format a duration as `HH:MM:SS.fff` (delegates to [`bo::time::format`]).
fn format_time(d: Duration) -> String {
    bo::time::format(d)
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

/// Frame an error message as the `err:` reply. A leading `error: ` (clap's
/// own framing) is stripped; the first line gets the `err:` prefix and any
/// further lines (a `reason:` or indented body) follow as-is. Replies that
/// already carry their own `ok:`/`err:` head pass through untouched —
/// `check` and `probe` report failure inside the reply itself.
fn frame_err(msg: &str) -> String {
    let msg = msg.strip_prefix("error: ").unwrap_or(msg);
    if msg.starts_with("ok: ") || msg.starts_with("err: ") {
        return format!("{msg}\n");
    }
    let mut out = String::new();
    for (i, line) in msg.lines().enumerate() {
        if i == 0 {
            let _ = writeln!(out, "err: {line}");
        } else {
            let _ = writeln!(out, "{line}");
        }
    }
    if out.is_empty() {
        out.push_str("err: \n");
    }
    out
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
        Command::Put {
            spec,
            placement,
            repeat,
            gain,
            fade_in,
            fade_in_from,
            fade_out,
            fade_out_to,
        } => put_command(
            a,
            &spec,
            placement.as_deref(),
            repeat,
            gain,
            fade_in.as_deref(),
            fade_in_from,
            fade_out.as_deref(),
            fade_out_to,
            cwd,
        ),
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
                    "cannot seek to {} - the arrangement ends at {}",
                    format_time(t),
                    format_time(a.player.duration())
                )));
            }
            a.player.seek(t).map_err(|e| fail(e.to_string()))?;
            Ok(Output::Seeked { at: t })
        }
        Command::Apply => apply_command(a),
        Command::Set { var, value } => match (var, value) {
            (None, None) => set_list(a),
            (Some(var), None) => Err(usage(format!(
                "set {var:?} needs a <value>; `bo set` with no var lists the current vars and values"
            ))),
            (Some(var), Some(value)) => set_command(a, &var, &value),
            // <var> comes first positionally, so a value can never arrive alone.
            (None, Some(_)) => unreachable!(),
        },
        Command::Take { track, clip } => take_command(a, track, &clip),
        Command::Move { track, clip, dest } => move_command(a, track, &clip, &dest),
        Command::Route { track, bus } => route_command(a, track, &bus),
        Command::Render {
            file,
            range,
            measure,
            mono,
        } => {
            let file = file.map(|f| absolutize(&f, cwd));
            let (from, to) = match range {
                Some(r) => parse_range(&r).map_err(usage)?,
                None => (Duration::ZERO, None),
            };
            let volume = a.player.volume();
            match (file.as_deref(), measure) {
                // A bare measure of the whole arrangement.
                (None, true) => {
                    if a.player.duration() == Duration::ZERO {
                        return Err(fail("no clips: nothing to measure"));
                    }
                    let (duration, stats) = if mono {
                        render_and_measure_mono(a.player.tracks(), a.player.groups(), None, from, to, volume)
                    } else {
                        render_and_measure(a.player.tracks(), a.player.groups(), None, from, to, volume)
                    }
                    .map_err(|e| fail(format!("render failed: {e}")))?;
                    Ok(Output::Rendered {
                        file: None,
                        duration,
                        stats: Some(stats),
                    })
                }
                (Some(path), measure) => {
                    // With --measure, a bare timecode in the file slot is
                    // almost certainly a mistyped range.
                    if measure && path.contains('-') && parse_range(path).is_ok() {
                        return Err(usage(format!(
                            "{path:?} looks like a range; `render --measure` measures the whole \
                             arrangement - to measure a range, write it: `render out.wav {path} \
                             --measure`"
                        )));
                    }
                    let path = std::path::Path::new(path);
                    let (duration, stats) = if mono && measure {
                        render_and_measure_mono(a.player.tracks(), a.player.groups(), Some(path), from, to, volume)
                            .map(|(d, m)| (d, Some(m)))
                    } else if mono {
                        render_to_file_mono(a.player.tracks(), a.player.groups(), path, from, to, volume)
                            .map(|d| (d, None))
                    } else if measure {
                        render_and_measure(a.player.tracks(), a.player.groups(), Some(path), from, to, volume)
                            .map(|(d, m)| (d, Some(m)))
                    } else {
                        render_to_file(a.player.tracks(), a.player.groups(), path, from, to, volume).map(|d| (d, None))
                    }
                    .map_err(|e| fail(format!("render failed: {e}")))?;
                    Ok(Output::Rendered {
                        file: Some(file.expect("a rendered wav always has a path")),
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
            let groups: Vec<Group> = staged.player.groups().to_vec();
            a.player.reset();
            a.player.set_volume(volume);
            a.player.tracks_mut().extend(tracks);
            // A loaded arrangement is a different mix entirely: whatever was
            // playing has nothing to do with it.
            a.player.changed(Change::Structure);
            // The group buses come across with the tracks that are routed
            // into them — strips and all, ids preserved.
            a.player.set_groups(groups);
            Ok(Output::Loaded { file })
        }
        Command::Reset => reset_command(a),
        Command::Check => {
            let clips: usize = a.player.tracks().iter().map(Track::len).sum();
            let outcomes = probe_sources(a.player.tracks());
            let problems: Vec<(String, String)> = outcomes
                .iter()
                .filter_map(|(uri, result)| result.as_ref().err().map(|e| (uri.clone(), e.clone())))
                .collect();
            // A source whose length the container cannot state is not a
            // problem: every clip carries a finite out-point (an open-ended
            // put probed it when it was placed), so no clip depends on a
            // measured length. It rates at most a note.
            let estimated = outcomes
                .iter()
                .filter(|(_, r)| {
                    matches!(
                        r,
                        Ok(Probing {
                            length: SourceLength::Estimated(_),
                            ..
                        })
                    )
                })
                .count();
            let has_problem = !problems.is_empty();
            let out = Output::Check {
                clips,
                problems,
                estimated,
            };
            if has_problem {
                Err((1, out.to_string()))
            } else {
                Ok(out)
            }
        }
        Command::Probe { uri } => match uri {
            None => probe_arrangement(a),
            Some(uri) => probe_uri(&absolutize(&uri, cwd)),
        },
        // Help is handled locally by the client; this arm keeps a stray
        // "help" line over the socket harmless.
        Command::Help { .. } => Ok(Output::Text(HELP)),
        Command::Daemon => Err(fail("the daemon runs standalone, not over the socket")),
    }
}

/// The arrangement as data for `ls`: an `ok:` head, a session line (state,
/// playhead, backend, end, master), then one `track` block per track with
/// indented `clip #id` signature lines. See [`Output::Ls`]'s rendering for
/// the exact grammar.
fn arrangement_view(a: &Arrangement) -> Ls {
    let p = &a.player;
    Ls {
        state: p.state(),
        playhead: p.playhead(),
        end: p.duration(),
        backend: p.backend().name(),
        volume: p.volume(),
        idle_timeout: a.idle_timeout,
        pending: p.pending().len(),
        buses: p
            .groups()
            .iter()
            .map(|g| LsBus {
                id: g.id(),
                name: g.name().map(str::to_string),
                volume: g.gain(),
                muted: g.muted(),
                tracks: p
                    .tracks()
                    .iter()
                    .filter(|t| t.bus() == BusRef::Group(g.id()))
                    .count(),
            })
            .collect(),
        tracks: p
            .tracks()
            .iter()
            .map(|t| LsTrack {
                name: t.name().map(str::to_string),
                volume: t.volume(),
                pan: t.pan(),
                muted: t.muted(),
                bus: match t.bus() {
                    BusRef::Master => None,
                    BusRef::Group(id) => Some(BusLabel {
                        id,
                        name: p.group(id).and_then(|g| g.name().map(str::to_string)),
                    }),
                },
                end: t.duration(),
                clips: t
                    .clips()
                    .iter()
                    .map(|c| LsClip {
                        id: c.id,
                        uri: c.source.uri.clone(),
                        at: c.at,
                        from: c.from,
                        to: c.to,
                        gain: c.gain,
                        pan: c.placement.map(bo::bus::Placement::position),
                        fade_in: c.fade.fade_in,
                        fade_in_from: c.fade.fade_in_from,
                        fade_out: c.fade.fade_out,
                        fade_out_to: c.fade.fade_out_to,
                        fade_shape: c.fade.shape,
                        pan_control: {
                            let controls = &c.pan_controls;
                            (!controls.is_empty()).then(|| {
                                controls
                                    .iter()
                                    .map(ToString::to_string)
                                    .collect::<Vec<_>>()
                                    .join("; ")
                            })
                        },
                        gain_control: {
                            let controls = &c.gain_controls;
                            (!controls.is_empty()).then(|| {
                                controls
                                    .iter()
                                    .map(ToString::to_string)
                                    .collect::<Vec<_>>()
                                    .join("; ")
                            })
                        },
                    })
                    .collect(),
            })
            .collect(),
    }
}

/// `at <t>`: the mix at track time `t` — every clip covering that moment,
/// one per track, or `ok: silent at ...` when nothing plays there.
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
                from: c.from,
                to: c.to,
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
        Some(clip) => {
            // A queue that is already sounding cannot have one of its clips
            // taken out of the middle: this waits for an `apply`.
            let landed = a.player.changed(Change::Structure);
            Ok(Output::Removed {
                track: track_index,
                id: clip.id,
                uri: clip.source.uri.clone(),
                at: clip.at,
                from: clip.from,
                to: clip.to,
                gain: clip.gain,
                fade_in: clip.fade.fade_in,
                fade_in_from: clip.fade.fade_in_from,
                fade_out: clip.fade.fade_out,
                fade_out_to: clip.fade.fade_out_to,
                fade_shape: clip.fade.shape,
                landed: noting(a, Some(landed)),
            })
        }
        None => Err(fail(format!("no clip {track_index}#{clip_arg}"))),
    }
}

/// `move <track> <clip> <dest>`: move a clip to another track, or to a new
/// position on its own. Unlike take+put the clip keeps its gain, fades and —
/// when the id is free on the destination — its id; the move is one atomic
/// step, refused whole if the destination is occupied.
///
/// The destination is `[track]@[pos]`: an omitted track means the source
/// track, an omitted pos means the playhead. A destination track index is
/// created on demand, exactly as `put` does.
fn move_command(
    a: &mut Arrangement,
    track_index: usize,
    clip_arg: &str,
    dest_arg: &str,
) -> Result<Output, (i32, String)> {
    let (want_track, want_pos) = parse_placement(dest_arg).map_err(usage)?;
    if want_track.is_none() && want_pos.is_none() {
        return Err(usage(format!(
            "move needs a destination: [track]@[pos], got {dest_arg:?}"
        )));
    }
    // The clip to move, as it sits on the source track now.
    let clip = {
        let t = a
            .player
            .tracks()
            .get(track_index)
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
        id.and_then(|id| t.clips().iter().find(|c| c.id == id).cloned())
            .ok_or_else(|| fail(format!("no clip {track_index}#{clip_arg}")))?
    };
    let dest_index = want_track.unwrap_or(track_index);
    while a.player.tracks().len() <= dest_index {
        a.player.add_track(Track::new());
    }
    let pos = want_pos.unwrap_or_else(|| a.player.playhead());
    let mut moved = clip;
    moved.at = pos;
    // Refuse whole if the destination is occupied: check every clip on the
    // destination except the moving clip itself (a same-track move vacates
    // its own span).
    let destination = &a.player.tracks()[dest_index];
    if let Some(conflict) = destination
        .clips()
        .iter()
        .filter(|x| !(dest_index == track_index && x.id == moved.id))
        .find(|x| x.overlaps(&moved))
    {
        let span = format!(
            "[{:.3},{:.3})",
            conflict.at.as_secs_f64(),
            conflict.end().as_secs_f64()
        );
        let next = destination.next_free_start(moved.at, moved.duration());
        return Err(fail(format!(
            "move refused: clip #{} overlaps\nreason: clip #{} occupies {}; next free start is {:.3}s",
            moved.id,
            conflict.id,
            span,
            next.as_secs_f64(),
        )));
    }
    // Ids are per-track counters: the moved clip keeps its id unless the
    // destination track already carries it, in which case it takes the
    // destination's next free id (the reply says which).
    let keep_id = dest_index == track_index
        || !destination.clips().iter().any(|c| c.id == moved.id);
    a.player.tracks_mut()[track_index]
        .remove(moved.id)
        .expect("the clip was found above");
    let id = if keep_id {
        a.player.tracks_mut()[dest_index].insert_keeping_id(moved.clone());
        moved.id
    } else {
        a.player.tracks_mut()[dest_index]
            .insert(moved.clone())
            .expect("pre-checked: the destination is free")
    };
    moved.id = id;
    // Moving a clip re-orders a queue, which a running graph cannot do.
    let landed = a.player.changed(Change::Structure);
    Ok(Output::Moved {
        from_track: track_index,
        to_track: dest_index,
        clip: PlacedClip {
            id: moved.id,
            uri: moved.source.uri.clone(),
            at: moved.at,
            from: moved.from,
            to: moved.to,
            gain: moved.gain,
            fade_in: moved.fade.fade_in,
            fade_in_from: moved.fade.fade_in_from,
            fade_out: moved.fade.fade_out,
            fade_out_to: moved.fade.fade_out_to,
            fade_shape: moved.fade.shape,
        },
        landed: noting(a, Some(landed)),
    })
}

/// `put <spec> [track[@pos]]`: place a clip, creating the track when needed.
///
/// The identifier in the output is the contract: an implicit put creates a
/// fresh track and prints its index; an explicit one names a track, created on
/// demand so that repeated puts rebuild the same layout. With `--repeat n`,
/// places n butt-joined copies as ordinary clips; the whole batch is checked
/// before any insert, so a collision refuses everything.
// One argument per put flag; the count is the command surface itself.
#[allow(clippy::too_many_arguments)]
fn put_command(
    a: &mut Arrangement,
    spec_arg: &str,
    placement_arg: Option<&str>,
    repeat: Option<u32>,
    gain: Option<f32>,
    fade_in: Option<&str>,
    fade_in_from: Option<f32>,
    fade_out: Option<&str>,
    fade_out_to: Option<f32>,
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
    let gain = gain.unwrap_or(1.0);
    let fade = Fade {
        fade_in: match fade_in {
            Some(s) => parse_timecode(s).map_err(usage)?,
            None => Duration::ZERO,
        },
        fade_in_from: fade_in_from.unwrap_or(0.0).clamp(0.0, 1.0),
        fade_out: match fade_out {
            Some(s) => parse_timecode(s).map_err(usage)?,
            None => Duration::ZERO,
        },
        fade_out_to: fade_out_to.unwrap_or(0.0).clamp(0.0, 1.0),
        shape: FadeShape::Linear,
    };
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
        .map(|i| {
            Clip::sliced(source.clone(), spec.from, to)
                .at(base_at + slice_len * i)
                .gain(gain)
                .fade(fade)
        })
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
            let who = if repeat > 1 {
                format!("copy {i} on track {track_index} @ {}", Tc(c.at))
            } else {
                format!("track {track_index} @ {}", Tc(c.at))
            };
            return Err(fail(format!(
                "put refused: {who} overlaps\nreason: clip #{} occupies {span}; next free start is {:.3}s",
                conflict.id,
                next.as_secs_f64(),
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
            gain: placed.gain,
            fade_in: placed.fade.fade_in,
            fade_in_from: placed.fade.fade_in_from,
            fade_out: placed.fade.fade_out,
            fade_out_to: placed.fade.fade_out_to,
            fade_shape: placed.fade.shape,
        });
    }
    // A clip placed past the end of what a track has already queued joins the
    // running graph as it is placed, which is what makes a live show
    // extensible without interrupting it. One placed into a gap *before*
    // queued material cannot — a queue can be extended, not re-ordered — so
    // it waits for an `apply`.
    let landed = a.player.changed(Change::Appended(track_index));
    Ok(Output::Put {
        track: track_index,
        clips: placed_clips,
        landed: noting(a, Some(landed)),
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

/// `apply`: make every pending arrangement edit audible.
///
/// Most edits land as they are made — a gain or a fade goes into the chain
/// that is playing it, a clip placed past a track's queue is appended to it.
/// This is for the rest: a clip taken or moved, an arrangement loaded. What
/// still cannot be taken live is what forces a graph rebuilt from where the
/// audio really is, so the rebuild picks up what the listener is hearing
/// rather than jumping ahead of it.
fn apply_command(a: &mut Arrangement) -> Result<Output, (i32, String)> {
    if !a.player.is_playing() && a.player.state() != State::Paused {
        return Ok(Output::Applied(ApplyReport {
            live: 0,
            rebuilt: None,
            stopped: true,
        }));
    }
    let report = match a.player.apply().map_err(|e| fail(e.to_string()))? {
        Applied::Nothing => ApplyReport {
            live: 0,
            rebuilt: None,
            stopped: false,
        },
        Applied::Live(live) => ApplyReport {
            live,
            rebuilt: None,
            stopped: false,
        },
        Applied::Rebuilt { live, at } => ApplyReport {
            live,
            rebuilt: Some(at),
            stopped: false,
        },
        Applied::NotPlaying => ApplyReport {
            live: 0,
            rebuilt: None,
            stopped: true,
        },
    };
    Ok(Output::Applied(report))
}

/// `reset`: drop every track and stop the transport — the daemon is back to
/// its fresh state, ready for a session script to rebuild the arrangement.
fn reset_command(a: &mut Arrangement) -> Result<Output, (i32, String)> {
    let tracks = a.player.tracks().len();
    a.player.reset();
    Ok(Output::Reset { tracks })
}

/// Whether an edit's landing is worth a line in the reply.
///
/// Only a pending edit made while something is playing: one that landed needs
/// no report, and with nothing playing every edit lands at the next `play`,
/// which is the default rather than news.
fn noting(a: &Arrangement, landed: Option<Landed>) -> Option<Landed> {
    landed.filter(|l| *l == Landed::Pending && a.player.state() != State::Stopped)
}

/// `route <track> <bus>`: point a track's output at a group bus, or back at
/// the master. A bus is created by its first mention and named by it — bus
/// names are unique — so `route 0 music` twice routes the same bus, and a
/// typo becomes a new (empty, visible) bus rather than silence.
///
/// Routing is structure, like taking or moving a clip: the graph has to be
/// rebuilt for it to sound, so a running mix takes it on the next `apply`.
fn route_command(a: &mut Arrangement, track: usize, bus: &str) -> Result<Output, (i32, String)> {
    let bus = bus.trim();
    if bus.is_empty() {
        return Err(usage("route needs a bus name"));
    }
    // Resolve the track first, so a bad index creates no stray bus.
    if track >= a.player.tracks().len() {
        return Err(fail(format!("no track {track}")));
    }
    let target = match bus {
        "master" => BusRef::Master,
        _ => match group_named(&a.player, bus) {
            Some(id) => BusRef::Group(id),
            None => BusRef::Group(a.player.add_group(Some(bus.to_string()))),
        },
    };
    a.player.tracks_mut()[track].set_bus(target);
    let landed = a.player.changed(Change::Structure);
    let to = match target {
        BusRef::Master => None,
        BusRef::Group(id) => {
            let name = a.player.group(id).and_then(|g| g.name().map(str::to_string));
            let members = a
                .player
                .tracks()
                .iter()
                .filter(|t| t.bus() == BusRef::Group(id))
                .count();
            Some(RoutedBus {
                id,
                name,
                tracks: members,
            })
        }
    };
    Ok(Output::Routed {
        track,
        to,
        landed: noting(a, Some(landed)),
    })
}

/// The id of the group bus named `name`, when one exists.
fn group_named(player: &Player<Runtime>, name: &str) -> Option<u64> {
    player
        .groups()
        .iter()
        .find(|g| g.name() == Some(name))
        .map(Group::id)
}

/// `set <var> <value>`: set an attribute, and land it on whatever is playing.
///
/// `master` has always been real-time. The rest is arrangement data that the
/// running graph is asked to take as it is — a track's gain, a clip's gain and
/// fades — and only what a graph cannot express waits for an `apply`. A name
/// is a label rather than part of the mix, so there is nothing to land.
///
/// The key space is the registry below: one property enum per object kind,
/// each variant knowing its key, how a value is parsed and applied, its
/// canonical current value, and what a change to it means in the mix. The
/// set paths (`master`, `track.N.<prop>`, `bus.N.<prop>`,
/// `clip.T.C.<prop>`) are parsed here; the property names themselves come
/// from the enums, so the reply, the errors and `bo set`'s listing can never
/// drift from what `set` accepts.
fn set_command(a: &mut Arrangement, var: &str, value: &str) -> Result<Output, (i32, String)> {
    if var == "master" {
        return set_master(a, value);
    }
    if let Some(rest) = var.strip_prefix("bus.") {
        return set_bus_command(a, rest, value);
    }
    if let Some(rest) = var.strip_prefix("clip.") {
        return set_clip_command(a, rest, value);
    }
    let (index, prop) = var
        .strip_prefix("track.")
        .and_then(|rest| rest.split_once('.'))
        .ok_or_else(|| usage(unknown_var(var)))?;
    let index: usize = index
        .parse()
        .map_err(|_| usage(format!("bad track {index:?}")))?;
    let prop = TrackProp::parse(prop).map_err(usage)?;
    let (value, change) = {
        let t = a
            .player
            .tracks_mut()
            .get_mut(index)
            .ok_or_else(|| fail(format!("no track {index}")))?;
        prop.apply(t, value).map_err(usage)?;
        (prop.read(t), prop.change(index))
    };
    let landed = land(a, change);
    Ok(Output::Set {
        var: format!("track.{index}.{}", prop.key()),
        value,
        landed,
    })
}

/// `set master <v>`: the master bus's gain. Always real-time.
fn set_master(a: &mut Arrangement, value: &str) -> Result<Output, (i32, String)> {
    let v: f32 = value
        .parse()
        .map_err(|_| usage(format!("bad gain {value:?}")))?;
    a.player.set_volume(v);
    Ok(Output::Set {
        var: "master".into(),
        value: two(a.player.volume()),
        landed: None,
    })
}

/// `set bus.<id>.<prop>`: set a group bus's strip (`volume`, `muted`) or
/// its `name`. A strip change is a group edit: no running graph can take it
/// (the strip is baked when a graph is built), so it waits for an `apply`'s
/// rebuild. A name is a label, and must be unique among buses — it is what
/// `route` addresses — and never `master`, which is the master bus's own.
fn set_bus_command(
    a: &mut Arrangement,
    rest: &str,
    value: &str,
) -> Result<Output, (i32, String)> {
    let (id, prop) = rest.split_once('.').ok_or_else(|| {
        usage(format!("bad bus var bus.{rest:?}: expected bus.ID.PROP"))
    })?;
    let id: u64 = id
        .parse()
        .map_err(|_| usage(format!("bad bus {id:?}")))?;
    let prop = BusProp::parse(prop).map_err(usage)?;
    // A rename must be checked against the table before the bus is touched;
    // the other properties only need the bus to exist.
    if prop == BusProp::Name {
        if value == "master" {
            return Err(usage("'master' is reserved for the master bus"));
        }
        if let Some(other) = a
            .player
            .groups()
            .iter()
            .find(|g| g.id() != id && g.name() == Some(value))
        {
            return Err(fail(format!(
                "a bus named {value:?} already exists as #{}",
                other.id()
            )));
        }
    }
    let (value, change) = {
        let bus = a
            .player
            .group_mut(id)
            .ok_or_else(|| fail(format!("no bus {id}")))?;
        prop.apply(bus, value).map_err(usage)?;
        (prop.read(bus), prop.change(id))
    };
    let landed = land(a, change);
    Ok(Output::Set {
        var: format!("bus.{id}.{}", prop.key()),
        value,
        landed,
    })
}

/// `set clip.<track>.<id>.<prop>`: set a clip attribute and land it on the
/// clip's own source chain, which reads its gain, placement and envelope as
/// it plays.
fn set_clip_command(
    a: &mut Arrangement,
    rest: &str,
    value: &str,
) -> Result<Output, (i32, String)> {
    let (track, rest) = rest.split_once('.').ok_or_else(|| {
        usage(format!(
            "bad clip var clip.{rest:?}: expected clip.TRACK.ID.PROP"
        ))
    })?;
    let (id, prop) = rest.split_once('.').ok_or_else(|| {
        usage(format!(
            "bad clip var clip.{rest:?}: expected clip.TRACK.ID.PROP"
        ))
    })?;
    let track_i: usize = track
        .parse()
        .map_err(|_| usage(format!("bad track {track:?}")))?;
    let id: u64 = id.parse().map_err(|_| usage(format!("bad clip id {id:?}")))?;
    let prop = ClipProp::parse(prop).map_err(usage)?;
    let (value, change) = {
        let t = a
            .player
            .tracks_mut()
            .get_mut(track_i)
            .ok_or_else(|| fail(format!("no track {track_i}")))?;
        let c = t
            .clip_mut(id)
            .ok_or_else(|| fail(format!("no clip {track_i}#{id}")))?;
        prop.apply(c, value).map_err(usage)?;
        (prop.read(c), prop.change(track_i, id))
    };
    let landed = land(a, change);
    Ok(Output::Set {
        var: format!("clip.{track_i}.{id}.{}", prop.key()),
        value,
        landed,
    })
}

/// `set` with no var: list every current var and value, row by row, from
/// the same registry `set` applies through. Master first; then each track's
/// properties and its clips' non-default ones; the group buses last. A clip
/// row only appears when the clip does not already carry the default, so an
/// untouched clip is quiet rather than ten lines of noise.
fn set_list(a: &Arrangement) -> Result<Output, (i32, String)> {
    let tracks = a.player.tracks().len();
    let clips: usize = a.player.tracks().iter().map(|t| t.clips().len()).sum();
    let buses = a.player.groups().len();
    let mut rows: Vec<(String, String)> = vec![("master".into(), two(a.player.volume()))];
    for (i, t) in a.player.tracks().iter().enumerate() {
        for p in TrackProp::ALL {
            if let Some(v) = p.row(t) {
                rows.push((format!("track.{i}.{}", p.key()), v));
            }
        }
        for c in t.clips() {
            for p in ClipProp::ALL {
                if let Some(v) = p.row(c) {
                    rows.push((format!("clip.{i}.{}.{}", c.id, p.key()), v));
                }
            }
        }
    }
    for g in a.player.groups() {
        for p in BusProp::ALL {
            if let Some(v) = p.row(g) {
                rows.push((format!("bus.{}.{}", g.id(), p.key()), v));
            }
        }
    }
    Ok(Output::SetList { rows, tracks, clips, buses })
}

/// Land a change on the running graph and decide whether the reply should
/// say so (only a pending edit made while something plays).
fn land(a: &mut Arrangement, change: Option<Change>) -> Option<Landed> {
    match change {
        Some(change) => {
            let landed = a.player.changed(change);
            noting(a, Some(landed))
        }
        None => None,
    }
}

/// A var that names no set target, told with the whole key space.
fn unknown_var(var: &str) -> String {
    format!(
        "unknown var {var:?}: set takes master, track.N.<prop>, bus.N.<prop> \
         or clip.T.C.<prop> - `bo set` lists the current ones"
    )
}

/// A level at two decimals, the reply's gain dialect.
fn two(v: f32) -> String {
    format!("{v:.2}")
}

/// The registry of a track's properties: every `track.N.<prop>` key is one
/// variant, knowing its name, how a value is parsed and applied, its
/// canonical current value, and what a change to it means to a running mix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrackProp {
    Volume,
    Pan,
    Muted,
    Name,
}

impl TrackProp {
    const ALL: [Self; 4] = [Self::Volume, Self::Pan, Self::Muted, Self::Name];

    fn key(self) -> &'static str {
        match self {
            Self::Volume => "volume",
            Self::Pan => "pan",
            Self::Muted => "muted",
            Self::Name => "name",
        }
    }

    fn parse(name: &str) -> Result<Self, String> {
        Self::ALL
            .iter()
            .copied()
            .find(|p| p.key() == name)
            .ok_or_else(|| {
                format!(
                    "no track property {name:?} - try {}",
                    Self::ALL.iter().map(|p| p.key()).collect::<Vec<_>>().join(", ")
                )
            })
    }

    fn apply(self, t: &mut Track, value: &str) -> Result<(), String> {
        match self {
            Self::Volume => {
                let v: f32 = value
                    .parse()
                    .map_err(|_| format!("bad gain {value:?}"))?;
                t.set_volume(v);
            }
            Self::Pan => {
                let v: f32 = value
                    .parse()
                    .map_err(|_| format!("bad pan {value:?}: -1..1"))?;
                t.set_pan(v);
            }
            Self::Muted => {
                let b = parse_bool(value)?;
                t.set_muted(b);
            }
            Self::Name => t.set_name(value.to_string()),
        }
        Ok(())
    }

    /// The canonical current value, the dialect replies and `bo set` share.
    fn read(self, t: &Track) -> String {
        match self {
            Self::Volume => two(t.volume()),
            Self::Pan => two(t.pan()),
            Self::Muted => t.muted().to_string(),
            Self::Name => t.name().unwrap_or_default().to_string(),
        }
    }

    /// What a change to this property means in the mix; a name is a label and
    /// changes nothing.
    fn change(self, i: usize) -> Option<Change> {
        match self {
            Self::Volume | Self::Muted => Some(Change::TrackGain(i)),
            Self::Pan => Some(Change::TrackPan(i)),
            Self::Name => None,
        }
    }

    /// The row `bo set` lists for this property on `t`, when the property is
    /// worth a row (a mute off and an unnamed track are the defaults, so they
    /// are absent rather than noise).
    fn row(self, t: &Track) -> Option<String> {
        let worth = match self {
            Self::Volume | Self::Pan => true,
            Self::Muted => t.muted(),
            Self::Name => t.name().is_some(),
        };
        worth.then(|| self.read(t))
    }
}

/// The registry of a group bus's properties: `bus.N.<prop>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BusProp {
    Volume,
    Muted,
    Name,
}

impl BusProp {
    const ALL: [Self; 3] = [Self::Volume, Self::Muted, Self::Name];

    fn key(self) -> &'static str {
        match self {
            Self::Volume => "volume",
            Self::Muted => "muted",
            Self::Name => "name",
        }
    }

    fn parse(name: &str) -> Result<Self, String> {
        Self::ALL
            .iter()
            .copied()
            .find(|p| p.key() == name)
            .ok_or_else(|| {
                format!(
                    "no bus property {name:?} - try {}",
                    Self::ALL.iter().map(|p| p.key()).collect::<Vec<_>>().join(", ")
                )
            })
    }

    fn apply(self, bus: &mut Group, value: &str) -> Result<(), String> {
        match self {
            Self::Volume => {
                let v: f32 = value
                    .parse()
                    .map_err(|_| format!("bad gain {value:?}"))?;
                bus.set_gain(v);
            }
            Self::Muted => {
                let b = parse_bool(value)?;
                bus.set_muted(b);
            }
            Self::Name => bus.set_name(value.to_string()),
        }
        Ok(())
    }

    fn read(self, bus: &Group) -> String {
        match self {
            Self::Volume => two(bus.gain()),
            Self::Muted => bus.muted().to_string(),
            Self::Name => bus.name().unwrap_or_default().to_string(),
        }
    }

    fn change(self, id: u64) -> Option<Change> {
        match self {
            Self::Volume | Self::Muted => Some(Change::GroupGain(id)),
            Self::Name => None,
        }
    }

    fn row(self, bus: &Group) -> Option<String> {
        let worth = match self {
            Self::Volume => true,
            Self::Muted => bus.muted(),
            Self::Name => bus.name().is_some(),
        };
        worth.then(|| self.read(bus))
    }
}

/// The registry of a clip's properties: `clip.T.C.<prop>`. A gain, a fade,
/// a shape or a placement is ClipParams or ClipPan; a plug into the pan or
/// gain input is a store into the running chain (ClipControls,
/// ClipGainControls).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClipProp {
    Gain,
    Pan,
    FadeIn,
    FadeInFrom,
    FadeOut,
    FadeOutTo,
    FadeShape,
    PanControl,
    GainControl,
}

impl ClipProp {
    const ALL: [Self; 9] = [
        Self::Gain,
        Self::Pan,
        Self::FadeIn,
        Self::FadeInFrom,
        Self::FadeOut,
        Self::FadeOutTo,
        Self::FadeShape,
        Self::PanControl,
        Self::GainControl,
    ];

    fn key(self) -> &'static str {
        match self {
            Self::Gain => "gain",
            Self::Pan => "pan",
            Self::FadeIn => "fade_in",
            Self::FadeInFrom => "fade_in_from",
            Self::FadeOut => "fade_out",
            Self::FadeOutTo => "fade_out_to",
            Self::FadeShape => "fade_shape",
            Self::PanControl => "pan_control",
            Self::GainControl => "gain_control",
        }
    }

    fn parse(name: &str) -> Result<Self, String> {
        Self::ALL
            .iter()
            .copied()
            .find(|p| p.key() == name)
            .ok_or_else(|| {
                format!(
                    "no clip property {name:?} - try {}",
                    Self::ALL.iter().map(|p| p.key()).collect::<Vec<_>>().join(", ")
                )
            })
    }

    fn apply(self, c: &mut Clip, value: &str) -> Result<(), String> {
        match self {
            Self::Gain => {
                let v: f32 = value
                    .parse()
                    .map_err(|_| format!("bad gain {value:?}"))?;
                c.gain = v.clamp(0.0, 1.0);
            }
            Self::Pan => {
                // A clip's own placement overrides its track for the whole
                // clip; `auto` gives it back to the track.
                c.placement = match value.trim() {
                    "auto" => None,
                    v => {
                        let p: f32 = v
                            .parse()
                            .map_err(|_| format!("bad pan {v:?}: -1..1, or auto"))?;
                        Some(bo::bus::Placement::Stereo {
                            position: p.clamp(-1.0, 1.0),
                        })
                    }
                };
            }
            Self::FadeIn => {
                c.fade.fade_in = parse_timecode(value)?;
            }
            Self::FadeInFrom => {
                let level: f32 = value
                    .parse()
                    .map_err(|_| format!("bad gain {value:?}"))?;
                c.fade.fade_in_from = level.clamp(0.0, 1.0);
            }
            Self::FadeOut => {
                c.fade.fade_out = parse_timecode(value)?;
            }
            Self::FadeOutTo => {
                let level: f32 = value
                    .parse()
                    .map_err(|_| format!("bad gain {value:?}"))?;
                c.fade.fade_out_to = level.clamp(0.0, 1.0);
            }
            Self::FadeShape => {
                c.fade.shape = value.parse::<FadeShape>()?;
            }
            Self::PanControl => {
                // Plug one control source into the clip's pan input — a
                // curve, an LFO or a sidechain, as one JSON object — or
                // `none` to unplug. Repeated sets replace the source.
                let plug = plug(value)?;
                c.pan_controls = plug;
            }
            Self::GainControl => {
                // The same, into the clip's gain input.
                let plug = plug(value)?;
                c.gain_controls = plug;
            }
        }
        Ok(())
    }

    fn read(self, c: &Clip) -> String {
        match self {
            Self::Gain => two(c.gain),
            Self::Pan => match &c.placement {
                Some(p) => two(p.position()),
                None => "auto".to_string(),
            },
            Self::FadeIn => bo::time::format(c.fade.fade_in),
            Self::FadeInFrom => two(c.fade.fade_in_from),
            Self::FadeOut => bo::time::format(c.fade.fade_out),
            Self::FadeOutTo => two(c.fade.fade_out_to),
            Self::FadeShape => c.fade.shape.to_string(),
            Self::PanControl => c.pan_controls.first().map_or_else(
                || "none".to_string(),
                |s| s.to_string(),
            ),
            Self::GainControl => c.gain_controls.first().map_or_else(
                || "none".to_string(),
                |s| s.to_string(),
            ),
        }
    }

    fn change(self, track: usize, id: u64) -> Option<Change> {
        Some(match self {
            Self::Pan => Change::ClipPan(track, id),
            Self::PanControl => Change::ClipControls(track, id),
            Self::GainControl => Change::ClipGainControls(track, id),
            _ => Change::ClipParams(track, id),
        })
    }

    /// The row `bo set` lists for this property on `c` — only when the clip
    /// does not already carry the default (a full-gain, straight fade, pan
    /// following its track, nothing plugged).
    fn row(self, c: &Clip) -> Option<String> {
        let worth = match self {
            Self::Gain => c.gain != 1.0,
            Self::Pan => c.placement.is_some(),
            Self::FadeIn => c.fade.fade_in > Duration::ZERO,
            Self::FadeInFrom => c.fade.fade_in_from != 0.0,
            Self::FadeOut => c.fade.fade_out > Duration::ZERO,
            Self::FadeOutTo => c.fade.fade_out_to != 0.0,
            Self::FadeShape => c.fade.shape != FadeShape::Linear,
            Self::PanControl => c.pan_controls.len() == 1,
            Self::GainControl => c.gain_controls.len() == 1,
        };
        worth.then(|| self.read(c))
    }
}

/// Parse a `set ..._control` value: `none` unplugs, anything else must be
/// one JSON control source.
fn plug(value: &str) -> Result<Vec<ControlSource>, String> {
    match value.trim() {
        "none" => Ok(Vec::new()),
        text => Ok(vec![text.parse::<ControlSource>()?]),
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
    match measure(uri) {
        Ok(Probing { length, channels }) => Ok(Output::Probed {
            uri: uri.to_string(),
            length,
            channels,
        }),
        Err(e) => Err(fail(e)),
    }
}

/// `probe` with no uri: measure every distinct source in the arrangement.
/// Lists each source's length (≈ marks an estimate); exit 1 if any source
/// cannot be opened or decoded.
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
        Command::Put {
            spec,
            placement,
            repeat,
            gain,
            fade_in,
            fade_in_from,
            fade_out,
            fade_out_to,
        } => {
            let mut line = String::from("put");
            if let Some(n) = repeat {
                let _ = write!(line, " --repeat {n}");
            }
            let _ = write!(line, " {}", quote_arg(spec));
            if let Some(p) = placement {
                let _ = write!(line, " {}", quote_arg(p));
            }
            if let Some(g) = gain {
                let _ = write!(line, " --gain {g}");
            }
            if let Some(f) = fade_in {
                let _ = write!(line, " --fade-in {}", quote_arg(f));
            }
            if let Some(f) = fade_in_from {
                let _ = write!(line, " --fade-in-from {f}");
            }
            if let Some(f) = fade_out {
                let _ = write!(line, " --fade-out {}", quote_arg(f));
            }
            if let Some(f) = fade_out_to {
                let _ = write!(line, " --fade-out-to {f}");
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
        Command::Set { var, value } => match (var, value) {
            (None, None) => "set".to_string(),
            (Some(var), None) => format!("set {}", quote_arg(var)),
            (Some(var), Some(value)) => format!("set {} {}", quote_arg(var), quote_arg(value)),
            (None, Some(_)) => unreachable!("a value never precedes its var"),
        },
        Command::Take { track, clip } => format!("take {track} {}", quote_arg(clip)),
        Command::Move {
            track,
            clip,
            dest,
        } => format!("move {track} {} {}", quote_arg(clip), quote_arg(dest)),
        Command::Route { track, bus } => format!("route {track} {}", quote_arg(bus)),
        Command::Render {
            file,
            range,
            measure,
            mono,
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
            if *mono {
                line.push_str(" --mono");
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
        Command::Help { .. } => unreachable!("help is handled locally, never sent"),
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
            let mut line = format!(
                "put {},{}-{} {ti}@{}",
                quote_arg(&c.source.uri),
                format_time(c.from),
                format_time(c.to),
                format_time(c.at)
            );
            if c.gain != 1.0 {
                let _ = write!(line, " --gain {}", c.gain);
            }
            if c.fade.fade_in > Duration::ZERO {
                let _ = write!(line, " --fade-in {}", format_time(c.fade.fade_in));
            }
            if c.fade.fade_in_from != 0.0 {
                let _ = write!(line, " --fade-in-from {}", c.fade.fade_in_from);
            }
            if c.fade.fade_out > Duration::ZERO {
                let _ = write!(line, " --fade-out {}", format_time(c.fade.fade_out));
            }
            if c.fade.fade_out_to != 0.0 {
                let _ = write!(line, " --fade-out-to {}", c.fade.fade_out_to);
            }
            let _ = writeln!(out, "{line}");
            // The fade shape is set-only, so a non-default curve serializes
            // as its own set line rather than a put flag.
            if c.fade.shape != FadeShape::Linear {
                let _ = writeln!(out, "set clip.{ti}.{}.fade_shape {}", c.id, c.fade.shape);
            }
            // A clip that carries its own placement serializes the same way.
            if let Some(p) = c.placement {
                let _ = writeln!(out, "set clip.{ti}.{}.pan {}", c.id, p.position());
            }
            // So does a plug in its pan input. One source has a set form
            // today; more than one is a future surface. The JSON is quoted
            // so the line tokenizes like any other value.
            if c.pan_controls.len() == 1 {
                let _ = writeln!(
                    out,
                    "set clip.{ti}.{}.pan_control {}",
                    c.id,
                    quote_arg(&c.pan_controls[0].to_string())
                );
            }
            if c.gain_controls.len() == 1 {
                let _ = writeln!(
                    out,
                    "set clip.{ti}.{}.gain_control {}",
                    c.id,
                    quote_arg(&c.gain_controls[0].to_string())
                );
            }
        }
        if let Some(name) = t.name() {
            let _ = writeln!(out, "set track.{ti}.name {}", quote_arg(name));
        }
        // A grouped track routes out of the master; the first route line in
        // the file is what creates the bus, named by it.
        if let BusRef::Group(id) = t.bus()
            && let Some(name) = a.player.group(id).and_then(|g| g.name())
        {
            let _ = writeln!(out, "route {ti} {}", quote_arg(name));
        }
        let _ = writeln!(out, "set track.{ti}.volume {}", t.volume());
        let _ = writeln!(out, "set track.{ti}.pan {}", t.pan());
        if t.muted() {
            let _ = writeln!(out, "set track.{ti}.muted true");
        }
    }
    // Bus strips come after every track block: a bus exists in the rebuilt
    // session from the first route that mentioned it, so only then can its
    // strip be set. A bus no track was routed to — empty, or its tracks
    // emptied of clips — is not recreated (empty tracks are not saved either)
    // and carries no lines.
    for g in a.player.groups() {
        let has_member = a
            .player
            .tracks()
            .iter()
            .any(|t| t.bus() == BusRef::Group(g.id()));
        if !has_member {
            continue;
        }
        let _ = writeln!(out, "set bus.{}.volume {}", g.id(), g.gain());
        if g.muted() {
            let _ = writeln!(out, "set bus.{}.muted true", g.id());
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
        Command::Help { topic: None } => {
            println!("{HELP}");
            0
        }
        Command::Help { topic: Some(topic) } => match help_topic(&topic) {
            Some(text) => {
                println!("{text}");
                0
            }
            None => {
                eprintln!("bo: no such command {topic:?} - try `bo help`");
                2
            }
        },
        Command::Daemon => daemon_main(&socket),
        Command::Probe { uri: Some(uri) } => probe_client(&absolutize(&uri, &cwd)),
        command => client_main(&socket, &command),
    }
}

/// `bo help <command>`: one page per command — a usage synopsis and a real
/// example of its reply. The example is rendered by the same code that
/// renders live replies ([`reply::example_reply`]), so the documented shape
/// cannot drift from the actual output.
fn help_topic(topic: &str) -> Option<String> {
    let synopsis = match topic {
        "put" => "bo put <spec> [track[@pos]] [--repeat n] [--gain g] \
                   [--fade-in t] [--fade-in-from v] [--fade-out t] [--fade-out-to v]",
        "take" => "bo take <track> <clip>        # clip: an id, or @timecode",
        "move" => "bo move <track> <clip> <dest>  # dest: [track]@[pos]; track omitted = same\n                              # track, pos omitted = playhead",
        "ls" => "bo ls",
        "at" => "bo at <t>",
        "render" => "bo render [file] [from-to] [--measure]",
        "save" => "bo save <file>",
        "load" => "bo load <file>",
        "reset" => "bo reset",
        "check" => "bo check",
        "probe" => "bo probe [uri]",
        "route" => "bo route <track> <bus>     # bus: a name (created by its first\n                              # mention), or master to route back out",
        "set" => "bo set [<var> <value>]   # no args lists every current var and value",
        "play" => "bo play",
        "pause" => "bo pause",
        "resume" => "bo resume",
        "stop" => "bo stop",
        "seek" => "bo seek <t>",
        "apply" => "bo apply",
        _ => return None,
    };
    let reply = reply::example_reply(topic)?;
    Some(format!("{synopsis}\n\nreply example:\n{reply}"))
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
            print!("{}", frame_err(&msg));
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
    daemon_main_with(socket, Runtime::open())
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
fn daemon_main_with(socket: &Path, backend: Runtime) -> i32 {
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
/// value says whether this command ends the daemon's session. A line that
/// opens with `{` is the typed JSON protocol the session client speaks;
/// anything else is the CLI's text grammar.
fn handle_line(state: &Mutex<Arrangement>, line: &str, cwd: &str) -> (i32, String, bool) {
    if line.trim_start().starts_with('{') {
        return handle_json(state, line, cwd);
    }
    let args = match tokenize(line) {
        Ok(args) => args,
        Err(e) => return (2, frame_err(&e), false),
    };
    let command = match parse_command(&args) {
        Ok(command) => command,
        Err(e) => return (e.exit_code(), frame_err(&e.to_string()), false),
    };
    let ends_session = matches!(command, Command::Stop);
    let mut a = state.lock().unwrap();
    match dispatch(&mut a, command, cwd) {
        Ok(out) => (0, out.to_string(), ends_session),
        Err((code, msg)) => (code, frame_err(&msg), ends_session),
    }
}

/// One JSON command (the typed wire [`bo::connection::Connection`] speaks): run it
/// against the same arrangement, answer one JSON object. The exit code stays
/// `0` — success and refusal both live in the reply's `ok` field.
fn handle_json(state: &Mutex<Arrangement>, line: &str, cwd: &str) -> (i32, String, bool) {
    // One Command, off the wire. Relative paths resolve against the client's
    // cwd, like the text grammar's.
    let mut command = match serde_json::from_str::<bo_core::command::Command>(line) {
        Ok(command) => command,
        Err(e) => {
            return (
                0,
                json_reply(&bo_core::command::Reply::Err(
                    bo_core::command::Error::Parse(e.to_string()),
                )),
                false,
            )
        }
    };
    let mut a = state.lock().unwrap();
    use bo_core::command::Command as Cmd;
    let reply = match &command {
        // Host-level: the daemon holds the history and the transport.
        Cmd::Snapshot => {
            let snapshot = bo_core::command::Snapshot {
                version: bo_core::command::SNAPSHOT_VERSION,
                history: a.history.clone(),
                playhead: a.player.playhead(),
            };
            bo_core::command::Reply::Ok(bo_core::command::Outcome::Snapshot(snapshot))
        }
        Cmd::Load { snapshot } => match load_snapshot(&mut a, snapshot) {
            Ok(()) => bo_core::command::Reply::Ok(bo_core::command::Outcome::Loaded),
            Err(e) => bo_core::command::Reply::Err(e),
        },
        Cmd::Check { snapshot } => {
            use bo_core::command::Error;
            if snapshot.version != bo_core::command::SNAPSHOT_VERSION {
                return (
                    0,
                    json_reply(&bo_core::command::Reply::Err(Error::Version(format!(
                        "snapshot version {} — this build reads {}",
                        snapshot.version,
                        bo_core::command::SNAPSHOT_VERSION
                    )))),
                    false,
                );
            }
            let mut staged = Arrangement::default();
            let mut problems = Vec::new();
            for (i, command) in snapshot.history.iter().enumerate() {
                if let Err(e) = bo::engine::exec(&mut staged.player, command.clone()) {
                    problems.push(format!("#{} {}", i + 1, e));
                }
            }
            let reply = if problems.is_empty() {
                bo_core::command::Reply::Ok(bo_core::command::Outcome::Checked)
            } else {
                bo_core::command::Reply::Err(Error::Check(problems.join("\n")))
            };
            return (0, json_reply(&reply), false);
        }
        _ => {
            match &mut command {
                Cmd::Insert { uri, .. } => *uri = absolutize(uri, cwd),
                Cmd::Render { file, .. } => *file = absolutize(file, cwd),
                _ => {}
            }
            let executed = command.clone();
            match bo::engine::exec(&mut a.player, command) {
                Ok(outcome) => {
                    if is_mutator(&executed) {
                        a.history.push(executed);
                    }
                    bo_core::command::Reply::Ok(outcome)
                }
                Err(e) => bo_core::command::Reply::Err(e),
            }
        }
    };
    (0, json_reply(&reply), false)
}

/// The arrangement commands a snapshot records: everything that edits the
/// session. Transport, queries, renders and snapshots do not count.
fn is_mutator(command: &bo_core::command::Command) -> bool {
    use bo_core::command::Command as Cmd;
    matches!(
        command,
        Cmd::Insert { .. }
            | Cmd::Remove { .. }
            | Cmd::Move { .. }
            | Cmd::Route { .. }
            | Cmd::Set { .. }
    )
}

/// Replace the arrangement from a snapshot, atomically: the history runs on
/// a silent staging session first, so a failing script leaves the live
/// session untouched; only then do the staged tracks, groups, volume and
/// playhead come across.
fn load_snapshot(
    a: &mut Arrangement,
    snapshot: &bo_core::command::Snapshot,
) -> Result<(), bo_core::command::Error> {
    use bo_core::command::Error;
    if snapshot.version != bo_core::command::SNAPSHOT_VERSION {
        return Err(Error::Version(format!(
            "snapshot version {} — this build reads {}",
            snapshot.version,
            bo_core::command::SNAPSHOT_VERSION
        )));
    }
    let mut staged = Arrangement::default();
    for command in &snapshot.history {
        bo::engine::exec(&mut staged.player, command.clone()).map_err(|e| Error::Host(format!(
            "load failed at {command:?}: {e}"
        )))?;
    }
    // Commit: the audio backend must survive — swap only the arrangement.
    a.player.reset();
    a.player.set_volume(staged.player.volume());
    let tracks: Vec<_> = staged.player.tracks().to_vec();
    let groups: Vec<_> = staged.player.groups().to_vec();
    a.player.tracks_mut().extend(tracks);
    a.player.set_groups(groups);
    a.player.set_playhead(snapshot.playhead);
    a.player.changed(bo::engine::Change::Structure);
    a.history = snapshot.history.clone();
    Ok(())
}

/// Serialize a typed reply for the wire.
fn json_reply(reply: &bo_core::command::Reply) -> String {
    serde_json::to_string(reply).unwrap_or_else(|e| {
        serde_json::to_string(&bo_core::command::Reply::Err(bo_core::command::Error::Daemon(
            e.to_string(),
        )))
        .unwrap_or_default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bo::engine::{BackendEvent, Silent, State};
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

    /// A mono 16-bit wav whose content changes every whole second: second `i`
    /// is a sine at `freqs[i]`, so a render's audio identifies which second
    /// of the source it really came from.
    fn write_stepped_wav(path: &std::path::Path, freqs: &[f32]) {
        let rate = 44_100u32;
        let frames = (rate * freqs.len() as u32) as usize;
        let mut data = Vec::with_capacity(frames * 2);
        for i in 0..frames {
            let f = freqs[(i / rate as usize).min(freqs.len() - 1)];
            let v = (0.5
                * (2.0 * std::f32::consts::PI * f * i as f32 / rate as f32).sin()
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

    /// Dominant frequency of each whole-second block of a stereo render,
    /// channel 0, by zero-crossing count.
    fn block_freqs(path: &std::path::Path) -> Vec<f32> {
        let decoder = rodio::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).unwrap()))
            .unwrap();
        let mut blocks: Vec<(u64, u64)> = Vec::new(); // (crossings, samples)
        let mut prev: Option<f32> = None;
        for (i, s) in decoder.enumerate() {
            if i % 2 == 1 {
                continue; // channel 1
            }
            let second = (i / 2) / 44_100;
            while blocks.len() <= second {
                blocks.push((0, 0));
            }
            if let Some(p) = prev
                && (p < 0.0) != (s < 0.0)
            {
                blocks[second].0 += 1;
            }
            blocks[second].1 += 1;
            prev = Some(s);
        }
        blocks
            .into_iter()
            .map(|(crossings, samples)| {
                let window = samples as f32 / 44_100.0;
                crossings as f32 / (2.0 * window)
            })
            .collect()
    }

    /// A decodable wav whose header states no length (a zero-frame float
    /// wav): the stand-in for an mp3 without a Xing/Info frame.
    fn write_empty_wav(path: &std::path::Path) {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 44_100,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let w = hound::WavWriter::create(path, spec).unwrap();
        w.finalize().unwrap();
    }

    #[test]
    fn probe_marks_lengths_it_had_to_decode_as_estimated() {
        let dir = temp_dir();
        let empty = dir.join("empty.wav");
        write_empty_wav(&empty);
        let empty_s = empty.to_string_lossy().into_owned();
        let a = dir.join("a.wav");
        write_test_wav(&a, 0.2, 0.5);
        let a_s = a.to_string_lossy().into_owned();

        let mut arr = Arrangement::default();
        // A container that states its length stays plain.
        let out = run_ok(&mut arr, &["probe", &a_s]);
        assert!(out.contains("duration=00:00:00.200"), "{out}");
        assert!(!out.contains("estimated"), "{out}");
        // One that does not (an mp3 without a Xing/Info frame) is decoded
        // and marked.
        let out = run_ok(&mut arr, &["probe", &empty_s]);
        assert!(out.contains("duration=00:00:00.000 estimated"), "{out}");

        // The arrangement probe marks its rows the same way.
        run_ok(&mut arr, &["put", &format!("{a_s},00:00:00-00:00:00.200")]);
        run_ok(&mut arr, &["put", &format!("{empty_s},00:00:00-00:00:00.500")]);
        let out = run_ok(&mut arr, &["probe"]);
        assert!(out.contains("ok: 2 sources"), "{out}");
        assert!(out.contains("duration=00:00:00.200"), "{out}");
        assert!(out.contains("duration=00:00:00.000 estimated"), "{out}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn check_reports_only_unreadable_sources_as_problems() {
        // Every clip carries an explicit out-point, so a source whose length
        // the container cannot state (a vbr mp3 without a Xing/Info frame)
        // is at most a note — never a problem. Only an unreadable source
        // exits 1.
        let dir = temp_dir();
        let empty = dir.join("empty.wav");
        write_empty_wav(&empty);
        let empty_s = empty.to_string_lossy().into_owned();

        let mut arr = Arrangement::default();
        run_ok(&mut arr, &["put", &format!("{empty_s},00:00:00-00:00:00.500")]);
        let out = run_ok(&mut arr, &["check"]);
        assert!(out.contains("ok: 1 clip, all sources ok"), "{out}");
        assert!(
            out.contains("note: 1 source with no header length"),
            "{out}"
        );

        // A missing file is still a real problem: the clip cannot play.
        run_ok(&mut arr, &["put", "gone.wav,00:00:00-00:00:00.500", "0@00:00:01"]);
        let (code, msg) = run_err(&mut arr, &["check"]);
        assert_eq!(code, 1, "{msg}");
        assert!(msg.contains("err: 1 problem") && msg.contains("cannot open"), "{msg}");
        std::fs::remove_dir_all(&dir).ok();
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
        assert!(msg.contains("clip #0 occupies [0.000,10.000)"), "{msg}");
        assert!(msg.contains("next free start is 10.000s"), "{msg}");
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
        assert!(session.contains("ok: 2 tracks, 3 clips"), "{out}");
        assert!(session.contains("ends 00:00:15.000"), "{out}");
        assert!(session.contains("playing from 00:00:00.000"), "{out}");
    }

    #[test]
    fn play_reports_a_silent_fallback_note() {
        let mut a = Arrangement::with_backend(Runtime::Silent(
            Silent::default(),
            Some("no audio device: x".to_string()),
        ));
        run_ok(&mut a, &["put", "a.wav,00:00:00-00:00:10"]);
        let out = run_ok(&mut a, &["play"]);
        assert!(out.contains("note: no audio device: x, 'silent' backend"), "{out}");
        assert!(run_ok(&mut a, &["ls"]).contains("'silent' backend"), "ls names the backend");
    }

    #[test]
    fn ls_reports_the_idle_timeout() {
        // The session line says how long a quiet daemon will wait before
        // cleaning itself up, so a long session cannot silently vanish.
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", "a.wav,00:00:00-00:00:10"]);
        let out = run_ok(&mut a, &["ls"]);
        assert!(out.contains("idle_timeout=00:10:00.000"), "default: {out}");
        a.idle_timeout = 3;
        let out = run_ok(&mut a, &["ls"]);
        assert!(out.contains("idle_timeout=00:00:03.000"), "from BO_IDLE_TIMEOUT: {out}");
    }

    #[test]
    fn ls_shows_the_arrangement() {
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", "a.wav,00:00:00-00:00:10"]);
        run_ok(&mut a, &["put", "b.wav,00:00:00-00:00:05", "0@00:00:10"]);
        let out = run_ok(&mut a, &["ls"]);
        for key in [
            "stopped, playhead at",
            "end=00:00:15.000",
            "'silent' backend",
            "master=1.00",
            "track 0 untitled vol=1.00",
        ] {
            assert!(out.contains(key), "missing {key}: {out}");
        }
        assert!(out.contains("a.wav") && out.contains("b.wav"), "{out}");
        let out = run_ok(&mut a, &["ls"]);
        assert!(out.contains("@ 00:00:10.000"), "butt-joined clip: {out}");
    }

    #[test]
    fn set_writes_track_volume_and_master() {
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", "a.wav,00:00:00-00:00:10"]);
        let out = run_ok(&mut a, &["set", "track.0.volume", "0.5"]);
        assert!(out.contains("`track.0.volume` set to `0.50`"), "{out}");
        assert_eq!(a.player.tracks()[0].volume(), 0.5);
        run_ok(&mut a, &["set", "track.0.volume", "2.5"]);
        assert_eq!(a.player.tracks()[0].volume(), 1.0, "clamped");
        let (code, msg) = run_err(&mut a, &["set", "track.9.volume", "0.5"]);
        assert_eq!(code, 1);
        assert!(msg.contains("no track 9"), "{msg}");
        assert!(run_ok(&mut a, &["ls"]).contains("vol=1.00"), "ls shows the gain");

        // Placement: set, clamped to the field, shown on ls, landed live.
        let out = run_ok(&mut a, &["set", "track.0.pan", "-0.5"]);
        assert!(out.contains("`track.0.pan` set to `-0.50`"), "{out}");
        assert_eq!(a.player.tracks()[0].pan(), -0.5);
        run_ok(&mut a, &["set", "track.0.pan", "2.5"]);
        assert_eq!(a.player.tracks()[0].pan(), 1.0, "clamped to the field");
        run_ok(&mut a, &["set", "track.0.pan", "-3"]);
        assert_eq!(a.player.tracks()[0].pan(), -1.0);
        assert!(
            run_ok(&mut a, &["ls"]).contains("pan=-1.00"),
            "ls shows the placement"
        );
        let (code, _) = run_err(&mut a, &["set", "track.0.pan", "abc"]);
        assert_eq!(code, 2);

        // Master is real-time and clamped too.
        let out = run_ok(&mut a, &["set", "master", "0.78"]);
        assert!(out.contains("`master` set to `0.78`"), "{out}");
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
        assert_eq!(out.matches("clip #").count(), 3, "{out}");
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
        assert!(out.contains("clip #1"), "{out}");
        assert_eq!(a.player.tracks()[0].clips().len(), 2);
    }

    #[test]
    fn put_repeat_is_atomic_and_refuses_bad_input() {
        let mut a = Arrangement::default();
        // a occupies 15s..25s; copy 3 of a 5s slice lands at 15s and collides.
        run_ok(&mut a, &["put", "a.wav,00:00:00-00:00:10", "@00:00:15"]);
        let (code, msg) = run_err(&mut a, &["put", "--repeat", "4", "b.wav,00:00:00-00:00:05", "0"]);
        assert_eq!(code, 1);
        assert!(msg.contains("copy 3") && msg.contains("overlaps"), "{msg}");
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
    fn put_repeat_with_an_in_point_repeats_the_slice_not_the_head() {
        // --repeat n plus from-to: every copy is a butt-joined copy of the
        // *slice*, each starting at the in-point. Regression: the in-point
        // used to be dropped, so every copy silently played the source's
        // start instead.
        let dir = temp_dir();
        let src = dir.join("steps.wav");
        write_stepped_wav(&src, &[440.0, 880.0, 1760.0]);
        let spec = format!("{},00:00:02-00:00:03", src.to_string_lossy());
        let out = dir.join("out.wav");
        let out_s = out.to_string_lossy().into_owned();

        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", "--repeat", "3", spec.as_str(), "0"]);
        let render = parse_command(&["render".to_string(), out_s]).unwrap();
        dispatch(&mut a, render, "").unwrap();
        let freqs = block_freqs(&out);
        assert_eq!(freqs.len(), 3, "three butt-joined copies: {freqs:?}");
        for (i, f) in freqs.iter().enumerate() {
            assert!(
                (f - 1760.0).abs() < 40.0,
                "copy {i} must start at the in-point (1760 Hz), got {f:.0} Hz"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn at_shows_the_clips_covering_a_timecode() {
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", "a.wav,00:00:00-00:00:10"]);
        run_ok(&mut a, &["put", "b.wav,00:00:00-00:00:05", "0@00:00:10"]);
        run_ok(&mut a, &["put", "c.wav,00:00:00-00:00:03"]);

        let out = run_ok(&mut a, &["at", "00:00:01.000"]);
        assert!(out.contains("track 0: clip #0"), "{out}");
        assert!(out.contains("track 1: clip #0"), "{out}");
        let out = run_ok(&mut a, &["at", "00:00:12.000"]);
        assert!(out.contains("track 0: clip #1"), "{out}");
        assert!(!out.contains("track 1"), "{out}");
        let out = run_ok(&mut a, &["at", "00:00:20.000"]);
        assert!(out.contains("ok: silent at 00:00:20.000"), "{out}");
        let (code, _) = run_err(&mut a, &["at", "bogus"]);
        assert_eq!(code, 2);
    }

    /// How many times the backend was asked to plan a mix.
    fn plays(a: &Arrangement) -> usize {
        match a.player.backend() {
            Runtime::Silent(s, _) => s
                .events
                .iter()
                .filter(|e| **e == BackendEvent::Play)
                .count(),
            Runtime::Rodio(_) => unreachable!("tests use the silent backend"),
        }
    }

    #[test]
    fn apply_rebuilds_only_what_a_running_mix_cannot_take() {
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", "a.wav,00:00:00-00:00:10"]);

        // Stopped: nothing is sounding, so nothing can land yet.
        let out = run_ok(&mut a, &["apply"]);
        assert!(out.contains("not playing"), "{out}");
        assert_eq!(plays(&a), 0);

        run_ok(&mut a, &["play"]);
        run_ok(&mut a, &["seek", "00:00:04"]);
        assert_eq!(plays(&a), 2, "play and seek each plan a mix");

        // A gain lands on the mix as it is set: no note in the reply, and no
        // re-plan. That is the point of `apply` no longer being the only way
        // to make a change audible.
        let out = run_ok(&mut a, &["set", "track.0.volume", "0.5"]);
        assert!(out.contains("ok: `track.0.volume` set to `0.50`"), "{out}");
        assert!(!out.contains("note:"), "a live landing needs no note: {out}");
        assert_eq!(plays(&a), 2, "setting a gain does not re-plan");
        let out = run_ok(&mut a, &["apply"]);
        assert!(out.contains("ok: nothing pending"), "{out}");
        assert_eq!(plays(&a), 2);

        // Taking a clip out of a queue that is sounding is the one edit a
        // running graph cannot make: the reply says it waits, `ls` counts it,
        // and `apply` rebuilds for it — from where the audio is.
        let out = run_ok(&mut a, &["take", "0", "0"]);
        assert!(out.contains("note: lands at next apply"), "{out}");
        let out = run_ok(&mut a, &["ls"]);
        assert!(out.contains("pending=1"), "{out}");
        let out = run_ok(&mut a, &["apply"]);
        assert!(out.contains("rebuilt from 00:00:04.000"), "{out}");
        assert_eq!(plays(&a), 3, "one rebuild for the structural edit");
        assert_eq!(
            a.player.playhead(),
            Duration::from_secs(4),
            "apply does not move the playhead"
        );
        let out = run_ok(&mut a, &["ls"]);
        assert!(!out.contains("pending="), "nothing left waiting: {out}");
    }

    #[test]
    fn a_put_extends_a_running_mix_without_rebuilding_it() {
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", "a.wav,00:00:00-00:00:10"]);
        run_ok(&mut a, &["play"]);
        assert_eq!(plays(&a), 1);

        // The live-show move: queue the next item while the current one
        // plays. It joins the running queue instead of waiting for a rebuild.
        let out = run_ok(&mut a, &["put", "b.wav,00:00:00-00:00:05", "0@00:00:10"]);
        assert!(out.contains("ok: 1 clip on track 0"), "{out}");
        assert!(!out.contains("note:"), "it landed as it was placed: {out}");
        assert_eq!(plays(&a), 1, "appending to a queue is not a re-plan");

        let out = run_ok(&mut a, &["apply"]);
        assert!(out.contains("ok: nothing pending"), "{out}");
        assert_eq!(plays(&a), 1);
    }

    #[test]
    fn edits_made_while_paused_land_without_a_rebuild_on_resume() {
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", "a.wav,00:00:00-00:00:10"]);
        run_ok(&mut a, &["put", "a.wav,00:00:00-00:00:10", "0@00:00:10"]);
        run_ok(&mut a, &["play"]);
        run_ok(&mut a, &["pause"]);

        // Pausing holds the sound, not the graph: a gain still lands, and
        // saying so is not the reply's business.
        let out = run_ok(&mut a, &["set", "track.0.volume", "0.5"]);
        assert!(!out.contains("note:"), "a paused graph takes a gain: {out}");
        let out = run_ok(&mut a, &["apply"]);
        assert!(out.contains("ok: nothing pending"), "{out}");
        let out = run_ok(&mut a, &["resume"]);
        assert!(out.contains("ok: playing from"), "{out}");
        assert_eq!(plays(&a), 1, "nothing was rebuilt on the way back");

        // A structural edit while paused cannot land, and resume must not
        // drop it: the old trap was an `ok:` that never took effect.
        run_ok(&mut a, &["pause"]);
        let out = run_ok(&mut a, &["take", "0", "1"]);
        assert!(out.contains("note: lands at next apply"), "{out}");
        let out = run_ok(&mut a, &["resume"]);
        assert!(out.contains("ok: playing from"), "{out}");
        assert_eq!(plays(&a), 2, "resume gave the edit its graph");
        assert!(a.player.pending().is_empty());
    }

    #[test]
    fn reset_clears_every_track_and_transport() {
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", "a.wav,00:00:00-00:00:10"]);
        run_ok(&mut a, &["put", "b.wav,00:00:00-00:00:05"]);
        run_ok(&mut a, &["set", "track.0.name", "bed"]);
        run_ok(&mut a, &["play"]);
        let out = run_ok(&mut a, &["reset"]);
        assert!(out.contains("ok: 2 tracks removed"), "{out}");
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
        assert!(out.contains("removed 1 clip from track 0") && out.contains("clip #1") && out.contains("b.wav"), "{out}");

        // By id: the remaining a on track 0 is id 0.
        let out = run_ok(&mut a, &["take", "0", "0"]);
        assert!(out.contains("removed 1 clip from track 0") && out.contains("clip #0") && out.contains("a.wav"), "{out}");

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
        assert!(out.contains("removed 1 clip from track 0") && out.contains("clip #0"), "{out}");
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
        assert!(out.contains("`track.0.muted` set to `true`"), "{out}");
        assert!(a.player.tracks()[0].muted());
        let out = run_ok(&mut a, &["set", "track.0.muted", "false"]);
        assert!(out.contains("`track.0.muted` set to `false`"), "{out}");
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
            thread::spawn(move || daemon_main_with(&socket, Runtime::silent()))
        };
        wait_until("socket", || UnixStream::connect(&socket).is_ok());

        send(&socket, "put a.wav,00:00:00-00:00:10");
        let reply = send(&socket, "ls");
        assert!(reply.contains("track 0 untitled vol=1.00 pan=0.00 end=00:00:10.000") && reply.contains("a.wav"), "{reply}");

        let reply = send(&socket, "set track.0.volume 0.5");
        assert!(reply.contains("`track.0.volume` set to `0.50`"), "{reply}");
        let reply = send(&socket, "set track.0.muted true");
        assert!(reply.contains("`track.0.muted` set to `true`"), "{reply}");
        let reply = send(&socket, "set track.0.muted false");
        assert!(reply.contains("`track.0.muted` set to `false`"), "{reply}");
        let reply = send(&socket, "take 0 0");
        assert!(reply.contains("removed 1 clip from track 0") && reply.contains("clip #0"), "{reply}");
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
        assert!(reply.contains("`track.0.name` set to `bed`"), "{reply}");
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
        assert!(out.contains("`track.0.name` set to `bed`"), "{out}");
        assert!(run_ok(&mut a, &["ls"]).contains("track 0 'bed'"), "ls shows the label");
        let (code, msg) = run_err(&mut a, &["set", "track.9.name", "x"]);
        assert_eq!(code, 1);
        assert!(msg.contains("no track 9"), "{msg}");
    }

    #[test]
    fn move_repositions_a_clip_between_tracks_keeping_identity() {
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", "a.wav,00:00:00-00:00:10", "0@00:00:00"]); // #0
        run_ok(
            &mut a,
            &["put", "--gain", "0.5", "b.wav,00:00:00-00:00:05", "0@00:00:10"],
        ); // #1, gain 0.5
        run_ok(&mut a, &["put", "c.wav,00:00:00-00:00:03"]); // track 1, #0

        // Cross-track: b (#1, gain 0.5) lands on a fresh track 2 at 2 s.
        let out = run_ok(&mut a, &["move", "0", "1", "2@00:00:02"]);
        assert!(out.contains("from track 0 to track 2 @ 00:00:02.000"), "{out}");
        assert!(out.contains("clip #1") && out.contains("gain=0.50"), "{out}");
        assert_eq!(a.player.tracks()[0].clips().len(), 1, "source kept a only");
        let moved = &a.player.tracks()[2].clips()[0];
        assert_eq!(
            (moved.id, moved.at, moved.gain, moved.from, moved.to),
            (
                1,
                Duration::from_secs(2),
                0.5,
                Duration::ZERO,
                Duration::from_secs(5)
            ),
            "identity, gain and slice survive the move"
        );
    }

    #[test]
    fn move_within_a_track_repositions_by_timecode() {
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", "a.wav,00:00:00-00:00:10", "0@00:00:00"]); // #0 0..10
        run_ok(&mut a, &["put", "b.wav,00:00:00-00:00:05", "0@00:00:10"]); // #1 10..15
        // a (covering @00:00:02) moves to 15 s on the same track: 15..25.
        let out = run_ok(&mut a, &["move", "0", "@00:00:02", "@00:00:15"]);
        assert!(out.contains("from track 0 to track 0 @ 00:00:15.000"), "{out}");
        assert!(out.contains("clip #0"), "{out}");
        let t = &a.player.tracks()[0];
        assert_eq!(t.clips().len(), 2);
        assert_eq!(t.clips()[0].id, 1, "b leads at 10 s");
        assert_eq!(t.clips()[0].at, Duration::from_secs(10));
        assert_eq!(t.clips()[1].id, 0, "a keeps its id at 15 s");
        assert_eq!(t.clips()[1].at, Duration::from_secs(15));
    }

    #[test]
    fn move_defaults_pos_to_the_playhead_and_creates_the_destination_track() {
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", "a.wav,00:00:00-00:00:10"]); // track 0, #0
        run_ok(&mut a, &["seek", "00:00:04"]);
        let out = run_ok(&mut a, &["move", "0", "0", "3"]);
        assert!(out.contains("to track 3 @ 00:00:04.000"), "{out}");
        assert_eq!(a.player.tracks().len(), 4, "tracks up to the destination");
        let moved = &a.player.tracks()[3].clips()[0];
        assert_eq!(moved.id, 0, "fresh destination: id kept");
        assert_eq!(moved.at, Duration::from_secs(4));
    }

    #[test]
    fn move_takes_a_fresh_id_when_the_destination_already_has_it() {
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", "a.wav,00:00:00-00:00:10", "0@00:00:00"]); // #0 on 0
        run_ok(&mut a, &["put", "c.wav,00:00:00-00:00:03", "1@00:00:00"]); // #0 on 1
        let out = run_ok(&mut a, &["move", "0", "0", "1@00:00:05"]);
        assert!(out.contains("clip #1"), "reassigned on the destination: {out}");
        let t = &a.player.tracks()[1];
        assert_eq!(t.clips().len(), 2);
        assert_eq!(t.clips()[0].id, 0);
        assert_eq!(t.clips()[1].id, 1, "no id collision on the destination");
        assert!(a.player.tracks()[0].is_empty());
    }

    #[test]
    fn move_is_refused_atomically_when_the_destination_is_occupied() {
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", "a.wav,00:00:00-00:00:10"]); // #0 0..10 on 0
        run_ok(&mut a, &["put", "c.wav,00:00:00-00:00:03", "1@00:00:00"]); // #0 0..3 on 1
        let (code, msg) = run_err(&mut a, &["move", "0", "0", "1@00:00:01"]);
        assert_eq!(code, 1, "{msg}");
        assert!(
            msg.contains("move refused") && msg.contains("occupies [0.000,3.000)"),
            "{msg}"
        );
        assert_eq!(a.player.tracks()[0].clips().len(), 1, "source untouched");
        assert_eq!(a.player.tracks()[1].clips().len(), 1, "destination untouched");

        // A missing clip and a bad destination are refusal/usage errors too.
        let (code, msg) = run_err(&mut a, &["move", "0", "9", "1@00:00:00"]);
        assert_eq!(code, 1);
        assert!(msg.contains("no clip 0#9"), "{msg}");
        let (code, _) = run_err(&mut a, &["move", "0", "0", "bogus"]);
        assert_eq!(code, 2);
        let (code, _) = run_err(&mut a, &["move", "9", "0", "1@00:00:00"]);
        assert_eq!(code, 1, "no track 9");
    }

    #[test]
    fn move_takes_an_id_or_timecode_like_take_does() {
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", "a.wav,00:00:00-00:00:10"]);
        // By id.
        run_ok(&mut a, &["move", "0", "0", "1@00:00:00"]);
        assert!(a.player.tracks()[1].clips().iter().any(|c| c.id == 0));
        // By @timecode, back onto track 0.
        let out = run_ok(&mut a, &["move", "1", "@00:00:00", "0@00:00:00"]);
        assert!(out.contains("from track 1 to track 0"), "{out}");
        assert!(a.player.tracks()[0].clips().iter().any(|c| c.id == 0));
        assert!(a.player.tracks()[1].is_empty());
        // A bare word is neither: usage error.
        let (code, _) = run_err(&mut a, &["move", "0", "xyz", "1@00:00:00"]);
        assert_eq!(code, 2);
    }

    #[test]
    fn serialize_round_trips_names_volume_and_mute() {
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", "bed.wav,00:00:05-00:00:25"]);
        run_ok(&mut a, &["put", "ding.wav,00:00:01-00:00:02", "1@00:00:00"]);
        run_ok(&mut a, &["set", "track.0.name", "bed"]);
        run_ok(&mut a, &["set", "track.0.volume", "0.5"]);
        run_ok(&mut a, &["set", "track.0.pan", "-0.5"]);
        run_ok(&mut a, &["set", "track.1.muted", "true"]);
        run_ok(&mut a, &["set", "master", "0.78"]);

        let script = serialize(&a);
        assert!(script.contains("set track.0.name bed"), "{script}");
        assert!(script.contains("set track.0.volume 0.5"), "{script}");
        assert!(script.contains("set track.0.pan -0.5"), "{script}");
        assert!(script.contains("set track.1.muted true"), "{script}");
        assert!(script.contains("set master 0.78"), "master is saved: {script}");
        let mut fresh = Arrangement::default();
        run_script(&mut fresh, &script, "test", "").unwrap();
        assert_eq!(serialize(&fresh), script, "the script rebuilds the same arrangement");
    }

    #[test]
    fn put_and_set_carry_clip_gain_and_fade() {
        let mut a = Arrangement::default();
        let out = run_ok(
            &mut a,
            &[
                "put",
                "a.wav,0-10",
                "--gain",
                "0.5",
                "--fade-in",
                "0.6",
                "--fade-in-from",
                "0.3",
                "--fade-out",
                "3.2",
                "--fade-out-to",
                "0.2",
            ],
        );
        assert!(out.contains("clip #0"), "{out}");
        let t = &a.player.tracks()[0];
        assert_eq!(t.clips()[0].gain, 0.5);
        assert_eq!(t.clips()[0].fade.fade_in, Duration::from_millis(600));
        assert_eq!(t.clips()[0].fade.fade_in_from, 0.3);
        assert_eq!(t.clips()[0].fade.fade_out, Duration::from_millis(3200));
        assert_eq!(t.clips()[0].fade.fade_out_to, 0.2);

        let ls = run_ok(&mut a, &["ls"]);
        assert!(
            ls.contains("gain=0.50")
                && ls.contains("fade_in=00:00:00.600")
                && ls.contains("fade_in_from=0.30")
                && ls.contains("fade_out=00:00:03.200")
                && ls.contains("fade_out_to=0.20"),
            "{ls}"
        );

        // set tweaks them after the fact, addressed by clip id.
        let out = run_ok(&mut a, &["set", "clip.0.0.gain", "0.25"]);
        assert!(out.contains("`clip.0.0.gain` set to `0.25`"), "{out}");
        let out = run_ok(&mut a, &["set", "clip.0.0.fade_in", "1"]);
        assert!(out.contains("`clip.0.0.fade_in` set to `00:00:01.000`"), "{out}");
        assert_eq!(
            a.player.tracks()[0].clips()[0].fade.fade_in,
            Duration::from_secs(1)
        );
        let out = run_ok(&mut a, &["set", "clip.0.0.fade_out_to", "0.4"]);
        assert!(out.contains("`clip.0.0.fade_out_to` set to `0.40`"), "{out}");

        // Unknown shape is a usage error; a missing clip id is refused.
        let (code, _) = run_err(&mut a, &["set", "clip.0.0.fade_shape", "wavy"]);
        assert_eq!(code, 2);
        let (code, msg) = run_err(&mut a, &["set", "clip.0.9.gain", "0.5"]);
        assert_eq!(code, 1);
        assert!(msg.contains("no clip 0#9"), "{msg}");
    }

    #[test]
    fn serialize_round_trips_clip_gain_and_fade() {
        let mut a = Arrangement::default();
        run_ok(
            &mut a,
            &[
                "put",
                "bed.wav,0-10",
                "--gain",
                "0.5",
                "--fade-in",
                "0.6",
                "--fade-in-from",
                "0.3",
                "--fade-out",
                "3.2",
                "--fade-out-to",
                "0.2",
            ],
        );
        let script = serialize(&a);
        assert!(script.contains("--gain 0.5"), "{script}");
        assert!(script.contains("--fade-in 00:00:00.600"), "{script}");
        assert!(script.contains("--fade-in-from 0.3"), "{script}");
        assert!(script.contains("--fade-out 00:00:03.200"), "{script}");
        assert!(script.contains("--fade-out-to 0.2"), "{script}");
        let mut fresh = Arrangement::default();
        run_script(&mut fresh, &script, "test", "").unwrap();
        assert_eq!(serialize(&fresh), script, "the script rebuilds the same arrangement");
    }

    #[test]
    fn a_clips_own_placement_sets_shows_and_round_trips() {
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", "a.wav,0-5"]);
        run_ok(&mut a, &["put", "b.wav,0-5", "0@00:00:05"]);

        // Set one clip's own placement; the other follows the track.
        let out = run_ok(&mut a, &["set", "clip.0.1.pan", "-1"]);
        assert!(out.contains("`clip.0.1.pan` set to `-1.00`"), "{out}");
        assert_eq!(
            a.player.tracks()[0].clips()[1].placement.map(|p| p.position()),
            Some(-1.0)
        );
        assert_eq!(a.player.tracks()[0].clips()[0].placement, None, "clip 0 still follows");

        // ls shows the override on the clip line, and the reply clamps.
        let ls = run_ok(&mut a, &["ls"]);
        assert!(ls.contains("pan=-1.00"), "ls shows the override: {ls}");
        run_ok(&mut a, &["set", "clip.0.1.pan", "5"]);
        assert_eq!(
            a.player.tracks()[0].clips()[1].placement.map(|p| p.position()),
            Some(1.0),
            "clamped to the field"
        );
        run_ok(&mut a, &["set", "track.0.pan", "0.5"]);
        assert_eq!(a.player.tracks()[0].pan(), 0.5);

        // auto gives the clip back to its track.
        let out = run_ok(&mut a, &["set", "clip.0.1.pan", "auto"]);
        assert!(out.contains("`clip.0.1.pan` set to `auto`"), "{out}");
        assert_eq!(a.player.tracks()[0].clips()[1].placement, None);
        let ls = run_ok(&mut a, &["ls"]);
        assert!(!ls.contains("pan=-1.00"), "the override is gone: {ls}");

        // A saved arrangement carries the override and rebuilds with it.
        run_ok(&mut a, &["set", "clip.0.0.pan", "-0.5"]);
        let script = serialize(&a);
        assert!(script.contains("set clip.0.0.pan -0.5"), "{script}");
        let mut fresh = Arrangement::default();
        run_script(&mut fresh, &script, "test", "").unwrap();
        assert_eq!(serialize(&fresh), script, "round trip with the override");
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
        let mut a = Arrangement::with_backend(Runtime::Silent(
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
        assert!(reply.starts_with("ok: measured 00:00:01.000"), "{reply}");
        // A centered mono source shares its energy across the pair at −3.01
        // dB, so a 0.5-amplitude sine reads 9 dB under full scale.
        assert!(reply.contains("peak_db=-9.0"), "{reply}");
        assert!(reply.contains("rms_db=-12.0"), "{reply}");
        assert!(reply.contains("true_peak_db=-9.0"), "{reply}");
        assert!(reply.contains("loudest_1s=00:00:00.000"), "{reply}");
        assert!(reply.contains("note: span under 3s"), "under 3 s, no LUFS: {reply}");
        assert!(!reply.contains("integrated_lufs="), "no LUFS under 3 s: {reply}");
        // Nothing was written.
        assert!(!dir.join("out.wav").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn render_mono_folds_to_one_channel() {
        // A mono delivery: the file is one channel, and the measured levels
        // describe that fold. A centered mono source folds back to its own
        // level (both sides equal, so (L+R)/2 == L), still 9 dB under full
        // scale for a 0.5-amplitude sine.
        let dir = temp_dir();
        let src = dir.join("a.wav");
        write_test_wav(&src, 1.0, 0.5);
        let spec = format!("{},00:00:00-00:00:01", src.to_string_lossy());
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", &spec]);

        let out = dir.join("mono.wav");
        let cmd = parse_command(&[
            "render".to_string(),
            out.to_string_lossy().into_owned(),
            "--mono".to_string(),
        ])
        .unwrap();
        let reply = dispatch(&mut a, cmd, "").unwrap().to_string();
        assert!(reply.starts_with("ok: rendered"), "{reply}");
        let decoder =
            rodio::Decoder::new(std::io::BufReader::new(std::fs::File::open(&out).unwrap()))
                .unwrap();
        assert_eq!(decoder.channels().get(), 1, "one channel out");
        let peak = decoder.fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(
            (peak - 0.3535).abs() < 0.02,
            "centered mono keeps its level when folded: {peak}"
        );

        let cmd = parse_command(&["render".to_string(), "--measure".to_string(), "--mono".to_string()])
            .unwrap();
        let reply = dispatch(&mut a, cmd, "").unwrap().to_string();
        assert!(reply.contains("peak_db=-9.0"), "{reply}");
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
        assert!(reply.starts_with("ok: rendered"), "{reply}");
        assert!(reply.contains("rms_db="), "{reply}");
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
        assert!(out.contains("duration=00:00:00.200"), "{out}");
        assert!(out.contains("channels=1"), "probe reports the layout: {out}");
        assert_eq!(a.player.tracks().len(), 0, "a bare probe touches nothing");

        run_ok(&mut a, &["put", path.as_str()]);
        let out = run_ok(&mut a, &["probe"]);
        assert!(out.contains("ok: 1 source"), "{out}");
        assert!(out.contains("00:00:00.200"), "{out}");
        assert!(out.contains("channels=1"), "the arrangement probe reports the layout: {out}");

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
            thread::spawn(move || daemon_main_with(&socket, Runtime::silent()))
        };
        wait_until("socket", || UnixStream::connect(&socket).is_ok());

        let reply = send(&socket, "put a.wav,00:00:00-00:00:00.200");
        assert!(reply.contains("clip #0"), "{reply}");
        let reply = send(&socket, "put b.wav,00:00:00-00:00:00.200 0@00:00:00.200");
        assert!(reply.contains("clip #1"), "{reply}");

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

    /// A track holding one clip of `len` seconds, with no file behind it —
    /// enough to route, list and set against, since none of those decode.
    fn seed_track(uri: &str, len: u64) -> Track {
        let mut t = Track::new();
        t.insert(Clip::new(
            Arc::new(Source {
                uri: uri.to_string(),
            }),
            Duration::from_secs(len),
        ))
        .unwrap();
        t
    }

    #[test]
    fn route_creates_and_joins_buses_and_ls_shows_them() {
        let mut a = Arrangement::default();
        a.player.add_track(seed_track("bed.wav", 10));
        a.player.add_track(seed_track("voice.wav", 8));

        assert_eq!(
            run_ok(&mut a, &["route", "0", "music"]),
            "ok: track 0 routed to bus #0 'music' (1 track)\n"
        );
        assert_eq!(
            run_ok(&mut a, &["route", "1", "music"]),
            "ok: track 1 routed to bus #0 'music' (2 tracks)\n"
        );
        assert_eq!(a.player.groups().len(), 1, "the second route joined, not created");

        let ls = run_ok(&mut a, &["ls"]);
        assert!(ls.contains("bus #0 'music' vol=1.00 tracks=2\n"), "{ls}");
        assert!(
            ls.contains("track 0 untitled vol=1.00 pan=0.00 end=00:00:10.000 bus=#0 'music'\n"),
            "{ls}"
        );
        assert!(
            ls.contains("track 1 untitled vol=1.00 pan=0.00 end=00:00:08.000 bus=#0 'music'\n"),
            "{ls}"
        );

        // Back to the master: the bus stays, its membership drops.
        assert_eq!(
            run_ok(&mut a, &["route", "1", "master"]),
            "ok: track 1 routed to master\n"
        );
        let ls = run_ok(&mut a, &["ls"]);
        assert!(ls.contains("bus #0 'music' vol=1.00 tracks=1\n"), "{ls}");
        assert!(
            !ls.contains("end=00:00:08.000 bus="),
            "the second track no longer names a bus: {ls}"
        );
    }

    #[test]
    fn route_refuses_what_it_cannot_route() {
        let mut a = Arrangement::default();
        a.player.add_track(seed_track("a.wav", 1));
        let (code, msg) = run_err(&mut a, &["route", "5", "music"]);
        assert_eq!(code, 1);
        assert!(msg.contains("no track 5"), "{msg}");
        assert!(a.player.groups().is_empty(), "a bad track created no stray bus");
        let (_, msg) = run_err(&mut a, &["route", "0", "   "]);
        assert!(msg.contains("bus name"), "{msg}");
    }

    #[test]
    fn set_bus_edits_the_strip_and_guards_the_name() {
        let mut a = Arrangement::default();
        a.player.add_track(seed_track("a.wav", 10));
        run_ok(&mut a, &["route", "0", "music"]);

        assert_eq!(
            run_ok(&mut a, &["set", "bus.0.volume", "0.4"]),
            "ok: `bus.0.volume` set to `0.40`\n"
        );
        assert_eq!(run_ok(&mut a, &["set", "bus.0.muted", "true"]), "ok: `bus.0.muted` set to `true`\n");
        assert_eq!(
            run_ok(&mut a, &["set", "bus.0.name", "voice"]),
            "ok: `bus.0.name` set to `voice`\n"
        );
        let g = a.player.group(0).unwrap();
        assert_eq!(g.gain(), 0.4);
        assert!(g.muted());
        assert_eq!(g.name(), Some("voice"));

        // Names are what `route` addresses, so they must stay unique.
        a.player.add_group(Some("music".to_string()));
        let (code, msg) = run_err(&mut a, &["set", "bus.0.name", "music"]);
        assert_eq!(code, 1);
        assert!(msg.contains("already exists as #1"), "{msg}");
        let (_, msg) = run_err(&mut a, &["set", "bus.0.name", "master"]);
        assert!(msg.contains("reserved"), "{msg}");
        let (code, msg) = run_err(&mut a, &["set", "bus.9.volume", "0.5"]);
        assert_eq!(code, 1);
        assert!(msg.contains("no bus 9"), "{msg}");
    }

    #[test]
    fn a_bus_strip_change_waits_for_apply_while_playing() {
        let mut a = Arrangement::default();
        a.player.add_track(seed_track("a.wav", 10));
        run_ok(&mut a, &["route", "0", "music"]);
        run_ok(&mut a, &["play"]);

        // A group strip cannot land on the running graph: it waits, and an
        // apply rebuilds for it — the same note a clip move would give.
        let reply = run_ok(&mut a, &["set", "bus.0.volume", "0.5"]);
        assert!(reply.contains("note:"), "{reply}");
        assert_eq!(a.player.pending(), &[Change::GroupGain(0)]);
        let reply = run_ok(&mut a, &["apply"]);
        assert!(reply.contains("rebuilt"), "{reply}");
        assert!(a.player.pending().is_empty());
    }

    #[test]
    fn save_and_load_round_trips_buses_and_routing() {
        let dir = temp_dir();
        let file = dir.join("prog.bo");
        let path = file.to_string_lossy().into_owned();
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", "bed.wav,00:00:00-00:00:10"]);
        run_ok(&mut a, &["put", "voice.wav,00:00:00-00:00:08", "1@00:00:00"]);
        run_ok(&mut a, &["route", "0", "music"]);
        run_ok(&mut a, &["route", "1", "music"]);
        run_ok(&mut a, &["set", "bus.0.volume", "0.6"]);
        run_ok(&mut a, &["set", "bus.0.muted", "true"]);

        let script = serialize(&a);
        assert!(script.contains("route 0 'music'") || script.contains("route 0 music"), "{script}");
        assert!(script.contains("set bus.0.volume 0.6"), "{script}");
        assert!(script.contains("set bus.0.muted true"), "{script}");
        run_ok(&mut a, &["save", &path]);

        let mut b = Arrangement::default();
        run_ok(&mut b, &["load", &path]);
        assert_eq!(serialize(&b), script, "buses and routing round trip");
        assert_eq!(b.player.groups().len(), 1);
        let g = b.player.group(0).unwrap();
        assert_eq!(g.gain(), 0.6);
        assert!(g.muted());
        assert_eq!(
            b.player.tracks().iter().filter(|t| t.bus() == BusRef::Group(0)).count(),
            2,
            "both tracks came back routed"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_clip_curve_sets_shows_round_trips_and_clears() {
        let dir = temp_dir();
        let file = dir.join("prog.bo");
        let path = file.to_string_lossy().into_owned();
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", "bed.wav,00:00:00-00:00:10"]);
        assert_eq!(
            run_ok(&mut a, &["set", "clip.0.0.pan_control", "{\"type\":\"curve\",\"0\":1,\"2\":-1}"]),
            "ok: `clip.0.0.pan_control` set to `{\"type\":\"curve\",\"00:00:00.000\":1,\"00:00:02.000\":-1}`\n"
        );
        let ls = run_ok(&mut a, &["ls"]);
        assert!(ls.contains("pan_control={\"type\":\"curve\",\"00:00:00.000\":1,\"00:00:02.000\":-1}"), "{ls}");
        assert!(ls.contains("pan=0.00"), "{ls}");

        // The curve survives save/load as a set line (JSON, quoted).
        let script = serialize(&a);
        assert!(
            script.contains("set clip.0.0.pan_control '{\"type\":\"curve\",\"00:00:00.000\":1,\"00:00:02.000\":-1}'"),
            "{script}"
        );
        run_ok(&mut a, &["save", &path]);
        let mut b = Arrangement::default();
        run_ok(&mut b, &["load", &path]);
        assert_eq!(serialize(&b), script);
        assert_eq!(b.player.tracks()[0].clips()[0].pan_controls.len(), 1);

        // 'none' unplugs; the text round-trip forgets it again.
        assert_eq!(
            run_ok(&mut a, &["set", "clip.0.0.pan_control", "none"]),
            "ok: `clip.0.0.pan_control` set to `none`\n"
        );
        assert!(!serialize(&a).contains("curve"), "an unplugged curve is not saved");
        let (code, msg) = run_err(&mut a, &["set", "clip.0.0.pan_control", "nope"]);
        assert_eq!(code, 2, "{msg}");
        assert!(msg.contains("expected curve, lfo or sidechain"), "{msg}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_curve_set_while_playing_lands_live() {
        let mut a = Arrangement::default();
        a.player.add_track(seed_track("a.wav", 10));
        run_ok(&mut a, &["play"]);
        // A curve is a store into the running chain — no note, no apply.
        let reply = run_ok(&mut a, &["set", "clip.0.0.pan_control", "{\"type\":\"curve\",\"0\":1}"]);
        assert_eq!(reply, "ok: `clip.0.0.pan_control` set to `{\"type\":\"curve\",\"00:00:00.000\":1}`\n");
        assert!(a.player.pending().is_empty());
        assert_eq!(
            run_ok(&mut a, &["apply"]),
            "ok: nothing pending\n"
        );
    }

    #[test]
    fn a_gain_curve_sets_shows_round_trips_and_lands_live() {
        let dir = temp_dir();
        let file = dir.join("prog.bo");
        let path = file.to_string_lossy().into_owned();
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", "bed.wav,00:00:00-00:00:10"]);
        assert_eq!(
            run_ok(&mut a, &["set", "clip.0.0.gain_control", "{\"type\":\"curve\",\"0\":-0.5,\"10\":0}"]),
            "ok: `clip.0.0.gain_control` set to `{\"type\":\"curve\",\"00:00:00.000\":-0.5,\"00:00:10.000\":0}`\n"
        );
        let ls = run_ok(&mut a, &["ls"]);
        assert!(ls.contains("gain_control={\"type\":\"curve\",\"00:00:00.000\":-0.5,\"00:00:10.000\":0}"), "{ls}");

        let script = serialize(&a);
        assert!(
            script.contains("set clip.0.0.gain_control '{\"type\":\"curve\",\"00:00:00.000\":-0.5,\"00:00:10.000\":0}'"),
            "{script}"
        );
        run_ok(&mut a, &["save", &path]);
        let mut b = Arrangement::default();
        run_ok(&mut b, &["load", &path]);
        assert_eq!(serialize(&b), script);
        assert_eq!(b.player.tracks()[0].clips()[0].gain_controls.len(), 1);

        // Playing, a gain-curve edit is a store into the running chain.
        run_ok(&mut a, &["play"]);
        let reply = run_ok(&mut a, &["set", "clip.0.0.gain_control", "none"]);
        assert_eq!(reply, "ok: `clip.0.0.gain_control` set to `none`\n");
        assert!(a.player.pending().is_empty());
        assert!(!serialize(&a).contains("gain_control"), "unplugged is not saved");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_lfo_plugs_into_either_control_input() {
        let dir = temp_dir();
        let file = dir.join("prog.bo");
        let path = file.to_string_lossy().into_owned();
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", "bed.wav,00:00:00-00:00:10"]);
        assert_eq!(
            run_ok(&mut a, &["set", "clip.0.0.pan_control", "{\"type\":\"lfo\",\"shape\":\"sine\",\"rate\":1,\"depth\":0.5,\"phase\":0}"]),
            "ok: `clip.0.0.pan_control` set to `{\"type\":\"lfo\",\"shape\":\"sine\",\"rate\":1,\"depth\":0.5,\"phase\":0}`\n"
        );
        assert_eq!(
            run_ok(&mut a, &["set", "clip.0.0.gain_control", "{\"type\":\"lfo\",\"shape\":\"triangle\",\"rate\":0.5,\"depth\":0.3,\"phase\":0.25}"]),
            "ok: `clip.0.0.gain_control` set to `{\"type\":\"lfo\",\"shape\":\"triangle\",\"rate\":0.5,\"depth\":0.3,\"phase\":0.25}`\n"
        );
        let ls = run_ok(&mut a, &["ls"]);
        assert!(ls.contains("pan_control={\"type\":\"lfo\",\"shape\":\"sine\",\"rate\":1,\"depth\":0.5,\"phase\":0}"), "{ls}");
        assert!(
            ls.contains("gain_control={\"type\":\"lfo\",\"shape\":\"triangle\",\"rate\":0.5,\"depth\":0.3,\"phase\":0.25}"),
            "{ls}"
        );

        // The LFOs ride save/load as the same set lines.
        run_ok(&mut a, &["save", &path]);
        let mut b = Arrangement::default();
        run_ok(&mut b, &["load", &path]);
        assert_eq!(serialize(&b), serialize(&a));
        let clip = &b.player.tracks()[0].clips()[0];
        assert_eq!(clip.pan_controls.len(), 1);
        assert_eq!(clip.gain_controls.len(), 1);

        // A bad shape or a bad number is refused.
        let (code, msg) = run_err(&mut a, &["set", "clip.0.0.pan_control", "wibble"]);
        assert_eq!(code, 2, "{msg}");
        let (_, msg) = run_err(&mut a, &["set", "clip.0.0.pan_control", "{\"type\":\"lfo\",\"rate\":\"x\"}"]);
        assert!(msg.contains("rate"), "{msg}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_sidechain_plugs_in_and_round_trips() {
        let dir = temp_dir();
        let file = dir.join("prog.bo");
        let path = file.to_string_lossy().into_owned();
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", "bed.wav,00:00:00-00:00:10"]);
        assert_eq!(
            run_ok(&mut a, &["set", "clip.0.0.gain_control", "{\"type\":\"sidechain\",\"bus\":\"group.0\",\"amount\":-1.5,\"attack\":0.005,\"release\":0.12}"]),
            "ok: `clip.0.0.gain_control` set to `{\"type\":\"sidechain\",\"bus\":\"group.0\",\"amount\":-1.5,\"attack\":\"00:00:00.005\",\"release\":\"00:00:00.120\"}`\n"
        );
        let ls = run_ok(&mut a, &["ls"]);
        assert!(ls.contains("gain_control={\"type\":\"sidechain\",\"bus\":\"group.0\",\"amount\":-1.5,\"attack\":\"00:00:00.005\",\"release\":\"00:00:00.120\"}"), "{ls}");

        run_ok(&mut a, &["save", &path]);
        let mut b = Arrangement::default();
        run_ok(&mut b, &["load", &path]);
        assert_eq!(serialize(&b), serialize(&a));
        let clip = &b.player.tracks()[0].clips()[0];
        assert!(matches!(
            clip.gain_controls.first(),
            Some(ControlSource::Sidechain(..))
        ));

        // Playing, plugging a sidechain is a store, not a rebuild.
        run_ok(&mut a, &["play"]);
        let reply = run_ok(&mut a, &["set", "clip.0.0.gain_control", "none"]);
        assert_eq!(reply, "ok: `clip.0.0.gain_control` set to `none`\n");
        assert!(a.player.pending().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_bare_set_lists_every_current_var_and_value() {
        // A fresh arrangement lists only the master; the reply opens with
        // the same ok: status line as `ls`.
        let mut a = Arrangement::default();
        assert_eq!(run_ok(&mut a, &["set"]), "ok: 0 tracks, 0 clips, 0 buses\nmaster 1.00\n");

        // A named, muted track on a bus, with a faded clip and one source
        // plugged in: rows come from the same registry set applies through.
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", "bed.wav,00:00:00-00:00:10"]);
        run_ok(&mut a, &["set", "track.0.name", "bed"]);
        run_ok(&mut a, &["set", "track.0.volume", "0.4"]);
        run_ok(&mut a, &["set", "track.0.muted", "true"]);
        run_ok(&mut a, &["set", "clip.0.0.gain", "0.7"]);
        run_ok(&mut a, &["set", "clip.0.0.fade_in", "0.25"]);
        run_ok(&mut a, &["set", "clip.0.0.pan_control", "{\"type\":\"lfo\",\"shape\":\"sine\",\"rate\":1,\"depth\":0.5,\"phase\":0}"]);
        run_ok(&mut a, &["route", "0", "music"]);
        run_ok(&mut a, &["set", "bus.0.volume", "0.5"]);
        let listing = run_ok(&mut a, &["set"]);
        let first = "ok: 1 track, 1 clip, 1 bus\n";
        assert!(listing.starts_with(first), "{listing}");
        for row in [
            "master 1.00",
            "track.0.volume 0.40",
            "track.0.pan 0.00",
            "track.0.muted true",
            "track.0.name bed",
            "clip.0.0.gain 0.70",
            "clip.0.0.fade_in 00:00:00.250",
            "clip.0.0.pan_control {\"type\":\"lfo\",\"shape\":\"sine\",\"rate\":1,\"depth\":0.5,\"phase\":0}",
            "bus.0.volume 0.50",
            "bus.0.name music",
        ] {
            assert!(listing.contains(row), "missing {row:?} in:\n{listing}");
        }
        // Defaults are quiet: no muted-false row, no name row when unnamed,
        // no default-valued clip props.
        assert!(!listing.contains("track.0.muted false"), "{listing}");
        assert!(!listing.contains("clip.0.0.gain 1.00"), "{listing}");
        // The rows round-trip: every listed var can be set back to the value.
        for line in listing.lines().skip(1) {
            let (var, value) = line.split_once(' ').unwrap();
            let reply = run_ok(&mut a, &["set", var, value]);
            assert!(reply.starts_with(&format!("ok: `{var}` set to `{value}`")), "{reply}");
        }
    }

    #[test]
    fn a_one_argument_set_is_a_usage_error() {
        let mut a = Arrangement::default();
        let (code, msg) = run_err(&mut a, &["set", "track.0.volume"]);
        assert_eq!(code, 2, "{msg}");
        assert!(msg.contains("needs a <value>"), "{msg}");
    }

    #[test]
    fn unknown_vars_and_properties_name_the_key_space() {
        let mut a = Arrangement::default();
        let (code, msg) = run_err(&mut a, &["set", "masterr", "1"]);
        assert_eq!(code, 2, "{msg}");
        assert!(msg.contains("master"), "{msg}");
        assert!(msg.contains("`bo set` lists"), "{msg}");
        let (_, msg) = run_err(&mut a, &["set", "clip.0.0.vol", "0.5"]);
        assert!(msg.contains("try gain, pan, fade_in"), "{msg}");
        let (_, msg) = run_err(&mut a, &["set", "track.0.vol", "0.5"]);
        assert!(msg.contains("try volume, pan, muted, name"), "{msg}");
        let (_, msg) = run_err(&mut a, &["set", "bus.0.vol", "0.5"]);
        assert!(msg.contains("try volume, muted, name"), "{msg}");
    }
}
