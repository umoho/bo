//! End-to-end tests for the client ↔ daemon lifecycle, using the real `bo`
//! binary: the first command auto-spawns a daemon, later invocations reach
//! the same arrangement, and the daemon cleans up its socket when the program
//! finishes.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

fn bo(socket: &str, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_bo"))
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
