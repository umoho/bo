//! End-to-end tests for the mini CLI — `play pause resume seek stop load` —
//! over the real `bo` binary: the daemon is spawned on demand, the CLI
//! shares its session with the typed clients (arrangement building happens
//! there, not in the CLI), and `stop`/playback-finish clean the socket up.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Once;
use std::time::{Duration, Instant};

use bo::client::{Bo, Clip, TimecodeRange, TrackIndex};
use bo::connection::Connection;

static ENV: Once = Once::new();

/// Point every daemon at the freshly built binary and force the silent
/// backend, once per process (the values never change, so parallel tests
/// cannot race each other into a wrong state).
fn env_once() {
    ENV.call_once(|| {
        unsafe {
            std::env::set_var("BO_BACKEND", "silent");
            std::env::set_var("BO_DAEMON", env!("CARGO_BIN_EXE_bo"));
        }
    });
}

/// Run the CLI and return (exit code, stdout).
fn bo_exit(socket: &Path, args: &[&str]) -> (i32, String) {
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

/// Run the CLI, asserting success.
fn bo(socket: &Path, args: &[&str]) -> String {
    let (code, out) = bo_exit(socket, args);
    assert_eq!(code, 0, "bo {args:?} failed: {out}");
    out
}

fn temp_dir() -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "bo-cli-e2e-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn wait_for_socket_gone(socket: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while socket.exists() {
        assert!(Instant::now() < deadline, "daemon did not clean up its socket");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A snapshot of a built arrangement, saved by a typed client: the CLI's
/// only way to *get* an arrangement is `load` of one of these.
fn seed_snapshot(dir: &Path, clips: usize) -> PathBuf {
    let socket = dir.join("d.sock");
    let mut bo = Bo::with_connection(Connection::at(&socket));
    for track in 0..clips {
        bo.put(
            Clip::of("a.wav").trim(TimecodeRange::from((
                Duration::ZERO,
                Duration::from_secs(30),
            ))),
            TrackIndex(track).at(Duration::ZERO),
        )
        .expect("the client seeds the session");
    }
    let snapshot = dir.join("prog.bo");
    bo.save(&snapshot).expect("save writes a snapshot");
    bo.stop().expect("stop ends the seeded session");
    wait_for_socket_gone(&socket);
    snapshot
}

#[test]
fn the_cli_spawns_a_daemon_and_transport_verbs_drive_it() {
    env_once();
    let dir = temp_dir();
    let socket = dir.join("d.sock");
    let snapshot = seed_snapshot(&dir, 2);

    // The first verb spawns the daemon and restores the seeded session.
    let out = bo(&socket, &["load", &snapshot.to_string_lossy()]);
    assert!(out.contains("ok: loaded"), "{out}");

    // Transport verbs answer from the same arrangement.
    let out = bo(&socket, &["play"]);
    assert!(
        out.contains("ok: 2 tracks, 2 clips") && out.contains("playing from 00:00:00.000"),
        "{out}"
    );
    let out = bo(&socket, &["seek", "00:00:05"]);
    assert!(out.contains("playhead at 00:00:05.000"), "{out}");
    let out = bo(&socket, &["pause"]);
    assert!(out.contains("paused at 00:00:05.000"), "{out}");
    let out = bo(&socket, &["resume"]);
    assert!(out.contains("playing from 00:00:05.000"), "{out}");

    // stop ends the session and cleans up.
    let out = bo(&socket, &["stop"]);
    assert!(out.contains("ok: stopped"), "{out}");
    wait_for_socket_gone(&socket);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn load_restores_a_session_across_a_daemon_restart() {
    env_once();
    let dir = temp_dir();
    let socket = dir.join("d.sock");
    let snapshot = seed_snapshot(&dir, 1);

    // Restore on a fresh daemon, then end it; restore again on another.
    for _ in 0..2 {
        let out = bo(&socket, &["load", &snapshot.to_string_lossy()]);
        assert!(out.contains("ok: loaded"), "{out}");
        let out = bo(&socket, &["play"]);
        assert!(out.contains("1 track, 1 clip"), "{out}");
        bo(&socket, &["stop"]);
        wait_for_socket_gone(&socket);
    }

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_client_and_the_cli_share_the_daemon_session() {
    env_once();
    let dir = temp_dir();
    let socket = dir.join("d.sock");

    // A typed client spawns the daemon and builds the arrangement.
    let mut client = Bo::with_connection(Connection::at(&socket));
    client
        .put(
            Clip::of("a.wav").trim(TimecodeRange::from((
                Duration::ZERO,
                Duration::from_secs(30),
            ))),
            TrackIndex(0).at(Duration::ZERO),
        )
        .unwrap();

    // The CLI reaches that very session: no load needed.
    let out = bo(&socket, &["play"]);
    assert!(out.contains("1 track, 1 clip"), "{out}");
    bo(&socket, &["stop"]);
    wait_for_socket_gone(&socket);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn refusal_and_usage_cases_are_exit_1_and_2() {
    env_once();
    let dir = temp_dir();
    let socket = dir.join("d.sock");
    let snapshot = seed_snapshot(&dir, 0);

    // A load of nothing, then play on nothing: refused with exit 1.
    bo(&socket, &["load", &snapshot.to_string_lossy()]);
    let (code, out) = bo_exit(&socket, &["play"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("no clips"), "{out}");
    wait_for_socket_gone(&socket);

    // A missing snapshot file is a refusal; a bad timecode and an unknown
    // command are usage errors.
    let (code, out) = bo_exit(&socket, &["load", "/nonexistent/nowhere.bo"]);
    assert_eq!(code, 1, "{out}");
    let (code, out) = bo_exit(&socket, &["seek", "bogus"]);
    assert_eq!(code, 2, "{out}");
    assert!(out.contains("timecode"), "{out}");
    let (code, _) = bo_exit(&socket, &["frobnicate"]);
    assert_eq!(code, 2, "an unknown command is usage");

    std::fs::remove_dir_all(&dir).ok();
}
