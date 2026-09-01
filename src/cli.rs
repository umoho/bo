//! The agent-facing CLI, one command so far: `put`.
//!
//! `bo put <spec> [track]` places a clip on a track. Without `[track]` a new
//! track is created and its index is printed, so later puts can name it. With
//! `[track]` the track is used, created on demand (up to that index) so that
//! state scripts rebuild the exact same layout.
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
//! # State
//!
//! Every successful `put` is persisted to a state file — `bo.state` in the
//! current directory, overridable with `--state <path>` or the `BO_STATE`
//! environment variable. The file holds the very commands that rebuild the
//! arrangement, so it doubles as a readable script.
//!
//! A clip with no known end (an unsliced, unprobed source) is open-ended: it
//! blocks everything after it on the same track, which is how the engine makes
//! overlap failures predictable. Slice what you place (`uri:from-to`) to keep
//! arranging on one track.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bo::engine::{Player, Silent};
use bo::track::{Clip, Source, Track};

/// Default state file, when neither `--state` nor `$BO_STATE` says otherwise.
const DEFAULT_STATE: &str = "bo.state";

const USAGE: &str = "\
bo put <spec> [track]

place a clip on a track. Without [track], a new track is created and its
index is printed, so later puts can name it:

  bo put bed.wav            -> creates track 0, prints it
  bo put jingle.wav 0       -> onto the track from above

<spec> is uri[@at][:from-to]:
  at      position on the track; default 0
  from-to slice of the source; an empty to (from-) plays to the source's end
  timecodes are SS, MM:SS or HH:MM:SS, optional .fff fraction

examples:
  bo put bed.wav
  bo put bed.wav@00:30:00
  bo put jingle.wav:00:00:10-00:00:25
  bo put news.wav@00:30:00:00:00:05-00:01:00
";

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

/// Execute one command line against the arrangement.
fn run_command(a: &mut Arrangement, args: &[String]) -> Result<String, (i32, String)> {
    match args.first().map(String::as_str) {
        Some("put") => put_command(a, args),
        Some(cmd) => Err(usage(format!("unknown command {cmd:?} — `bo put` is all there is"))),
        None => Err(usage("missing command")),
    }
}

/// `put <spec> [track]`: place a clip, creating the track when needed.
///
/// The identifier in the output is the contract: an implicit put creates a
/// fresh track and prints its index; an explicit one names a track, created on
/// demand so that state scripts rebuild the same layout.
fn put_command(a: &mut Arrangement, args: &[String]) -> Result<String, (i32, String)> {
    let (spec_arg, want_track) = parse_put_args(args)?;
    let spec = parse_spec(&spec_arg).map_err(usage)?;
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

/// Pull the clip spec and the optional positional `[track]` out of a `put`
/// line.
fn parse_put_args(args: &[String]) -> Result<(String, Option<usize>), (i32, String)> {
    let spec = args.get(1).ok_or_else(|| usage("missing clip spec"))?;
    let track = match args.get(2) {
        Some(s) => Some(s.parse().map_err(|_| usage(format!("bad track index {s:?}")))?),
        None => None,
    };
    if args.len() > 3 {
        return Err(usage("too many arguments"));
    }
    Ok((spec.clone(), track))
}

/// The state file as a script: the commands that rebuild the arrangement.
fn serialize(a: &Arrangement) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# bo arrangement v1");
    for (ti, t) in a.player.tracks().iter().enumerate() {
        for c in t.clips() {
            let to = c.to.map(format_time).unwrap_or_default();
            let _ = writeln!(
                out,
                "put {}@{}:{}-{} {ti}",
                c.source.uri,
                format_time(c.at),
                format_time(c.from),
                to
            );
        }
    }
    out
}

/// Execute a script (the state file) into the arrangement.
///
/// Stops at the first failing line and reports `src:line: message`; what ran
/// before the failure stays applied.
fn run_script(a: &mut Arrangement, text: &str, src: &str) -> Result<(), String> {
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let args: Vec<String> = line.split_whitespace().map(str::to_string).collect();
        match run_command(a, &args) {
            Ok(_) => {}
            Err((_, msg)) => return Err(format!("{src}:{}: {msg}", n + 1)),
        }
    }
    Ok(())
}

