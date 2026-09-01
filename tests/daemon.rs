//! End-to-end tests for the client ↔ daemon lifecycle, using the real `bo`
//! binary: the first command auto-spawns a daemon, later invocations reach
//! the same arrangement, and the daemon cleans up its socket when the program
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
    let out = bo(&sp, &["put", "a.wav:00:00:00-00:00:00.200"]);
    assert!(out.contains("ok: track 0 clip #0"), "{out}");

    // A second invocation reaches the same daemon and its arrangement.
    let out = bo(&sp, &["put", "b.wav@00:00:00.200:00:00:00-00:00:00.200", "0"]);
    assert!(out.contains("ok: track 0 clip #1"), "{out}");

    // The second command sees the same arrangement over the wire.
    let out = bo(&sp, &["ls"]);
    assert!(out.contains("track 0: volume=1.00 clips=2"), "{out}");
    let out = bo(&sp, &["volume", "0", "0.25"]);
    assert!(out.contains("track 0 volume 0.25"), "{out}");

    // Play the 0.4s program: the daemon exits and removes its socket.
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

    let out = bo(&sp, &["put", "a.wav:00:00:00-00:00:10"]);
    assert!(out.contains("ok: track 0"), "{out}");
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

    let out = bo(&sp, &["put", "a.wav:00:00:00-00:00:10"]);
    assert!(out.contains("ok: track 0"), "{out}");
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
fn probe_measures_locally_without_a_daemon_and_over_the_wire() {
    let dir = temp_dir();
    let socket = dir.join("d.sock");
    let sp = socket.to_string_lossy().into_owned();
    let src = dir.join("a.wav");
    write_test_wav(&src, 0.2, 0.5);
    let spec = format!("{}:00:00:00-00:00:00.200", src.to_string_lossy());

    // A bare uri is probed in the client: no daemon is spawned.
    let out = bo(&sp, &["probe", src.to_str().unwrap()]);
    assert!(out.contains("probe:") && out.contains("00:00:00.200"), "{out}");
    assert!(!socket.exists(), "a bare probe must not spawn a daemon");

    // The arrangement's sources are probed over the wire.
    let out = bo(&sp, &["put", spec.as_str()]);
    assert!(out.contains("ok: track 0"), "{out}");
    let out = bo(&sp, &["probe"]);
    assert!(out.contains("probe: 1 source"), "{out}");
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
    let out = bo(&sp, &["put", "a.wav:00:00:00-00:00:10"]);
    assert!(out.contains("ok: track 0"), "{out}");
    let out = bo(&sp, &["take", "0", "0"]);
    assert!(out.contains("removed"), "{out}");

    // play on nothing: refused with exit 1, not a fake success.
    let (code, out) = bo_exit(&sp, &["play"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("no clips: nothing to play"), "{out}");

    // The daemon is still alive: the session did not vanish.
    let out = bo(&sp, &["ls"]);
    assert!(out.contains("state: stopped"), "{out}");

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
    let spec = format!("{}:00:00:00-00:00:00.200", src.to_string_lossy());

    // A space-bearing path arrives as one argument over the wire.
    let out = bo(&sp, &["put", &spec]);
    assert!(out.contains("ok: track 0"), "{out}");
    let out = bo(&sp, &["ls"]);
    assert!(out.contains("Bo FM.wav"), "{out}");

    // Multi-word names survive too.
    let out = bo(&sp, &["name", "0", "bed soft"]);
    assert!(out.contains("bed soft"), "{out}");
    let out = bo(&sp, &["ls"]);
    assert!(out.contains("name=bed soft"), "{out}");

    // save writes quoted lines; a fresh daemon's load restores them.
    let prog = dir.join("show plan.bo");
    let ps = prog.to_string_lossy().into_owned();
    let out = bo(&sp, &["save", &ps]);
    assert!(out.contains("saved"), "{out}");
    let script = std::fs::read_to_string(&prog).unwrap();
    assert!(script.contains("Bo FM.wav'@"), "quoted in the script: {script}");
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

    let out = bo(&sp, &["put", "a.wav:00:00:00-00:00:10"]);
    assert!(out.contains("ok: track 0"), "{out}");
    let out = bo(&sp, &["put", "b.wav@00:00:05:00:00:00-00:00:03", "1"]);
    assert!(out.contains("ok: track 1"), "{out}");

    let out = bo(&sp, &["at", "00:00:06.000"]);
    assert!(out.contains("track 0: clip=0"), "{out}");
    assert!(out.contains("track 1: clip=0"), "{out}");
    let out = bo(&sp, &["at", "00:00:20.000"]);
    assert!(out.contains("silent at"), "{out}");

    let out = bo(&sp, &["stop"]);
    assert!(out.contains("stopped"), "{out}");
    wait_for_socket_gone(&socket);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn take_addresses_clips_by_timecode_over_the_wire() {
    let dir = temp_dir();
    let socket = dir.join("d.sock");
    let sp = socket.to_string_lossy().into_owned();

    let out = bo(&sp, &["put", "a.wav:00:00:00-00:00:10"]);
    assert!(out.contains("ok: track 0 clip #0"), "{out}");
    let out = bo(&sp, &["put", "b.wav@00:00:10:00:00:00-00:00:05", "0"]);
    assert!(out.contains("ok: track 0 clip #1"), "{out}");

    // Delete by timecode: b covers 00:00:12, so it goes — by its stable id.
    let out = bo(&sp, &["take", "0", "@00:00:12"]);
    assert!(out.contains("removed track 0 clip #1 b.wav"), "{out}");

    // Ids are stable: a is still id 0 even though b is gone.
    let out = bo(&sp, &["take", "0", "0"]);
    assert!(out.contains("removed track 0 clip #0 a.wav"), "{out}");

    let out = bo(&sp, &["stop"]);
    assert!(out.contains("stopped"), "{out}");
    wait_for_socket_gone(&socket);
    std::fs::remove_dir_all(&dir).ok();
}
