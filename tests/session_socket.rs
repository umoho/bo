//! End-to-end: the session client ([`bo::client::Bo`] over a
//! [`bo::connection::Connection`]) reaches the real daemon over its Unix socket —
//! the same arrangement the CLI drives — and typed replies come back typed.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use bo::client::{Bo, BusIndex, BusRef, Clip, ClipOnTrack, Error, Landed, TimecodeRange, TrackIndex};
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

#[test]
fn bo_put_reaches_the_daemon_and_clients_share_the_session() {
    let dir = temp_dir();
    let socket = dir.join("d.sock");
    let _daemon = spawn_daemon(&socket);

    let mut bo = Bo::with_connection(Connection::at(&socket));
    // A closed window needs no file: nothing is probed.
    let put = bo
        .put(
            Clip::of("bed.wav").trim(TimecodeRange::from((Duration::from_secs(10), Duration::from_secs(20)))),
            TrackIndex(0).at(Duration::from_secs(5)),
        )
        .expect("the daemon accepts the put");
    assert_eq!(put.track, 0);
    assert_eq!(put.clip.id, 0);
    assert_eq!(put.clip.at, Duration::from_secs(5));

    // A second Bo shares the session: ids stay stable and grow.
    let mut other = Bo::with_connection(Connection::at(&socket));
    let put = other
        .put(
            Clip::of("voice.wav").trim(TimecodeRange::from((Duration::ZERO, Duration::from_secs(5)))),
            TrackIndex(1).at(Duration::ZERO),
        )
        .expect("voice joins on a fresh track");
    assert_eq!(put.track, 1);
    assert_eq!(put.clip.id, 0, "ids are per-track");
    let tree = bo.get("").unwrap();
    assert_eq!(tree["track"].as_array().unwrap().len(), 2, "both clients see both tracks");
    assert!(
        tree["track"][0]["clips"][0]["uri"]
            .as_str()
            .unwrap()
            .ends_with("bed.wav"),
        "the uri is absolutized against the caller's cwd"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_refused_put_comes_back_as_a_typed_overlap() {
    let dir = temp_dir();
    let socket = dir.join("d.sock");
    let _daemon = spawn_daemon(&socket);

    let mut bo = Bo::with_connection(Connection::at(&socket));
    bo.put(
        Clip::of("a.wav").trim(TimecodeRange::from((Duration::ZERO, Duration::from_secs(10)))),
        TrackIndex(0).at(Duration::ZERO),
    )
    .unwrap();
    let err = bo
        .put(
            Clip::of("b.wav").trim(TimecodeRange::from((Duration::ZERO, Duration::from_secs(10)))),
            TrackIndex(0).at(Duration::ZERO),
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
            Clip::of("a.wav").trim(TimecodeRange::from((Duration::ZERO, Duration::from_secs(1)))),
            TrackIndex(0).at(Duration::ZERO),
        )
        .expect("the first request spawns the daemon");
    assert_eq!(put.clip.id, 0);
    unsafe { std::env::remove_var("BO_DAEMON") };

    // The spawned daemon is the real one: another client reaches it.
    let mut other = Bo::with_connection(Connection::at(&socket));
    let tree = other.get("").unwrap();
    assert_eq!(tree["track"][0]["clips"].as_array().unwrap().len(), 1);
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
        Clip::of("a.wav").trim(TimecodeRange::from((Duration::ZERO, Duration::from_secs(10)))),
        TrackIndex(0).at(Duration::ZERO),
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
    let state = bo.get("transport.state").unwrap();
    assert_eq!(state, serde_json::json!("playing"));
    let playhead = bo.get("transport.playhead").unwrap();
    assert_eq!(playhead, serde_json::json!(4_000u64));

    // stop ends the session: the daemon cleans its socket up.
    bo.stop().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while socket.exists() {
        assert!(Instant::now() < deadline, "daemon did not clean up after stop");
        std::thread::sleep(Duration::from_millis(20));
    }

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn buses_and_routing_round_trip_through_the_daemon() {
    use bo::client::{NewBus, TrackIndex};
    let dir = temp_dir();
    let socket = dir.join("d.sock");
    let _daemon = spawn_daemon(&socket);

    let mut bo = Bo::with_connection(Connection::at(&socket));

    bo.put(
        Clip::of("a.wav").trim(TimecodeRange::from((Duration::ZERO, Duration::from_secs(10)))),
        TrackIndex(0).at(Duration::ZERO),
    )
    .unwrap();

    let routed = bo
        .route(TrackIndex(0), NewBus::with_name("music"))
        .expect("first mention creates and routes");
    assert_eq!(routed.bus, BusRef::Group(0));
    assert_eq!(routed.landed, Landed::Pending, "structure waits for apply");

    // The tree agrees: a bus named music with the track routed into it.
    let buses = bo.get("bus").unwrap();
    assert_eq!(buses[0]["name"], serde_json::json!("music"));
    assert_eq!(buses[0]["members"], serde_json::json!(1));

    bo.route(TrackIndex(0), BusIndex::master()).unwrap();
    let buses = bo.get("bus").unwrap();
    assert_eq!(buses[0]["members"], serde_json::json!(0), "routed back out");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_second_track_joins_an_existing_group_bus_by_id() {
    use bo::client::{BusIndex, NewBus, TrackIndex};
    let dir = temp_dir();
    let socket = dir.join("d.sock");
    let _daemon = spawn_daemon(&socket);

    let mut bo = Bo::with_connection(Connection::at(&socket));
    bo.put(
        Clip::of("a.wav").trim(TimecodeRange::from((Duration::ZERO, Duration::from_secs(10)))),
        TrackIndex(0).at(Duration::ZERO),
    )
    .unwrap();
    bo.put(
        Clip::of("b.wav").trim(TimecodeRange::from((Duration::ZERO, Duration::from_secs(10)))),
        TrackIndex(1).at(Duration::ZERO),
    )
    .unwrap();

    // First mention creates the bus; routing into it by id must travel the
    // wire as a tagged struct variant (Group(u64) could never serialize).
    let created = bo.route(TrackIndex(0), NewBus::with_name("music")).unwrap();
    assert_eq!(created.bus, BusRef::Group(0));
    let joined = bo.route(TrackIndex(1), BusIndex::group(0)).unwrap();
    assert_eq!(joined.bus, BusRef::Group(0));

    let tree = bo.get("").unwrap();
    assert_eq!(tree["bus"][0]["members"], 2, "both tracks share the bus");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn take_round_trips_through_the_daemon() {
    use bo::client::ClipOnTrack;
    let dir = temp_dir();
    let socket = dir.join("d.sock");
    let _daemon = spawn_daemon(&socket);

    let mut bo = Bo::with_connection(Connection::at(&socket));
    bo.put(
        Clip::of("a.wav").trim(TimecodeRange::from((Duration::ZERO, Duration::from_secs(10)))),
        TrackIndex(0).at(Duration::ZERO),
    )
    .unwrap();
    bo.put(
        Clip::of("b.wav").trim(TimecodeRange::from((Duration::ZERO, Duration::from_secs(5)))),
        TrackIndex(0).at(Duration::from_secs(10)),
    )
    .unwrap();

    // Address the clip covering 12 s (b, id 1) by time, then a by id.
    let removed = bo.take(ClipOnTrack::at(TrackIndex(0), Duration::from_secs(12))).unwrap();
    assert_eq!(removed.clip.id, 1);
    assert!(removed.clip.uri.ends_with("b.wav"), "{}", removed.clip.uri);
    let removed = bo.take(ClipOnTrack::id(TrackIndex(0), 0)).unwrap();
    assert!(removed.clip.uri.ends_with("a.wav"), "{}", removed.clip.uri);

    let tree = bo.get("").unwrap();
    let clips: usize = tree["track"]
        .as_array()
        .unwrap()
        .iter()
        .map(|track| track["clips"].as_array().unwrap().len())
        .sum();
    assert_eq!(clips, 0, "both clips are gone");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn get_reads_the_arrangement_over_the_wire() {
    let dir = temp_dir();
    let socket = dir.join("d.sock");
    let _daemon = spawn_daemon(&socket);

    let mut bo = Bo::with_connection(Connection::at(&socket));
    bo.put(
        Clip::of("a.wav").trim(TimecodeRange::from((Duration::ZERO, Duration::from_secs(10)))),
        TrackIndex(0).at(Duration::ZERO),
    )
    .unwrap();

    let tree = bo.get("").unwrap();
    assert_eq!(tree["track"].as_array().unwrap().len(), 1);
    assert_eq!(tree["track"][0]["clips"][0]["to"], 10_000u64);

    let track = bo.get("track.0").unwrap();
    assert_eq!(track["volume"], serde_json::json!(1.0));
    assert!(bo.get("track.9").is_err(), "a dead path is an error");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn set_patches_over_the_wire() {
    let dir = temp_dir();
    let socket = dir.join("d.sock");
    let _daemon = spawn_daemon(&socket);

    let mut bo = Bo::with_connection(Connection::at(&socket));
    bo.put(
        Clip::of("a.wav").trim(TimecodeRange::from((Duration::ZERO, Duration::from_secs(10)))),
        TrackIndex(0).at(Duration::ZERO),
    )
    .unwrap();

    let set = bo.set("track.0", serde_json::json!({"volume": 0.4, "muted": true})).unwrap();
    assert_eq!(set.path, "track.0");
    assert!((set.patched["volume"].as_f64().unwrap() - 0.4).abs() < 1e-6);
    assert_eq!(set.patched["muted"], true);

    let track = bo.get("track.0").unwrap();
    assert_eq!(track["muted"], true);
    assert_eq!(track["name"], serde_json::Value::Null);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn save_and_load_round_trip_through_the_daemon() {
    let dir = temp_dir();
    let socket = dir.join("d.sock");
    let _daemon = spawn_daemon(&socket);
    let snap = dir.join("show.json");

    let mut bo = Bo::with_connection(Connection::at(&socket));
    bo.put(
        Clip::of("a.wav").trim(TimecodeRange::from((Duration::ZERO, Duration::from_secs(10)))),
        TrackIndex(0).at(Duration::ZERO),
    )
    .unwrap();
    bo.set("track.0.volume", serde_json::json!(0.4)).unwrap();
    let snapshot = bo.save(&snap).unwrap();
    assert_eq!(snapshot.version, 1);
    assert_eq!(snapshot.history.len(), 2);

    // Trash the session, then load the snapshot back.
    bo.take(ClipOnTrack::id(TrackIndex(0), 0)).unwrap();
    bo.load(&snap).unwrap();
    let tree = bo.get("").unwrap();
    assert_eq!(tree["track"][0]["clips"].as_array().unwrap().len(), 1, "replayed");
    assert!((tree["track"][0]["volume"].as_f64().unwrap() - 0.4).abs() < 1e-6);

    // A snapshot of a different version is refused, leaving the session as
    // it was.
    let mut wrong = serde_json::from_str::<serde_json::Value>(
        &std::fs::read_to_string(&snap).unwrap(),
    )
    .unwrap();
    wrong["version"] = serde_json::json!(99);
    let bad = dir.join("bad.json");
    std::fs::write(&bad, wrong.to_string()).unwrap();
    assert!(bo.load(&bad).is_err());
    let tree = bo.get("").unwrap();
    assert_eq!(tree["track"][0]["clips"].as_array().unwrap().len(), 1);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn check_validates_a_snapshot_file() {
    let dir = temp_dir();
    let socket = dir.join("d.sock");
    let _daemon = spawn_daemon(&socket);

    let mut bo = Bo::with_connection(Connection::at(&socket));
    let snap = dir.join("ok.bo");
    bo.put(
        Clip::of("a.wav").trim(TimecodeRange::from((Duration::ZERO, Duration::from_secs(10)))),
        TrackIndex(0).at(Duration::ZERO),
    )
    .unwrap();
    bo.save(&snap).unwrap();
    bo.check(&snap).unwrap();

    // An overlapping second insert makes the snapshot invalid.
    let mut bad = serde_json::from_str::<serde_json::Value>(
        &std::fs::read_to_string(&snap).unwrap(),
    )
    .unwrap();
    let mut history = bad["history"].as_array().unwrap().clone();
    history.push(serde_json::json!({
        "cmd": "insert",
        "uri": "a.wav",
        "from": 0,
        "to": 5000,
        "on": { "Track": { "index": 0, "at": 0 } },
    }));
    bad["history"] = serde_json::Value::Array(history);
    let file = dir.join("bad.bo");
    std::fs::write(&file, bad.to_string()).unwrap();
    match bo.check(&file) {
        Err(Error::Check(msg)) => assert!(msg.contains("#2"), "{msg}"),
        other => panic!("expected a Check error, got {other:?}"),
    }
    // Check never touched the session.
    let tree = bo.get("").unwrap();
    assert_eq!(tree["track"][0]["clips"].as_array().unwrap().len(), 1);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn reset_clears_the_session_over_the_wire() {
    let dir = temp_dir();
    let socket = dir.join("d.sock");
    let _daemon = spawn_daemon(&socket);

    let mut bo = Bo::with_connection(Connection::at(&socket));
    bo.put(
        Clip::of("a.wav").trim(TimecodeRange::from((Duration::ZERO, Duration::from_secs(10)))),
        TrackIndex(0).at(Duration::ZERO),
    )
    .unwrap();
    bo.set("track.0.volume", serde_json::json!(0.4)).unwrap();
    bo.reset().unwrap();

    let tree = bo.get("").unwrap();
    assert!(tree["track"].as_array().unwrap().is_empty(), "{tree}");
    let snap = bo.save(dir.join("after.json")).unwrap();
    assert!(snap.history.is_empty(), "a fresh session has no history");
    std::fs::remove_dir_all(&dir).ok();
}