fn load_into(a: &mut Arrangement, path: &Path) -> Result<(), String> {
    match std::fs::read_to_string(path) {
        Ok(text) => run_script(a, &text, &path.display().to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

fn write_state(a: &Arrangement, path: &Path) -> Result<(), String> {
    std::fs::write(path, serialize(a)).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// Entry point for `main`: parse global options, load the state file, run one
/// command, persist if it mutated the arrangement. Returns the exit code.
pub fn run(args: Vec<String>) -> i32 {
    let mut state_path = std::env::var("BO_STATE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_STATE));
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--state" => {
                i += 1;
                let Some(path) = args.get(i) else {
                    eprintln!("bo: --state needs a path");
                    return 2;
                };
                state_path = PathBuf::from(path);
                i += 1;
            }
            s if let Some(path) = s.strip_prefix("--state=") => {
                state_path = PathBuf::from(path);
                i += 1;
            }
            "--help" | "-h" => {
                println!("{USAGE}");
                return 0;
            }
            s if s.starts_with('-') => {
                eprintln!("bo: unknown option {s:?}");
                return 2;
            }
            _ => break,
        }
    }
    let command: Vec<String> = args[i..].to_vec();
    if command.is_empty() {
        println!("{USAGE}");
        return 0;
    }

    let mut arrangement = Arrangement::default();
    if let Err(msg) = load_into(&mut arrangement, &state_path) {
        eprintln!("bo: state file {msg}");
        return 1;
    }

    let out = match run_command(&mut arrangement, &command) {
        Ok(out) => out,
        Err((code, msg)) => {
            eprintln!("bo: {msg}");
            return code;
        }
    };
    if let Err(msg) = write_state(&arrangement, &state_path) {
        eprintln!("bo: {msg}");
        return 1;
    }
    print!("{out}");
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn run_ok(a: &mut Arrangement, args: &[&str]) -> String {
        let v: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        match run_command(a, &v) {
            Ok(out) => out,
            Err((code, msg)) => panic!("command {args:?} failed ({code}): {msg}"),
        }
    }

    fn run_err(a: &mut Arrangement, args: &[&str]) -> (i32, String) {
        let v: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        run_command(a, &v).unwrap_err()
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
    fn serialize_round_trips() {
        let mut a = Arrangement::default();
        run_ok(&mut a, &["put", "bed.wav:00:00:05-00:00:25"]);
        run_ok(&mut a, &["put", "ding.wav@00:00:00:00:00:01-00:00:02", "1"]);
        run_ok(&mut a, &["put", "live.wav", "2"]);

        let script = serialize(&a);
        let mut fresh = Arrangement::default();
        run_script(&mut fresh, &script, "test").unwrap();
        assert_eq!(serialize(&fresh), script, "the script rebuilds the same arrangement");
        assert_eq!(fresh.player.tracks().len(), 3);
    }

    #[test]
    fn run_persists_state_between_invocations() {
        let dir = temp_dir();
        let state = dir.join("state.bo");
        let sp = state.to_string_lossy().into_owned();

        let code = run(vec!["--state".into(), sp.clone(), "put".into(), "bed.wav:00:00:00-00:00:30".into()]);
        assert_eq!(code, 0);
        // The printed identifier is used by a later invocation.
        let code = run(vec![
            "--state".into(),
            sp.clone(),
            "put".into(),
            "jingle.wav@00:00:30:00:00:00-00:00:10".into(),
            "0".into(),
        ]);
        assert_eq!(code, 0);
        let text = std::fs::read_to_string(&state).unwrap();
        assert!(text.contains("put bed.wav@00:00:00.000:00:00:00.000-00:00:30.000 0"), "{text}");
        assert!(text.contains("put jingle.wav@00:00:30.000:00:00:00.000-00:00:10.000 0"), "{text}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn usage_errors_exit_with_code_two() {
        let mut a = Arrangement::default();
        assert_eq!(run_err(&mut a, &["nope"]).0, 2);
        assert_eq!(run_err(&mut a, &["put"]).0, 2);
        assert_eq!(run_err(&mut a, &["put", "a.wav", "x"]).0, 2, "bad track index");
        assert_eq!(run_err(&mut a, &["put", "a.wav", "0", "extra"]).0, 2, "too many arguments");
    }
}
