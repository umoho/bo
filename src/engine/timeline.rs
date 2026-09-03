//! [`Timeline`]: a resolved playback schedule — what plays, when, at what
//! gain. The single source of scheduling truth shared by realtime playback
//! and offline render: both sides consume the plan, and neither re-derives
//! the timing.
//!
//! Scheduling rules live here and nowhere else: clips finished before the
//! playhead are dropped, the clip covering the playhead is entered mid-way,
//! later clips keep their full length with a silence gap up to their
//! timecode, and the plan's end is the arrangement's end.

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
    /// Every clip already has a known finite length (the put command probes
    /// sources with no out-point), so planning cannot fail.
    pub fn plan(tracks: &[Track], at: Duration) -> Self {
        let mut planned_tracks = Vec::new();
        let mut end = Duration::ZERO;
        for track in tracks {
            if track.clips().is_empty() {
                continue;
            }
            let mut clips = Vec::new();
            let mut previous_end: Option<Duration> = None;
            for clip in track.clips() {
                let length = clip.duration();
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
        Self {
            tracks: planned_tracks,
            end,
        }
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

    /// Cut the plan short: keep only the first `to` of playback, dropping
    /// clips that start after it and trimming a clip that spans it. `to` is
    /// measured from the plan's own start (`at`), not from track time zero.
    /// Used by range renders; playback never truncates.
    pub fn truncate(&mut self, to: Duration) {
        for track in &mut self.tracks {
            let mut cursor = Duration::ZERO;
            track.clips.retain_mut(|clip| {
                let start = cursor + clip.delay;
                cursor = start + clip.length;
                if start >= to {
                    false
                } else {
                    clip.length = clip.length.min(to - start);
                    true
                }
            });
        }
        self.end = self.end.min(to);
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

    fn src(uri: &str) -> Arc<Source> {
        Arc::new(Source { uri: uri.into() })
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
            Clip::new(src("a.wav"), secs(10)),
            Clip::new(src("b.wav"), secs(5)).at(secs(10)),
            Clip::new(src("c.wav"), secs(2)).at(secs(20)),
        ]);
        let plan = Timeline::plan(&[t], Duration::ZERO);
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
            Clip::new(src("a.wav"), secs(10)),
            Clip::new(src("b.wav"), secs(5)).at(secs(10)),
        ]);
        let plan = Timeline::plan(&[t], secs(12));
        let clips = &plan.tracks()[0].clips();
        assert_eq!(clips.len(), 1, "a ended before the playhead");
        assert_eq!(clips[0].uri, "b.wav");
        assert_eq!(clips[0].into, secs(2), "entered 2s in");
        assert_eq!(clips[0].length, secs(3));
        assert_eq!(clips[0].delay, Duration::ZERO, "already in progress");
        assert_eq!(plan.end(), secs(15));
    }

    #[test]
    fn truncate_cuts_a_plan_to_a_range() {
        let t = track_with(vec![
            Clip::new(src("a.wav"), secs(10)),
            Clip::new(src("b.wav"), secs(5)).at(secs(10)),
            Clip::new(src("c.wav"), secs(2)).at(secs(20)),
        ]);
        let mut plan = Timeline::plan(&[t], Duration::ZERO);
        plan.truncate(secs(12));
        assert_eq!(plan.end(), secs(12));
        let clips = &plan.tracks()[0].clips();
        assert_eq!(clips.len(), 2, "c starts at 20s, past the cut");
        assert_eq!(clips[0].length, secs(10), "a untouched");
        assert_eq!(clips[1].length, secs(2), "b cut at 12s");
        // Starting mid-way, `to` is relative to the plan's start: entered
        // 3s in, keep 4 more seconds (absolute 7s).
        let mut plan = Timeline::plan(&[track_with(vec![Clip::new(src("a.wav"), secs(10))])], secs(3));
        plan.truncate(secs(4));
        assert_eq!(plan.tracks()[0].clips()[0].length, secs(4), "entered 3s in, keep 4s");
    }

    #[test]
    fn muted_tracks_and_volume_survive_into_the_plan() {
        let mut bed = track_with(vec![Clip::new(src("a.wav"), secs(5))]);
        bed.set_volume(0.4);
        let mut voice = track_with(vec![Clip::new(src("v.wav"), secs(5))]);
        voice.set_muted(true);
        let plan = Timeline::plan(&[bed, voice], Duration::ZERO);
        assert_eq!(plan.tracks().len(), 2);
        assert_eq!(plan.tracks()[0].gain(), 0.4);
        assert!(!plan.tracks()[0].muted());
        assert!(plan.tracks()[1].muted());
        // Empty tracks are dropped from the plan.
        let plan = Timeline::plan(&[Track::new()], Duration::ZERO);
        assert!(plan.tracks().is_empty());
        assert_eq!(plan.end(), Duration::ZERO);
    }
}
