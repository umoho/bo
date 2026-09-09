//! End-to-end: the session client ([`bo::client::Bo`] over a
//! [`bo::connection::Connection`]) reaches the real daemon over its Unix socket —
//! the same arrangement the CLI drives — and typed replies come back typed.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use bo::client::{Bo, Clip, Error, TrackRef};
use bo::connection::Connection;

fn temp_dir() -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "bo-session-test-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Start a silent daemon on `socket` (the way `bo` would), wait until it
/// answers, and return a handle that stops it when dropped.
fn spawn_daemon(socket: &Path) -> DaemonGuard {
    let child = Command::new(env!("CARGO_BIN_EXE_bo"))
        .env("BO_BACKEND", "silent")
        .arg("daemon")
        .arg("--socket")
        .arg(socket)
        .spawn()
        .expect("the daemon starts");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if std::os::unix::net::UnixStream::connect(socket).is_ok() {
            break;
        }
        assert!(Instant::now() < deadline, "daemon did not come up");
        std::thread::sleep(Duration::from_millis(20));
    }
    DaemonGuard { socket: socket.to_path_buf(), child }
}

struct DaemonGuard {
    socket: PathBuf,
    child: std::process::Child,
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = Command::new(env!("CARGO_BIN_EXE_bo"))
            .env("BO_BACKEND", "silent")
            .arg("--socket")
            .arg(&self.socket)
            .args(["stop"])
            .output();
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.socket.exists() {
            if Instant::now() > deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
    }
}

fn bo_cli(socket: &Path, args: &[&str]) -> String {
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

#[test]
fn bo_put_reaches_the_daemon_and_shares_its_arrangement_with_the_cli() {
    let dir = temp_dir();
    let socket = dir.join("d.sock");
    let _daemon = spawn_daemon(&socket);

    let mut bo = Bo::with_connection(Connection::at(&socket));
    // A closed window needs no file: nothing is probed.
    let put = bo
        .put(
            Clip::window("bed.wav", Duration::from_secs(10), Duration::from_secs(20)),
            TrackRef(0).at(Duration::from_secs(5)),
        )
        .expect("the daemon accepts the put");
    assert_eq!(put.track, 0);
    assert_eq!(put.clip.id, 0);
    assert_eq!(put.clip.at, Duration::from_secs(5));

    // The CLI sees the very same arrangement over its own socket.
    let out = bo_cli(&socket, &["ls"]);
    assert!(out.contains("clip #0") && out.contains("bed.wav"), "{out}");
    assert!(out.contains("00:00:10.000-00:00:20.000"), "{out}");

    // A second Bo shares the session too: ids stay stable and grow.
    let mut other = Bo::with_connection(Connection::at(&socket));
    let put = other
        .put(
            Clip::window("voice.wav", Duration::ZERO, Duration::from_secs(5)),
            TrackRef(1).at(Duration::ZERO),
        )
        .expect("voice joins on a fresh track");
    assert_eq!(put.track, 1);
    assert_eq!(put.clip.id, 0, "ids are per-track");
    let out = bo_cli(&socket, &["ls"]);
    assert!(out.contains("2 tracks") && out.contains("clip #"), "{out}");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_refused_put_comes_back_as_a_typed_overlap() {
    let dir = temp_dir();
    let socket = dir.join("d.sock");
    let _daemon = spawn_daemon(&socket);

    let mut bo = Bo::with_connection(Connection::at(&socket));
    bo.put(
        Clip::window("a.wav", Duration::ZERO, Duration::from_secs(10)),
        TrackRef(0).at(Duration::ZERO),
    )
    .unwrap();
    let err = bo
        .put(
            Clip::window("b.wav", Duration::ZERO, Duration::from_secs(10)),
            TrackRef(0).at(Duration::ZERO),
        )
        .unwrap_err();
    match err {
        Error::Overlap(overlap) => {
            assert_eq!(overlap.track, 0);
            assert_eq!(overlap.conflict, 0);
            assert_eq!(overlap.next_free, Duration::from_secs(10));
        }
        other => panic!("expected a typed overlap, got {other:?}"),
    }

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn session_default_spawns_the_daemon_on_demand() {
    let dir = temp_dir();
    let socket = dir.join("d.sock");
    let _sp = socket.to_string_lossy().into_owned();

    // Point the session at the real bo binary, like an embedded program would.
    let mut bo = Bo::with_connection(Connection::at(&socket));
    unsafe { std::env::set_var("BO_DAEMON", env!("CARGO_BIN_EXE_bo")) };
    let put = bo
        .put(
            Clip::window("a.wav", Duration::ZERO, Duration::from_secs(1)),
            TrackRef(0).at(Duration::ZERO),
        )
        .expect("the first request spawns the daemon");
    assert_eq!(put.clip.id, 0);
    unsafe { std::env::remove_var("BO_DAEMON") };

    // The spawned daemon is the real one: the CLI can reach it.
    let out = bo_cli(&socket, &["ls"]);
    assert!(out.contains("clip #0"), "{out}");
    let _ = Command::new(env!("CARGO_BIN_EXE_bo"))
        .env("BO_BACKEND", "silent")
        .arg("--socket")
        .arg(&socket)
        .args(["stop"])
        .output();
    let deadline = Instant::now() + Duration::from_secs(5);
    while socket.exists() {
        assert!(Instant::now() < deadline, "daemon did not clean up");
        std::thread::sleep(Duration::from_millis(20));
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn transport_verbs_round_trip_through_the_daemon() {
    let dir = temp_dir();
    let socket = dir.join("d.sock");
    let _daemon = spawn_daemon(&socket);

    let mut bo = Bo::with_connection(Connection::at(&socket));
    bo.put(
        Clip::window("a.wav", Duration::ZERO, Duration::from_secs(10)),
        TrackRef(0).at(Duration::ZERO),
    )
    .unwrap();

    let played = bo.play().expect("play");
    assert_eq!(played.clips, 1);
    assert_eq!(played.end, Duration::from_secs(10));

    bo.seek(Duration::from_secs(4)).unwrap();
    let at = bo.pause().unwrap();
    assert_eq!(at, Duration::from_secs(4));
    bo.resume().unwrap();

    // The daemon's own transport agrees.
    let out = bo_cli(&socket, &["ls"]);
    assert!(out.contains("playing") || out.contains("stopped"), "{out}");

    bo.stop().unwrap();
    let out = bo_cli(&socket, &["ls"]);
    assert!(out.contains("stopped, playhead at 00:00:00.000"), "{out}");

    std::fs::remove_dir_all(&dir).ok();
}
