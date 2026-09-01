//! [`Timeline`]: a resolved playback schedule — what plays, when, at what
//! gain. The single source of scheduling truth shared by realtime playback
//! and offline render: both sides consume the plan, and neither re-derives
//! the timing.
//!
//! Scheduling rules live here and nowhere else: open-ended clips are resolved
//! through a probe, clips finished before the playhead are dropped, the clip
//! covering the playhead is entered mid-way, later clips keep their full
//! length with a silence gap up to their timecode, and the plan's end is the
//! arrangement's end.

use std::time::Duration;

use crate::track::Track;

/// The resolved schedule: per-track plans and the end of the arrangement.
#[derive(Debug, Clone, PartialEq)]
pub struct Timeline {
    tracks: Vec<TrackPlan>,
    end: Duration,
}

/// One track's contribution to the mix: gain and the clips that actually play.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackPlan {
    gain: f32,
    muted: bool,
    clips: Vec<ClipPlan>,
}

/// One clip as scheduled: where in the source to start, how long it plays,
/// and how much silence comes before it.
#[derive(Debug, Clone, PartialEq)]
pub struct ClipPlan {
    /// Address of the source.
    pub uri: String,
    /// Playback starts this far into the source (`from`, plus any playhead
    /// offset).
    pub into: Duration,
    /// How long it plays.
    pub length: Duration,
    /// Silence before it starts, measured from the previous clip's end (or
    /// from the playhead for the first clip of a track).
    pub delay: Duration,
}

impl Timeline {
    /// Resolve an arrangement into a schedule from playhead `at`.
    ///
    /// `probe` supplies the total length of a source; it is only called for
    /// clips whose length is not already known (open-ended clips).
    pub fn plan<F>(tracks: &[Track], at: Duration, probe: F) -> Result<Self, String>
    where
        F: Fn(&str) -> Result<Duration, String>,
    {
        let mut planned_tracks = Vec::new();
        let mut end = Duration::ZERO;
        for track in tracks {
            if track.clips().is_empty() {
                continue;
            }
            let mut clips = Vec::new();
            let mut previous_end: Option<Duration> = None;
            for clip in track.clips() {
                let from = clip.from;
                let length = match clip.duration() {
                    Some(len) => len,
                    None => probe(&clip.source.uri)?.saturating_sub(from),
                };
                if length == Duration::ZERO {
                    continue;
                }
                let abs_end = clip.at + length;
                if abs_end <= at {
                    continue; // finished before the playhead
                }
                // Enter the clip covering the playhead mid-way.
                let into = at.saturating_sub(clip.at).min(length);
                let remaining = length - into;
                let delay = match previous_end {
                    Some(prev) => clip.at.saturating_sub(prev),
                    None => clip.at.saturating_sub(at),
                };
                clips.push(ClipPlan {
                    uri: clip.source.uri.clone(),
                    into,
                    length: remaining,
                    delay,
                });
                previous_end = Some(abs_end);
                end = end.max(abs_end);
            }
            if clips.is_empty() {
                continue;
            }
            planned_tracks.push(TrackPlan {
                gain: track.volume(),
                muted: track.muted(),
                clips,
            });
        }
        Ok(Self {
            tracks: planned_tracks,
            end,
        })
    }

    /// The per-track plans.
    #[must_use]
    pub fn tracks(&self) -> &[TrackPlan] {
        &self.tracks
    }

    /// The end of the arrangement: the latest end of any planned clip.
    #[must_use]
    pub fn end(&self) -> Duration {
        self.end
    }
}

impl TrackPlan {
    /// Gain of this track in the mix.
    #[must_use]
    pub fn gain(&self) -> f32 {
        self.gain
    }

    /// Whether the track is muted.
    #[must_use]
    pub fn muted(&self) -> bool {
        self.muted
    }

