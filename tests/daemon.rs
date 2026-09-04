//! End-to-end tests for the client ↔ daemon lifecycle, using the real `bo`
//! binary: the first command auto-spawns a daemon, later invocations reach
//! the same arrangement, and the daemon cleans up its socket when playback
//! finishes.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

fn bo(socket: &str, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_bo"))
        .env("BO_BACKEND", "silent")
        .arg("--socket")
        .arg(socket)
        .args(args)
        .output()
        .expect("bo runs");
    assert!(
        out.status.success(),
        "bo {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Run `bo` and return (exit code, stdout), without asserting success.
fn bo_exit(socket: &str, args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_bo"))
        .env("BO_BACKEND", "silent")
        .arg("--socket")
        .arg(socket)
        .args(args)
        .output()
        .expect("bo runs");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

fn temp_dir() -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "bo-e2e-test-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A tiny mono PCM wav with a sine at `amp` amplitude.
fn write_test_wav(path: &Path, seconds: f32, amp: f32) {
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

/// A mono 16-bit wav whose content changes every whole second: second `i` is
/// a sine at `freqs[i]`, so a render's audio identifies which second of the
/// source it really came from.
fn write_stepped_wav(path: &Path, freqs: &[f32]) {
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

/// Parse a bo render (stereo, 32-bit float, 44.1 kHz) as raw interleaved
/// samples, walking the RIFF chunks — deliberately not rodio, so the test
/// also proves the file bo wrote is really readable.
fn read_render_samples(path: &Path) -> Vec<f32> {
    let bytes = std::fs::read(path).unwrap();
    assert_eq!(&bytes[0..4], b"RIFF", "a riff header");
    assert_eq!(&bytes[8..12], b"WAVE", "a wave file");
    let mut off = 12usize;
    while off + 8 <= bytes.len() {
        let id = &bytes[off..off + 4];
        let size = u32::from_le_bytes(bytes[off + 4..off + 8].try_into().unwrap()) as usize;
        if id == b"data" {
            let mut out = Vec::with_capacity(size / 4);
            for chunk in bytes[off + 8..off + 8 + size].chunks_exact(4) {
                out.push(f32::from_le_bytes(chunk.try_into().unwrap()));
            }
            return out;
        }
        off += 8 + size + (size & 1); // chunks are word-aligned
    }
    panic!("no data chunk in {}", path.display());
}

/// Dominant frequency of the first whole second of a render, channel 0, by
/// zero-crossing count.
fn first_second_freq(samples: &[f32]) -> f32 {
    let ch0: Vec<f32> = samples.iter().step_by(2).take(44_100).copied().collect();
    let crossings = ch0
        .windows(2)
        .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
        .count();
    crossings as f32 / 2.0
}

/// A decodable wav whose header states no length (zero frames of float
/// audio): the stand-in for an mp3 without a Xing/Info frame.
fn write_empty_wav(path: &Path) {
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&36u32.to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&3u16.to_le_bytes()); // IEEE float
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&44_100u32.to_le_bytes());
    let byte_rate: u32 = 44_100 * 8;
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&8u16.to_le_bytes());
    wav.extend_from_slice(&32u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&0u32.to_le_bytes());
    std::fs::write(path, wav).unwrap();
}

fn wait_for_socket_gone(socket: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while socket.exists() {
        assert!(Instant::now() < deadline, "daemon did not clean up its socket");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn client_spawns_a_daemon_and_it_cleans_up_when_done() {
    let dir = temp_dir();
    let socket = dir.join("d.sock");
    let sp = socket.to_string_lossy().into_owned();

    // The first command auto-spawns the daemon.
    let out = bo(&sp, &["put", "a.wav,00:00:00-00:00:00.200"]);
    assert!(out.contains("clip #0"), "{out}");

    // A second invocation reaches the same daemon and its arrangement.
    let out = bo(&sp, &["put", "b.wav,00:00:00-00:00:00.200", "0@00:00:00.200"]);
    assert!(out.contains("clip #1"), "{out}");

    // The second command sees the same arrangement over the wire.
    let out = bo(&sp, &["ls"]);
    assert!(out.contains("track 0 untitled vol=1.00 end=00:00:00.400"), "{out}");
    let out = bo(&sp, &["set", "track.0.volume", "0.25"]);
    assert!(out.contains("`track.0.volume` set to `0.25`"), "{out}");

    // Play the 0.4s arrangement: the daemon exits and removes its socket.
    let out = bo(&sp, &["play"]);
    assert!(out.contains("playing from"), "{out}");
    let deadline = Instant::now() + Duration::from_secs(5);
    while socket.exists() {
        assert!(Instant::now() < deadline, "daemon did not clean up its socket");
        std::thread::sleep(Duration::from_millis(20));
    }

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn save_and_load_survive_a_daemon_restart() {
    let dir = temp_dir();
    let socket = dir.join("d.sock");
    let sp = socket.to_string_lossy().into_owned();
    let prog = dir.join("prog.bo");
    let ps = prog.to_string_lossy().into_owned();

    let out = bo(&sp, &["put", "a.wav,00:00:00-00:00:10"]);
    assert!(out.contains("on track 0"), "{out}");
    let out = bo(&sp, &["save", &ps]);
    assert!(out.contains("saved"), "{out}");
    let out = bo(&sp, &["stop"]);
    assert!(out.contains("stopped"), "{out}");
    wait_for_socket_gone(&socket);

    // A fresh daemon (spawned by load) restores the arrangement from the script.
    let out = bo(&sp, &["load", &ps]);
    assert!(out.contains("loaded"), "{out}");
    let out = bo(&sp, &["ls"]);
    assert!(out.contains("a.wav"), "{out}");

    let out = bo(&sp, &["stop"]);
    assert!(out.contains("stopped"), "{out}");
    wait_for_socket_gone(&socket);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn stop_ends_the_session() {
    let dir = temp_dir();
    let socket = dir.join("s.sock");
    let sp = socket.to_string_lossy().into_owned();

    let out = bo(&sp, &["put", "a.wav,00:00:00-00:00:10"]);
    assert!(out.contains("on track 0"), "{out}");
    let out = bo(&sp, &["stop"]);
    assert!(out.contains("stopped"), "{out}");

    let deadline = Instant::now() + Duration::from_secs(5);
    while socket.exists() {
        assert!(Instant::now() < deadline, "daemon did not clean up after stop");
        std::thread::sleep(Duration::from_millis(20));
    }

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn in_point_is_honored_end_to_end_over_the_wire() {
    let dir = temp_dir();
    let socket = dir.join("d.sock");
    let sp = socket.to_string_lossy().into_owned();
    let src = dir.join("steps.wav");
    write_stepped_wav(&src, &[440.0, 880.0, 1760.0]);
    let srcs = src.to_string_lossy().into_owned();

    // A slice of the source's 3rd second plays the 3rd second, not the 1st.
    let out = dir.join("out.wav");
    let outs = out.to_string_lossy().into_owned();
    let put = bo(&sp, &["put", &format!("{srcs},00:00:02-00:00:03")]);
    assert!(put.contains("00:00:02.000-00:00:03.000"), "{put}");
    let rendered = bo(&sp, &["render", &outs]);
    assert!(rendered.contains("00:00:01.000"), "{rendered}");
    let f = first_second_freq(&read_render_samples(&out));
    assert!(
        (f - 1760.0).abs() < 40.0,
        "in-point ignored: rendered {f:.0} Hz, want the source's 3rd second (1760)"
    );

    // Entered mid-way — the render range starts 1 s into a 1..3 s clip —
    // reading must start at from + offset (the 2 s mark), not at `from`.
    bo(&sp, &["reset"]);
    let put = bo(&sp, &["put", &format!("{srcs},00:00:01-00:00:03")]);
    assert!(put.contains("00:00:01.000-00:00:03.000"), "{put}");
    let out2 = dir.join("out2.wav");
    let out2s = out2.to_string_lossy().into_owned();
    let rendered = bo(&sp, &["render", &out2s, "00:00:01.000-00:00:02.000"]);
    assert!(rendered.contains("00:00:01.000"), "{rendered}");
    let f = first_second_freq(&read_render_samples(&out2));
    assert!(
        (f - 1760.0).abs() < 40.0,
        "mid-clip entry wrong: rendered {f:.0} Hz, want from+offset = 2 s (1760)"
    );

    let out = bo(&sp, &["stop"]);
    assert!(out.contains("stopped"), "{out}");
    wait_for_socket_gone(&socket);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn headerless_sources_probe_as_estimated_and_check_stays_ok() {
    let dir = temp_dir();
    let socket = dir.join("d.sock");
    let sp = socket.to_string_lossy().into_owned();
    let src = dir.join("empty.wav");
    write_empty_wav(&src);
    let srcs = src.to_string_lossy().into_owned();

    // A bare probe marks the estimate.
    let out = bo(&sp, &["probe", &srcs]);
    assert!(out.contains("duration=00:00:00.000 estimated"), "{out}");
    assert!(!socket.exists(), "a bare probe must not spawn a daemon");

    // Explicit out-points: put and check succeed; the source's missing
    // header length is a note, not a problem.
    let out = bo(&sp, &["put", &format!("{srcs},00:00:00-00:00:00.500")]);
    assert!(out.contains("clip #0"), "{out}");
    let out = bo(&sp, &["probe"]);
    assert!(out.contains("ok: 1 source"), "{out}");
    assert!(out.contains("duration=00:00:00.000 estimated"), "{out}");
    let out = bo(&sp, &["check"]);
    assert!(out.contains("ok: 1 clip, all sources ok"), "{out}");
    assert!(out.contains("note: 1 source with no header length"), "{out}");

    let out = bo(&sp, &["stop"]);
    assert!(out.contains("stopped"), "{out}");
    wait_for_socket_gone(&socket);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn move_rearranges_clips_over_the_wire() {
    let dir = temp_dir();
    let socket = dir.join("d.sock");
    let sp = socket.to_string_lossy().into_owned();

    let out = bo(&sp, &["put", "a.wav,00:00:00-00:00:10"]);
    assert!(out.contains("clip #0"), "{out}");
    let out = bo(&sp, &["put", "b.wav,00:00:00-00:00:05", "0@00:00:10"]);
    assert!(out.contains("clip #1"), "{out}");

    // a (#0) moves to a fresh track 1; the id survives.
    let out = bo(&sp, &["move", "0", "0", "1@00:00:03"]);
    assert!(
        out.contains("from track 0 to track 1 @ 00:00:03.000") && out.contains("clip #0"),
        "{out}"
    );
    let out = bo(&sp, &["ls"]);
    assert!(out.contains("ok: 2 tracks"), "{out}");
    assert!(out.contains("@ 00:00:10.000") && out.contains("@ 00:00:03.000"), "{out}");

    // A refused move leaves both tracks as they were: track 0's b still
    // occupies 10..15, so a landing inside it collides.
    let out = bo_exit(&sp, &["move", "1", "0", "0@00:00:12"]);
    assert_eq!(out.0, 1, "{out:?}");
    assert!(out.1.contains("move refused"), "{out:?}");

    let out = bo(&sp, &["stop"]);
    assert!(out.contains("stopped"), "{out}");
    wait_for_socket_gone(&socket);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn ls_reports_the_idle_timeout_the_daemon_was_started_with() {
    let dir = temp_dir();
    let socket = dir.join("d.sock");
    let sp = socket.to_string_lossy().into_owned();

    // A daemon started with a short idle timeout says so on ls, so a quiet
    // session cannot silently time out.
    let out = Command::new(env!("CARGO_BIN_EXE_bo"))
        .env("BO_BACKEND", "silent")
        .env("BO_IDLE_TIMEOUT", "3")
        .arg("--socket")
        .arg(&sp)
        .args(["put", "a.wav,00:00:00-00:00:10"])
        .output()
        .expect("bo runs");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let out = bo(&sp, &["ls"]);
    assert!(out.contains("idle_timeout=00:00:03.000"), "{out}");

    let out = bo(&sp, &["stop"]);
    assert!(out.contains("stopped"), "{out}");
    wait_for_socket_gone(&socket);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn probe_measures_locally_without_a_daemon_and_over_the_wire() {
    let dir = temp_dir();
    let socket = dir.join("d.sock");
    let sp = socket.to_string_lossy().into_owned();
    let src = dir.join("a.wav");
    write_test_wav(&src, 0.2, 0.5);
    let spec = format!("{},00:00:00-00:00:00.200", src.to_string_lossy());

    // A bare uri is probed in the client: no daemon is spawned.
    let out = bo(&sp, &["probe", src.to_str().unwrap()]);
    assert!(out.contains("duration=00:00:00.200"), "{out}");
    assert!(!socket.exists(), "a bare probe must not spawn a daemon");

    // The arrangement's sources are probed over the wire.
    let out = bo(&sp, &["put", spec.as_str()]);
    assert!(out.contains("on track 0"), "{out}");
    let out = bo(&sp, &["probe"]);
    assert!(out.contains("ok: 1 source"), "{out}");
    assert!(out.contains("00:00:00.200"), "{out}");

    let out = bo(&sp, &["stop"]);
    assert!(out.contains("stopped"), "{out}");
    wait_for_socket_gone(&socket);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn play_on_an_empty_arrangement_is_refused_and_the_daemon_survives() {
    let dir = temp_dir();
    let socket = dir.join("d.sock");
    let sp = socket.to_string_lossy().into_owned();

    // Spawn a daemon, then empty the arrangement out from under it.
    let out = bo(&sp, &["put", "a.wav,00:00:00-00:00:10"]);
    assert!(out.contains("on track 0"), "{out}");
    let out = bo(&sp, &["take", "0", "0"]);
    assert!(out.contains("removed"), "{out}");

    // play on nothing: refused with exit 1, not a fake success.
    let (code, out) = bo_exit(&sp, &["play"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("no clips: nothing to play"), "{out}");

    // The daemon is still alive: the session did not vanish.
    let out = bo(&sp, &["ls"]);
    assert!(out.contains("stopped, playhead at"), "{out}");

    let out = bo(&sp, &["stop"]);
    assert!(out.contains("stopped"), "{out}");
    wait_for_socket_gone(&socket);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn quoted_arguments_survive_the_wire_and_scripts() {
    let dir = temp_dir();
    let socket = dir.join("d.sock");
    let sp = socket.to_string_lossy().into_owned();
    let src = dir.join("Bo FM.wav");
    write_test_wav(&src, 0.2, 0.5);
    let spec = format!("{},00:00:00-00:00:00.200", src.to_string_lossy());

    // A space-bearing path arrives as one argument over the wire.
    let out = bo(&sp, &["put", &spec]);
    assert!(out.contains("on track 0"), "{out}");
    let out = bo(&sp, &["ls"]);
    assert!(out.contains("Bo FM.wav"), "{out}");

    // Multi-word names survive too.
    let out = bo(&sp, &["set", "track.0.name", "bed soft"]);
    assert!(out.contains("bed soft"), "{out}");
    let out = bo(&sp, &["ls"]);
    assert!(out.contains("'bed soft'"), "{out}");

    // save writes quoted lines; a fresh daemon's load restores them.
    let prog = dir.join("show plan.bo");
    let ps = prog.to_string_lossy().into_owned();
    let out = bo(&sp, &["save", &ps]);
    assert!(out.contains("saved"), "{out}");
    let script = std::fs::read_to_string(&prog).unwrap();
    assert!(script.contains("Bo FM.wav',00:00:00.000-00:00:00.200"), "quoted in the script: {script}");
    let out = bo(&sp, &["stop"]);
    assert!(out.contains("stopped"), "{out}");
    wait_for_socket_gone(&socket);

    let out = bo(&sp, &["load", &ps]);
    assert!(out.contains("loaded"), "{out}");
    let out = bo(&sp, &["ls"]);
    assert!(out.contains("Bo FM.wav") && out.contains("bed soft"), "{out}");

    let out = bo(&sp, &["stop"]);
    assert!(out.contains("stopped"), "{out}");
    wait_for_socket_gone(&socket);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn at_reports_the_mix_at_a_timecode_over_the_wire() {
    let dir = temp_dir();
    let socket = dir.join("d.sock");
    let sp = socket.to_string_lossy().into_owned();

    let out = bo(&sp, &["put", "a.wav,00:00:00-00:00:10"]);
    assert!(out.contains("on track 0"), "{out}");
    let out = bo(&sp, &["put", "b.wav,00:00:00-00:00:03", "1@00:00:05"]);
    assert!(out.contains("on track 1"), "{out}");

    let out = bo(&sp, &["at", "00:00:06.000"]);
    assert!(out.contains("track 0: clip #0"), "{out}");
    assert!(out.contains("track 1: clip #0"), "{out}");
    let out = bo(&sp, &["at", "00:00:20.000"]);
    assert!(out.contains("silent at"), "{out}");

    let out = bo(&sp, &["stop"]);
    assert!(out.contains("stopped"), "{out}");
    wait_for_socket_gone(&socket);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn reset_clears_the_arrangement_for_a_script_replay() {
    let dir = temp_dir();
    let socket = dir.join("d.sock");
    let sp = socket.to_string_lossy().into_owned();
    let prog = dir.join("prog.bo");
    let ps = prog.to_string_lossy().into_owned();

    // Build a two-track mix and save it as a session script.
    let out = bo(&sp, &["put", "a.wav,00:00:00-00:00:10"]);
    assert!(out.contains("on track 0"), "{out}");
    let out = bo(&sp, &["put", "b.wav,00:00:00-00:00:05"]);
    assert!(out.contains("on track 1"), "{out}");
    let out = bo(&sp, &["save", &ps]);
    assert!(out.contains("saved"), "{out}");

    // Reset, then replay: the arrangement is rebuilt, not duplicated.
    let out = bo(&sp, &["reset"]);
    assert!(out.contains("ok: 2 tracks removed"), "{out}");
    let out = bo(&sp, &["load", &ps]);
    assert!(out.contains("loaded"), "{out}");
    let out = bo(&sp, &["ls"]);
    assert!(out.contains("ok: 2 tracks,"), "{out}");
    assert_eq!(out.matches("clip #").count(), 2, "no duplicates after reset + replay: {out}");

    let out = bo(&sp, &["stop"]);
    assert!(out.contains("stopped"), "{out}");
    wait_for_socket_gone(&socket);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn put_repeat_places_butt_joined_copies_over_the_wire() {
    let dir = temp_dir();
    let socket = dir.join("d.sock");
    let sp = socket.to_string_lossy().into_owned();

    let out = bo(&sp, &["put", "--repeat", "3", "crackle.wav,00:00:00-00:00:12"]);
    assert_eq!(out.matches("clip #").count(), 3, "{out}");
    let out = bo(&sp, &["ls"]);
    assert_eq!(out.matches("clip #").count(), 3, "{out}");
    assert!(out.contains("@ 00:00:12.000") && out.contains("@ 00:00:24.000"), "{out}");

    let out = bo(&sp, &["stop"]);
    assert!(out.contains("stopped"), "{out}");
    wait_for_socket_gone(&socket);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn idle_timeout_cleans_up_a_quiet_paused_daemon() {
    let dir = temp_dir();
    let socket = dir.join("d.sock");
    let sp = socket.to_string_lossy().into_owned();

    // Spawn a daemon with a 1-second idle timeout, then go quiet.
    let out = Command::new(env!("CARGO_BIN_EXE_bo"))
        .env("BO_BACKEND", "silent")
        .env("BO_IDLE_TIMEOUT", "1")
        .arg("--socket")
        .arg(&sp)
        .args(["put", "a.wav,00:00:00-00:00:10"])
        .output()
        .expect("bo runs");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    // No further commands: the daemon exits and removes its socket.
    let deadline = Instant::now() + Duration::from_secs(5);
    while socket.exists() {
        assert!(Instant::now() < deadline, "quiet daemon did not time out");
        std::thread::sleep(Duration::from_millis(20));
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn take_addresses_clips_by_timecode_over_the_wire() {
    let dir = temp_dir();
    let socket = dir.join("d.sock");
    let sp = socket.to_string_lossy().into_owned();

    let out = bo(&sp, &["put", "a.wav,00:00:00-00:00:10"]);
    assert!(out.contains("clip #0"), "{out}");
    let out = bo(&sp, &["put", "b.wav,00:00:00-00:00:05", "0@00:00:10"]);
    assert!(out.contains("clip #1"), "{out}");

    // Delete by timecode: b covers 00:00:12, so it goes — by its stable id.
    let out = bo(&sp, &["take", "0", "@00:00:12"]);
    assert!(out.contains("removed 1 clip from track 0") && out.contains("clip #1") && out.contains("b.wav"), "{out}");

    // Ids are stable: a is still id 0 even though b is gone.
    let out = bo(&sp, &["take", "0", "0"]);
    assert!(out.contains("removed 1 clip from track 0") && out.contains("clip #0") && out.contains("a.wav"), "{out}");

    let out = bo(&sp, &["stop"]);
    assert!(out.contains("stopped"), "{out}");
    wait_for_socket_gone(&socket);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn relative_paths_resolve_against_the_invocation_cwd() {
    let dir = temp_dir();
    let socket = dir.join("d.sock");
    let sp = socket.to_string_lossy().into_owned();

    // A relative source resolves against this command's cwd, not the
    // daemon's: the daemon is spawned from the same cwd here, but the client
    // sends the cwd explicitly so a long-lived daemon never leaks its own.
    let run_in = |args: &[&str]| -> String {
        let out = Command::new(env!("CARGO_BIN_EXE_bo"))
            .env("BO_BACKEND", "silent")
            .current_dir(&dir)
            .arg("--socket")
            .arg(&sp)
            .args(args)
            .output()
            .expect("bo runs");
        assert!(
            out.status.success(),
            "bo {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };

    let out = run_in(&["put", "a.wav,00:00:00-00:00:10"]);
    assert!(out.contains("clip #0"), "{out}");

    let out = run_in(&["ls"]);
    // current_dir() returns the canonical path (macOS: /private/var/...), so
    // compare against the canonicalized dir, not the symlinky temp_dir().
    let resolved = std::fs::canonicalize(&dir).unwrap();
    let expected = format!("'{}/a.wav'", resolved.to_string_lossy());
    assert!(out.contains(&expected), "ls should show the absolute uri: {out}");

    let out = run_in(&["stop"]);
    assert!(out.contains("stopped"), "{out}");
    wait_for_socket_gone(&socket);
    std::fs::remove_dir_all(&dir).ok();
}
