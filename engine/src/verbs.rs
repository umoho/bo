//! The session verbs: one command applied to a player-owned arrangement.
//!
//! [`put_on`] is the single implementation every executor runs — the
//! daemon's typed endpoint, a future process session, headless tests. The
//! client sends the same command over its wire; here is where it is done.

use std::sync::Arc;

use bo_core::command::{Error, Overlap, PlacedClip, Put, Slice, TrackPos};
use bo_core::track::{Clip, Fade, Source, Track};

use crate::rodio::probe;
use crate::{Backend, Change, Player};

/// Place a clip into a player-owned arrangement: the `from..to` window
/// `slice` of source `uri`, on `on.track` at track-time `on.at`.
///
/// A refused put — a slice with no end whose source cannot be measured, or
/// a placement that collides with a resident clip — is an `Err` and leaves
/// the arrangement exactly as it was. The track is grown to fit. A clip
/// placed past the end of a running track's queue joins that queue as it is
/// placed; anything else waits for an `apply` — see [`Put::landed`].
pub fn put_on<B: Backend>(
    player: &mut Player<B>,
    uri: &str,
    slice: Slice,
    on: TrackPos,
) -> Result<Put, Error> {
    // An open slice plays to the source's end; resolve that end now, so
    // every clip has a known finite length and none can silently block its
    // track. Same refusal the CLI makes.
    let to = match slice.to {
        Some(to) => to,
        None => probe(uri).map_err(|why| Error::Probe {
            uri: uri.to_string(),
            why,
        })?,
    };
    // The track is addressed by index, created on demand like the CLI's.
    while player.tracks().len() <= on.track {
        player.add_track(Track::new());
    }
    let clip = Clip::sliced(Arc::new(Source::new(uri)), slice.from, to)
        .at(on.at)
        .gain(1.0)
        .fade(Fade::default());
    // Refuse a collision before inserting anything: a rejected put leaves no
    // trace, and says where the clip could go instead.
    let view = &player.tracks()[on.track];
    if let Some(conflict) = view.clips().iter().find(|c| c.overlaps(&clip)) {
        return Err(Error::Overlap(Overlap {
            track: on.track,
            at: clip.at,
            conflict: conflict.id,
            conflict_at: conflict.at,
            conflict_end: conflict.end(),
            next_free: view.next_free_start(clip.at, clip.duration()),
        }));
    }
    let id = player.tracks_mut()[on.track]
        .insert(clip)
        .expect("pre-checked: the insert cannot collide");
    // Placement past the queued tail joins the running graph; the rest waits
    // for an apply — exactly what the daemon does.
    let landed = player.changed(Change::Appended(on.track));
    Ok(Put {
        track: on.track,
        clip: PlacedClip {
            id,
            uri: uri.to_string(),
            at: on.at,
            from: slice.from,
            to,
            gain: 1.0,
            fade: Fade::default(),
        },
        landed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Landed, Player, Silent};
    use std::path::Path;
    use std::time::Duration;

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

    fn player() -> Player<Silent> {
        Player::default()
    }

    #[test]
    fn put_places_a_windowed_clip_on_a_track() {
        let mut p = player();
        let uri = src("a.wav");
        let put = put_on(
            &mut p,
            &uri,
            Slice::window(Duration::ZERO, Duration::from_secs_f64(0.2)),
            TrackPos::from((0, Duration::ZERO)),
        )
        .unwrap();
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
    fn an_open_slice_is_probed_to_the_sources_end() {
        let mut p = player();
        let uri = src("a.wav");
        let put = put_on(
            &mut p,
            &uri,
            Slice::whole(),
            TrackPos::from((0, Duration::ZERO)),
        )
        .unwrap();
        assert_eq!(put.clip.from, Duration::ZERO);
        assert!(
            !put.clip.to.is_zero(),
            "an open slice resolves to a finite end"
        );
        assert_eq!(put.clip.to, Duration::from_secs_f64(0.5));
    }

    #[test]
    fn a_collision_refuses_and_leaves_no_trace() {
        let mut p = player();
        let uri = src("a.wav");
        let zero = Duration::ZERO;
        put_on(
            &mut p,
            &uri,
            Slice::window(Duration::ZERO, Duration::from_secs_f64(0.2)),
            TrackPos::from((0, zero)),
        )
        .unwrap();
        let err = put_on(
            &mut p,
            &uri,
            Slice::window(Duration::ZERO, Duration::from_secs_f64(0.2)),
            TrackPos::from((0, zero)),
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
    fn butt_joined_clips_share_a_track_in_order() {
        let mut p = player();
        let uri = src("a.wav");
        let d = Duration::from_secs_f64(0.2);
        put_on(
            &mut p,
            &uri,
            Slice::window(Duration::ZERO, d),
            TrackPos::from((0, Duration::ZERO)),
        )
        .unwrap();
        let put = put_on(
            &mut p,
            &uri,
            Slice::window(Duration::ZERO, d),
            TrackPos::from((0, d)),
        )
        .unwrap();
        assert_eq!(put.clip.id, 1);
        assert_eq!(p.tracks()[0].clips().len(), 2);
        assert_eq!(p.tracks()[0].clips()[1].at, d);
        assert_eq!(p.duration(), d + d);
    }

    #[test]
    fn tracks_grow_to_fit() {
        let mut p = player();
        let uri = src("a.wav");
        let d = Duration::from_secs_f64(0.2);
        let put = put_on(
            &mut p,
            &uri,
            Slice::window(Duration::ZERO, d),
            TrackPos::from((4, Duration::ZERO)),
        )
        .unwrap();
        assert_eq!(put.track, 4);
        assert_eq!(p.tracks().len(), 5, "missing tracks are created");
    }
}
