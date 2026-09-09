//! exec: one command against a player-owned arrangement — headless.

use std::path::Path;
use std::time::Duration;

use bo_core::bus::BusRef;
use bo_core::command::{Applied, ClipHere, Command, Error, Inserted, Landed, OnTrack, Outcome, RouteBus};
use bo_engine::{exec, Player, Silent};

fn write_test_wav(path: &Path, seconds: f32) {
    let rate = 44_100u32;
    let n = (rate as f32 * seconds) as usize;
    let mut data = Vec::with_capacity(n * 2);
    for i in 0..n {
        let v = (0.5 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / rate as f32).sin()
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

fn src(uri: &str) -> String {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(Path::new(uri).file_name().unwrap());
    write_test_wav(&path, 0.5);
    // Keep the dir alive: leak it, so the file outlives the test.
    std::mem::forget(dir);
    path.to_string_lossy().into_owned()
}

/// An insert command with a closed window.
fn insert(uri: &str, from: Duration, to: Duration, at: Duration, track: usize) -> Command {
    Command::Insert {
        uri: uri.to_string(),
        from,
        to: Some(to),
        on: OnTrack::Track { index: track, at },
    }
}

/// An insert command with an open end (probing resolves it).
fn insert_open(uri: &str, track: usize) -> Command {
    Command::Insert {
        uri: uri.to_string(),
        from: Duration::ZERO,
        to: None,
        on: OnTrack::Track {
            index: track,
            at: Duration::ZERO,
        },
    }
}

/// The Inserted payload of an outcome, or a panic — tests only ask for
/// inserts.
fn inserted_outcome(outcome: Outcome) -> Inserted {
    match outcome {
        Outcome::Inserted(inserted) => inserted,
        other => panic!("expected an Inserted outcome, got {other:?}"),
    }
}

fn player() -> Player<Silent> {
    Player::default()
}

#[test]
fn exec_insert_places_a_windowed_clip_on_a_track() {
    let mut p = player();
    let uri = src("a.wav");
    let outcome = exec(
        &mut p,
        insert(
            &uri,
            Duration::ZERO,
            Duration::from_secs_f64(0.2),
            Duration::ZERO,
            0,
        ),
    )
    .unwrap();
    let put = inserted_outcome(outcome);
    assert_eq!(put.track, 0);
    assert_eq!(put.clip.id, 0);
    assert_eq!(p.tracks().len(), 1);
    assert_eq!(p.tracks()[0].clips().len(), 1);
    assert_eq!(
        p.tracks()[0].clips()[0].duration(),
        Duration::from_secs_f64(0.2)
    );
    // Stopped transport: the placement waits for a play, not a rebuild.
    assert_eq!(put.landed, Landed::Pending);
}

#[test]
fn exec_insert_probes_an_open_end_to_the_sources_end() {
    let mut p = player();
    let uri = src("a.wav");
    let outcome = exec(&mut p, insert_open(&uri, 0)).unwrap();
    let put = inserted_outcome(outcome);
    assert_eq!(put.clip.from, Duration::ZERO);
    assert!(
        !put.clip.to.is_zero(),
        "an open end resolves to a finite end"
    );
    assert_eq!(put.clip.to, Duration::from_secs_f64(0.5));
}

#[test]
fn exec_insert_refuses_a_collision_and_leaves_no_trace() {
    let mut p = player();
    let uri = src("a.wav");
    let zero = Duration::ZERO;
    let d = Duration::from_secs_f64(0.2);
    exec(&mut p, insert(&uri, zero, d, zero, 0)).unwrap();
    let err = exec(&mut p, insert(&uri, zero, d, zero, 0)).unwrap_err();
    match err {
        Error::Overlap(overlap) => {
            assert_eq!(overlap.track, 0);
            assert_eq!(overlap.conflict, 0);
            assert!(!overlap.next_free.is_zero(), "the freed tail is reported");
        }
        other => panic!("expected an overlap, got {other:?}"),
    }
    assert_eq!(
        p.tracks()[0].clips().len(),
        1,
        "a refused insert leaves no trace"
    );
    assert_eq!(p.duration(), Duration::from_secs_f64(0.2));
}

#[test]
fn exec_insert_butt_joins_clips_in_order() {
    let mut p = player();
    let uri = src("a.wav");
    let d = Duration::from_secs_f64(0.2);
    exec(&mut p, insert(&uri, Duration::ZERO, d, Duration::ZERO, 0)).unwrap();
    let outcome = exec(&mut p, insert(&uri, Duration::ZERO, d, d, 0)).unwrap();
    let put = inserted_outcome(outcome);
    assert_eq!(put.clip.id, 1);
    assert_eq!(p.tracks()[0].clips().len(), 2);
    assert_eq!(p.tracks()[0].clips()[1].at, d);
    assert_eq!(p.duration(), d + d);
}

#[test]
fn exec_insert_grows_tracks_to_fit() {
    let mut p = player();
    let uri = src("a.wav");
    let d = Duration::from_secs_f64(0.2);
    let outcome = exec(&mut p, insert(&uri, Duration::ZERO, d, Duration::ZERO, 4)).unwrap();
    let put = inserted_outcome(outcome);
    assert_eq!(put.track, 4);
    assert_eq!(p.tracks().len(), 5, "missing tracks are created");
}

#[test]
fn exec_insert_on_a_fresh_track_uses_the_playhead() {
    let mut p = player();
    p.seek(Duration::from_secs(3)).unwrap();
    let uri = src("a.wav");
    let outcome = exec(
        &mut p,
        Command::Insert {
            uri: uri.clone(),
            from: Duration::ZERO,
            to: Some(Duration::from_secs(2)),
            on: OnTrack::New { at: None },
        },
    )
    .unwrap();
    let put = inserted_outcome(outcome);
    assert_eq!(put.track, 0, "the fresh track is appended at the end");
    assert_eq!(put.clip.at, Duration::from_secs(3), "landed at the playhead");
    assert_eq!(p.tracks()[0].clips()[0].at, Duration::from_secs(3));

    // A second fresh put appends another track, also at the playhead.
    let outcome = exec(
        &mut p,
        Command::Insert {
            uri,
            from: Duration::ZERO,
            to: Some(Duration::from_secs(2)),
            on: OnTrack::New { at: Some(Duration::from_secs(7)) },
        },
    )
    .unwrap();
    let put = inserted_outcome(outcome);
    assert_eq!(put.track, 1);
    assert_eq!(put.clip.at, Duration::from_secs(7), "a fresh track honors an explicit at");
}

#[test]
fn exec_drives_the_transport() {
    let mut p = player();
    let uri = src("a.wav");
    exec(
        &mut p,
        insert(&uri, Duration::ZERO, Duration::from_secs(10), Duration::ZERO, 0),
    )
    .unwrap();

    let Outcome::Played(played) = exec(&mut p, Command::Play).unwrap() else {
        panic!("expected Played");
    };
    assert_eq!(played.tracks, 1);
    assert_eq!(played.clips, 1);
    assert_eq!(played.end, Duration::from_secs(10));
    assert!(p.is_playing());

    let Outcome::Seeked { at } =
        exec(&mut p, Command::Seek { at: Duration::from_secs(4) }).unwrap()
    else {
        panic!("expected Seeked");
    };
    assert_eq!(at, Duration::from_secs(4));
    assert_eq!(
        p.playhead(),
        Duration::from_secs(4),
        "a running transport is re-planned"
    );

    // A structural edit while playing waits; apply rebuilds for it.
    assert_eq!(p.changed(bo_engine::Change::Structure), Landed::Pending);
    let Outcome::Applied(Applied::Rebuilt { live, at }) = exec(&mut p, Command::Apply).unwrap()
    else {
        panic!("expected a rebuild");
    };
    assert_eq!(live, 0);
    assert_eq!(at, Duration::from_secs(4));

    let Outcome::Paused { at } = exec(&mut p, Command::Pause).unwrap() else {
        panic!("expected Paused");
    };
    assert_eq!(at, Duration::from_secs(4));

    let Outcome::Resumed { .. } = exec(&mut p, Command::Resume).unwrap() else {
        panic!("expected Resumed");
    };
    let Outcome::Stopped = exec(&mut p, Command::Stop).unwrap() else {
        panic!("expected Stopped");
    };
    assert_eq!(p.state(), bo_engine::State::Stopped);
}

#[test]
fn exec_routes_tracks_into_buses() {
    let mut p = player();
    exec(
        &mut p,
        insert(&src("a.wav"), Duration::ZERO, Duration::from_secs(10), Duration::ZERO, 0),
    )
    .unwrap();

    // A route to a missing track or bus is refused, no trace.
    match exec(
        &mut p,
        Command::Route { track: 9, bus: RouteBus::Group(0) },
    )
    .unwrap_err()
    {
        Error::NoTrack(9) => {}
        other => panic!("expected NoTrack, got {other:?}"),
    }
    match exec(
        &mut p,
        Command::Route { track: 0, bus: RouteBus::Group(7) },
    )
    .unwrap_err()
    {
        Error::NoBus(7) => {}
        other => panic!("expected NoBus, got {other:?}"),
    }
    assert_eq!(p.tracks()[0].bus(), BusRef::Master, "refusals leave no trace");

    // A first-mention New routes and creates; structure lands at apply.
    let Outcome::Routed(routed) = exec(
        &mut p,
        Command::Route {
            track: 0,
            bus: RouteBus::New { name: Some("music".into()) },
        },
    )
    .unwrap()
    else {
        panic!("expected Routed");
    };
    assert_eq!(routed.bus, BusRef::Group(0));
    assert_eq!(routed.landed, Landed::Pending);
    assert_eq!(p.tracks()[0].bus(), BusRef::Group(0));
    assert!(!p.groups()[0].muted());
    assert_eq!(p.groups()[0].name(), Some("music"));

    // A duplicate name is refused, leaving the routing untouched.
    match exec(
        &mut p,
        Command::Route { track: 0, bus: RouteBus::New { name: Some("music".into()) } },
    )
    .unwrap_err()
    {
        Error::Bus(msg) => assert!(msg.contains("already exists"), "{msg}"),
        other => panic!("expected a bus-name error, got {other:?}"),
    }
    assert_eq!(p.groups().len(), 1, "no stray bus");

    // Routing into the fresh group by id joins the same bus.
    let Outcome::Routed(_) = exec(
        &mut p,
        Command::Route { track: 0, bus: RouteBus::Group(0) },
    )
    .unwrap()
    else {
        panic!("expected Routed");
    };

    // Back to the master.
    let Outcome::Routed(routed) = exec(
        &mut p,
        Command::Route { track: 0, bus: RouteBus::Master },
    )
    .unwrap()
    else {
        panic!("expected Routed");
    };
    assert_eq!(routed.bus, BusRef::Master);
    assert_eq!(p.tracks()[0].bus(), BusRef::Master);
}

#[test]
fn exec_take_removes_by_id_or_time() {
    let mut p = player();
    let uri = src("a.wav");
    exec(
        &mut p,
        insert(&uri, Duration::ZERO, Duration::from_secs(10), Duration::ZERO, 0),
    )
    .unwrap();
    exec(
        &mut p,
        insert(&uri, Duration::ZERO, Duration::from_secs(5), Duration::from_secs(10), 0),
    )
    .unwrap();

    // By track time: the clip covering 12 s is the second one (#1).
    let Outcome::Removed(removed) = exec(
        &mut p,
        Command::Remove {
            track: 0,
            clip: ClipHere::At(Duration::from_secs(12)),
        },
    )
    .unwrap()
    else {
        panic!("expected Removed");
    };
    assert_eq!(removed.clip.id, 1);
    assert_eq!(p.tracks()[0].clips().len(), 1);

    // By id: #0 still there.
    let Outcome::Removed(removed) = exec(
        &mut p,
        Command::Remove { track: 0, clip: ClipHere::Id(0) },
    )
    .unwrap()
    else {
        panic!("expected Removed");
    };
    assert_eq!(removed.clip.id, 0);
    assert!(p.tracks()[0].clips().is_empty());

    // Ids are never reused: asking for the gone #1 is refused.
    match exec(&mut p, Command::Remove { track: 0, clip: ClipHere::Id(1) }).unwrap_err() {
        Error::NoClip(what) => assert_eq!(what, "0#1"),
        other => panic!("expected NoClip, got {other:?}"),
    }
    match exec(
        &mut p,
        Command::Remove { track: 0, clip: ClipHere::At(Duration::ZERO) },
    )
    .unwrap_err()
    {
        Error::NoClip(what) => assert!(what.starts_with("0@"), "{what}"),
        other => panic!("expected NoClip, got {other:?}"),
    }
}

#[test]
fn exec_move_rearranges_clips_atomically() {
    let mut p = player();
    let uri = src("a.wav");
    exec(
        &mut p,
        insert(&uri, Duration::ZERO, Duration::from_secs(10), Duration::ZERO, 0),
    )
    .unwrap(); // track 0 #0: 0..10
    exec(
        &mut p,
        insert(&uri, Duration::ZERO, Duration::from_secs(5), Duration::from_secs(10), 0),
    )
    .unwrap(); // track 0 #1: 10..15
    exec(
        &mut p,
        insert(&uri, Duration::ZERO, Duration::from_secs(5), Duration::from_secs(20), 1),
    )
    .unwrap(); // track 1 #0: 20..25 — the id 0 is taken there, but far away

    // #0 from track 0 → track 1 @ 3 s (3..13, clear of 20..25). Track 1
    // already carries id 0, so the moved clip takes track 1's next id.
    let Outcome::Moved(moved) = exec(
        &mut p,
        Command::Move {
            track: 0,
            clip: ClipHere::Id(0),
            to: OnTrack::Track { index: 1, at: Duration::from_secs(3) },
        },
    )
    .unwrap()
    else {
        panic!("expected Moved")
    };
    assert_eq!(moved.from_track, 0);
    assert_eq!(moved.to_track, 1);
    assert_eq!(moved.clip.id, 1, "id 0 was taken on the destination");
    assert_eq!(moved.clip.at, Duration::from_secs(3));
    assert_eq!(p.tracks()[0].clips().len(), 1, "track 0 lost #0");
    assert_eq!(p.tracks()[1].clips().len(), 2);

    // A refused move leaves both tracks untouched: track 0's #1 (10..15)
    // onto track 1 @ 11 collides with the clip just moved there (3..13).
    match exec(
        &mut p,
        Command::Move {
            track: 0,
            clip: ClipHere::Id(1),
            to: OnTrack::Track { index: 1, at: Duration::from_secs(11) },
        },
    )
    .unwrap_err()
    {
        Error::Overlap(overlap) => assert_eq!(overlap.conflict, 1),
        other => panic!("expected Overlap, got {other:?}"),
    }
    assert_eq!(p.tracks()[0].clips().len(), 1, "refused move leaves no trace");
    assert_eq!(p.tracks()[1].clips().len(), 2);

    // A same-track move vacates its own span: #1 at 3..13 → 6 (6..16,
    // clear of #0's 20..25) and keeps its id.
    let Outcome::Moved(moved) = exec(
        &mut p,
        Command::Move {
            track: 1,
            clip: ClipHere::Id(1),
            to: OnTrack::Track { index: 1, at: Duration::from_secs(6) },
        },
    )
    .unwrap()
    else {
        panic!("expected Moved")
    };
    assert_eq!(moved.clip.id, 1, "same-track moves keep the id");
    assert_eq!(moved.clip.at, Duration::from_secs(6));
}

#[test]
fn exec_get_reads_the_arrangement_as_a_tree() {
    let mut p = player();
    exec(
        &mut p,
        insert(&src("a.wav"), Duration::ZERO, Duration::from_secs(10), Duration::ZERO, 0),
    )
    .unwrap();
    exec(&mut p, Command::Route { track: 0, bus: RouteBus::New { name: Some("music".into()) } }).unwrap();

    let Outcome::Tree(tree) = exec(&mut p, Command::Get { path: String::new() }).unwrap() else {
        panic!("expected a tree")
    };
    assert_eq!(tree["track"][0]["clips"][0]["id"], 0);
    assert_eq!(tree["track"][0]["clips"][0]["to"], 10_000u64);
    assert_eq!(tree["bus"][0]["members"], 1);
    assert_eq!(tree["transport"]["state"], "stopped");

    // Subtree and leaf paths, then a dead end.
    let Outcome::Tree(leaf) = exec(
        &mut p,
        Command::Get { path: "track.0.volume".into() },
    )
    .unwrap()
    else {
        panic!("expected a leaf")
    };
    assert_eq!(leaf, serde_json::json!(1.0));
    match exec(&mut p, Command::Get { path: "track.9".into() }).unwrap_err() {
        Error::Path(msg) => assert!(msg.contains("no index 9"), "{msg}"),
        other => panic!("expected Path, got {other:?}"),
    }
}

#[test]
fn exec_set_patches_the_state_zone() {
    let mut p = player();
    exec(
        &mut p,
        insert(&src("a.wav"), Duration::ZERO, Duration::from_secs(10), Duration::ZERO, 0),
    )
    .unwrap();
    exec(
        &mut p,
        Command::Route { track: 0, bus: RouteBus::New { name: Some("music".into()) } },
    )
    .unwrap();

    // Leaf patch: a scalar at a path.
    let Outcome::Set(set) = exec(
        &mut p,
        Command::Set {
            path: "track.0.volume".into(),
            patcher: serde_json::json!(0.4),
        },
    )
    .unwrap()
    else {
        panic!("expected Set")
    };
    assert!((set.patched.as_f64().unwrap() - 0.4).abs() < 1e-6, "{}", set.patched);
    assert!((p.tracks()[0].volume() - 0.4).abs() < 1e-6);

    // Strip patch: an object merges, missing keys untouched.
    let Outcome::Set(_) = exec(
        &mut p,
        Command::Set {
            path: "track.0".into(),
            patcher: serde_json::json!({"muted": true}),
        },
    )
    .unwrap()
    else {
        panic!("expected Set")
    };
    assert!(p.tracks()[0].muted());
    assert!((p.tracks()[0].volume() - 0.4).abs() < 1e-6, "patch leaves volume alone");

    // Master and bus strips.
    exec(
        &mut p,
        Command::Set { path: "master.volume".into(), patcher: serde_json::json!(0.8) },
    )
    .unwrap();
    assert!((p.volume() - 0.8).abs() < 1e-6);
    exec(
        &mut p,
        Command::Set { path: "bus.0.volume".into(), patcher: serde_json::json!(0.5) },
    )
    .unwrap();
    assert!((p.groups()[0].gain() - 0.5).abs() < 1e-6);

    // Refusals: unknown key, structure path, bad type, missing track/bus.
    let Outcome::Tree(tree) = exec(&mut p, Command::Get { path: String::new() }).unwrap() else {
        panic!("expected a tree")
    };
    assert!((tree["track"][0]["volume"].as_f64().unwrap() - 0.4).abs() < 1e-6);
    match exec(
        &mut p,
        Command::Set { path: "track.0.nope".into(), patcher: serde_json::json!(1) },
    )
    .unwrap_err()
    {
        Error::Value(msg) => assert!(msg.contains("unknown track property"), "{msg}"),
        other => panic!("expected Value, got {other:?}"),
    }
    // Clip params are writable too — gain and fades, by the clip's array
    // index in the tree.
    let Outcome::Set(set) = exec(
        &mut p,
        Command::Set {
            path: "track.0.clips.0.gain".into(),
            patcher: serde_json::json!(0.5),
        },
    )
    .unwrap()
    else {
        panic!("expected Set")
    };
    assert!((set.patched.as_f64().unwrap() - 0.5).abs() < 1e-6);
    exec(
        &mut p,
        Command::Set {
            path: "track.0.clips.0".into(),
            patcher: serde_json::json!({"fade_in": 500, "fade_out": 250}),
        },
    )
    .unwrap();
    assert_eq!(p.tracks()[0].clips()[0].fade.fade_in, Duration::from_millis(500));
    assert_eq!(p.tracks()[0].clips()[0].fade.fade_out, Duration::from_millis(250));
    match exec(
        &mut p,
        Command::Set {
            path: "track.0.clips.0.chorus".into(),
            patcher: serde_json::json!(1),
        },
    )
    .unwrap_err()
    {
        Error::Value(msg) => assert!(msg.contains("unknown clip property"), "{msg}"),
        other => panic!("expected Value, got {other:?}"),
    }
    match exec(
        &mut p,
        Command::Set {
            path: "track.0.clips.5.gain".into(),
            patcher: serde_json::json!(0.5),
        },
    )
    .unwrap_err()
    {
        Error::Path(msg) => assert!(msg.contains("no clip index"), "{msg}"),
        other => panic!("expected Path, got {other:?}"),
    }
    match exec(
        &mut p,
        Command::Set { path: "track.0.muted".into(), patcher: serde_json::json!(2) },
    )
    .unwrap_err()
    {
        Error::Value(_) => {}
        other => panic!("expected Value, got {other:?}"),
    }
    match exec(
        &mut p,
        Command::Set { path: "track.9.volume".into(), patcher: serde_json::json!(1) },
    )
    .unwrap_err()
    {
        Error::NoTrack(9) => {}
        other => panic!("expected NoTrack, got {other:?}"),
    }
    match exec(
        &mut p,
        Command::Set { path: "bus.7.volume".into(), patcher: serde_json::json!(1) },
    )
    .unwrap_err()
    {
        Error::NoBus(7) => {}
        other => panic!("expected NoBus, got {other:?}"),
    }
}
