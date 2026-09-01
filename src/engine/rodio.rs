//! [`Rodio`]: a real audio [`Backend`] over the system output device.
//!
//! The arrangement maps onto rodio's model one to one: each non-empty track
//! becomes a [`Player`] (a mixing node on the device's mixer) whose gain is
//! the track's volume times the master; each clip becomes a decoded source,
//! skipped into, cut short, and delayed so it lands at its timecode. The mix
//! is rebuilt from the tracks on every `play`, which is how the engine's
//! re-plan-on-seek works in sound.
//!
//! Unknown source lengths are resolved here by probing the file: an
//! open-ended clip plays to the end of its source, exactly as the model
//! promises.

use std::fs::File;
use std::io::BufReader;
use std::time::Duration;

use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink, Player, Source};

use crate::engine::{Backend, BackendError};
use crate::track::{Clip, Track};

/// A backend that actually makes sound.
pub struct Rodio {
    sink: MixerDeviceSink,
    /// One player per non-empty track, with the gain it was built with.
    players: Vec<(Player, f32)>,
    master: f32,
}

impl std::fmt::Debug for Rodio {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rodio")
            .field("sink", &self.sink)
            .field("players", &self.players.len())
            .field("master", &self.master)
            .finish()
    }
}

impl Rodio {
    /// Open the default output device.
    pub fn try_new() -> Result<Self, String> {
        let mut sink = DeviceSinkBuilder::open_default_sink().map_err(|e| e.to_string())?;
        sink.log_on_drop(false);
        Ok(Self {
            sink,
            players: Vec::new(),
            master: 1.0,
        })
    }

    fn berr(msg: impl Into<String>) -> BackendError {
        BackendError::new("rodio", msg)
    }
}

impl Backend for Rodio {
    fn play(&mut self, tracks: &[Track], at: Duration) -> Result<(), BackendError> {
        for (player, _) in &self.players {
            player.clear();
        }
        self.players.clear();

        let mixer = self.sink.mixer();
        for track in tracks {
            if track.clips().is_empty() {
                continue;
            }
            let gain = if track.muted() { 0.0 } else { track.volume() * self.master };
            let mut current: Option<Player> = None;
            let mut previous_end: Option<Duration> = None;
            for clip in track.clips() {
                let Some((source, abs_end)) = schedule(clip, at, previous_end).map_err(Self::berr)?
                else {
                    continue;
                };
                match &current {
                    Some(player) => player.append(source),
                    None => {
                        let player = Player::connect_new(mixer);
                        player.set_volume(gain);
                        player.append(source);
                        current = Some(player);
                    }
                }
                // The queue plays appended sources back to back, so the next
                // clip's delay counts from this one's actual end.
                previous_end = Some(abs_end);
            }
            if let Some(player) = current {
                self.players.push((player, gain));
            }
        }
        Ok(())
    }

    fn pause(&mut self) {
        for (player, _) in &self.players {
            player.pause();
        }
    }

    fn resume(&mut self) {
        for (player, _) in &self.players {
            player.play();
        }
    }

    fn stop(&mut self) {
        for (player, _) in &self.players {
            player.clear();
        }
        self.players.clear();
    }

    fn set_volume(&mut self, volume: f32) {
        self.master = volume;
        for (player, gain) in &self.players {
            player.set_volume(gain * self.master);
        }
    }
}

/// One clip as a rodio source chain, plus where it ends on the track.
///
/// `playhead` is where playback starts: clips that end before it are skipped,
/// the clip covering it is entered mid-way, and later clips keep their full
/// length with a silence gap up to their timecode. `previous_end` is the end
/// of the last clip actually queued, which anchors the gap math.
fn schedule(
    clip: &Clip,
    playhead: Duration,
    previous_end: Option<Duration>,
) -> Result<Option<(impl Source + Send + 'static, Duration)>, String> {
    let from = clip.from;
    let len = match clip.to {
        Some(to) => to.saturating_sub(from),
        None => {
            let total = probe(&clip.source.uri)?;
            total.saturating_sub(from)
        }
    };
    if len == Duration::ZERO {
        return Ok(None);
    }
    let abs_end = clip.at + len;
    if abs_end <= playhead {
        return Ok(None); // finished before the playhead
    }
    let into = playhead.saturating_sub(clip.at).min(len);
    let remaining = len - into;
    let gap = match previous_end {
        Some(prev) => clip.at.saturating_sub(prev),
        None => clip.at.saturating_sub(playhead),
    };
    let file = File::open(&clip.source.uri)
        .map_err(|e| format!("cannot open {}: {e}", clip.source.uri))?;
    let decoder = Decoder::new(BufReader::new(file))
        .map_err(|e| format!("cannot decode {}: {e}", clip.source.uri))?;
    let source = decoder
        .skip_duration(from + into)
        .take_duration(remaining)
        .delay(gap);
    Ok(Some((source, abs_end)))
}

/// Decode a file far enough to learn its length.
fn probe(uri: &str) -> Result<Duration, String> {
    let file = File::open(uri).map_err(|e| format!("cannot open {uri}: {e}"))?;
    let decoder = Decoder::new(BufReader::new(file))
        .map_err(|e| format!("cannot decode {uri}: {e}"))?;
    decoder
        .total_duration()
        .ok_or_else(|| format!("cannot determine the length of {uri}"))
}