    /// The clips that actually play, in order.
    #[must_use]
    pub fn clips(&self) -> &[ClipPlan] {
        &self.clips
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::track::{Clip, Source};
    use std::sync::Arc;

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    fn src(uri: &str, len: Option<Duration>) -> Arc<Source> {
        Arc::new(Source {
            uri: uri.into(),
            duration: len,
        })
    }

    fn track_with(clips: Vec<Clip>) -> Track {
        let mut t = Track::new();
        for clip in clips {
            t.insert(clip).unwrap();
        }
        t
    }

    #[test]
    fn plan_schedules_clips_with_their_gaps() {
        let t = track_with(vec![
            Clip::new(src("a.wav", Some(secs(10)))),
            Clip::new(src("b.wav", Some(secs(5)))).at(secs(10)),
            Clip::new(src("c.wav", Some(secs(2)))).at(secs(20)),
        ]);
        let plan = Timeline::plan(&[t], Duration::ZERO, |_| unreachable!("all lengths known")).unwrap();
        assert_eq!(plan.end(), secs(22));
        let clips = &plan.tracks()[0].clips();
        assert_eq!(clips.len(), 3);
        assert_eq!((clips[0].uri.as_str(), clips[0].into, clips[0].length, clips[0].delay),
                   ("a.wav", Duration::ZERO, secs(10), Duration::ZERO));
        assert_eq!((clips[1].uri.as_str(), clips[1].into, clips[1].length, clips[1].delay),
                   ("b.wav", Duration::ZERO, secs(5), Duration::ZERO), "butt-joined at a's end");
        assert_eq!(clips[2].delay, secs(5), "gap from b's end (15s) to c's start (20s)");
    }

    #[test]
    fn plan_enters_the_current_clip_midway_and_drops_finished_ones() {
        let t = track_with(vec![
            Clip::new(src("a.wav", Some(secs(10)))),
            Clip::new(src("b.wav", Some(secs(5)))).at(secs(10)),
        ]);
        let plan = Timeline::plan(&[t], secs(12), |_| unreachable!()).unwrap();
        let clips = &plan.tracks()[0].clips();
        assert_eq!(clips.len(), 1, "a ended before the playhead");
        assert_eq!(clips[0].uri, "b.wav");
        assert_eq!(clips[0].into, secs(2), "entered 2s in");
        assert_eq!(clips[0].length, secs(3));
        assert_eq!(clips[0].delay, Duration::ZERO, "already in progress");
        assert_eq!(plan.end(), secs(15));
    }

    #[test]
    fn open_ended_clips_resolve_through_the_probe() {
        let t = track_with(vec![Clip::new(src("live.wav", None))]);
        let plan = Timeline::plan(&[t], Duration::ZERO, |uri| {
            assert_eq!(uri, "live.wav");
            Ok(secs(60))
        })
        .unwrap();
        assert_eq!(plan.tracks()[0].clips()[0].length, secs(60));
        assert_eq!(plan.end(), secs(60));
        // A failed probe fails the plan.
        let t = track_with(vec![Clip::new(src("missing.wav", None))]);
        let err = Timeline::plan(&[t], Duration::ZERO, |_| Err("cannot probe".into())).unwrap_err();
        assert_eq!(err, "cannot probe");
    }

    #[test]
    fn muted_tracks_and_volume_survive_into_the_plan() {
        let mut bed = track_with(vec![Clip::new(src("a.wav", Some(secs(5))))]);
        bed.set_volume(0.4);
        let mut voice = track_with(vec![Clip::new(src("v.wav", Some(secs(5))))]);
        voice.set_muted(true);
        let plan = Timeline::plan(&[bed, voice], Duration::ZERO, |_| unreachable!()).unwrap();
        assert_eq!(plan.tracks().len(), 2);
        assert_eq!(plan.tracks()[0].gain(), 0.4);
        assert!(!plan.tracks()[0].muted());
        assert!(plan.tracks()[1].muted());
        // Empty tracks are dropped from the plan.
        let plan = Timeline::plan(&[Track::new()], Duration::ZERO, |_| unreachable!()).unwrap();
        assert!(plan.tracks().is_empty());
        assert_eq!(plan.end(), Duration::ZERO);
    }
}
