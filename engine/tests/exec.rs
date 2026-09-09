//! exec: one command against a player-owned arrangement — headless.

use std::path::Path;
use std::time::Duration;

use bo_core::command::{Command, Error, Landed, Outcome, Slice, TrackPos};
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

fn put(uri: &str, slice: Slice, on: TrackPos) -> Command {
    Command::Put {
        uri: uri.to_string(),
        slice,
        on,
    }
}

fn player() -> Player<Silent> {
    Player::default()
}

#[test]
fn exec_put_places_a_windowed_clip_on_a_track() {
    let mut p = player();
    let uri = src("a.wav");
    let outcome = exec(
        &mut p,
        put(
            &uri,
            Slice::window(Duration::ZERO, Duration::from_secs_f64(0.2)),
            TrackPos::from((0, Duration::ZERO)),
        ),
    )
    .unwrap();
    let Outcome::Put(put) = outcome;
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
fn exec_put_probes_an_open_slice_to_the_sources_end() {
    let mut p = player();
    let uri = src("a.wav");
    let outcome = exec(
        &mut p,
        put(&uri, Slice::whole(), TrackPos::from((0, Duration::ZERO))),
    )
    .unwrap();
    let Outcome::Put(put) = outcome;
    assert_eq!(put.clip.from, Duration::ZERO);
    assert!(
        !put.clip.to.is_zero(),
        "an open slice resolves to a finite end"
    );
    assert_eq!(put.clip.to, Duration::from_secs_f64(0.5));
}

#[test]
fn exec_put_refuses_a_collision_and_leaves_no_trace() {
    let mut p = player();
    let uri = src("a.wav");
    let zero = Duration::ZERO;
    exec(
        &mut p,
        put(
            &uri,
            Slice::window(Duration::ZERO, Duration::from_secs_f64(0.2)),
            TrackPos::from((0, zero)),
        ),
    )
    .unwrap();
    let err = exec(
        &mut p,
        put(
            &uri,
            Slice::window(Duration::ZERO, Duration::from_secs_f64(0.2)),
            TrackPos::from((0, zero)),
        ),
    )
    .unwrap_err();
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
        "a refused put leaves no trace"
    );
    assert_eq!(p.duration(), Duration::from_secs_f64(0.2));
}

#[test]
fn exec_put_butt_joins_clips_in_order() {
    let mut p = player();
    let uri = src("a.wav");
    let d = Duration::from_secs_f64(0.2);
    exec(
        &mut p,
        put(
            &uri,
            Slice::window(Duration::ZERO, d),
            TrackPos::from((0, Duration::ZERO)),
        ),
    )
    .unwrap();
    let outcome = exec(
        &mut p,
        put(
            &uri,
            Slice::window(Duration::ZERO, d),
            TrackPos::from((0, d)),
        ),
    )
    .unwrap();
    let Outcome::Put(put) = outcome;
    assert_eq!(put.clip.id, 1);
    assert_eq!(p.tracks()[0].clips().len(), 2);
    assert_eq!(p.tracks()[0].clips()[1].at, d);
    assert_eq!(p.duration(), d + d);
}

#[test]
fn exec_put_grows_tracks_to_fit() {
    let mut p = player();
    let uri = src("a.wav");
    let d = Duration::from_secs_f64(0.2);
    let outcome = exec(
        &mut p,
        put(
            &uri,
            Slice::window(Duration::ZERO, d),
            TrackPos::from((4, Duration::ZERO)),
        ),
    )
    .unwrap();
    let Outcome::Put(put) = outcome;
    assert_eq!(put.track, 4);
    assert_eq!(p.tracks().len(), 5, "missing tracks are created");
}
