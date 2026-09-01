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

use rodio::mixer::{self, Mixer};
use rodio::math::nz;
use rodio::source::from_factory;
use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink, Player, Sample, Source, wav_to_file};

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
        let (players, _) = build_mix(self.sink.mixer(), tracks, at, self.master).map_err(Self::berr)?;
        self.players = players;
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

/// One player per non-empty track on `mixer`, every clip scheduled from
/// `at`. Returns the players with the gains they were built with, and the end
/// of the arrangement. Shared by playback and offline render.
fn build_mix(
    mixer: &Mixer,
    tracks: &[Track],
    at: Duration,
    master: f32,
) -> Result<(Vec<(Player, f32)>, Duration), String> {
    let mut players = Vec::new();
    let mut end = Duration::ZERO;
    for track in tracks {
        if track.clips().is_empty() {
            continue;
        }
        let gain = if track.muted() { 0.0 } else { track.volume() * master };
        let mut current: Option<Player> = None;
        let mut previous_end: Option<Duration> = None;
        for clip in track.clips() {
            let Some((source, abs_end)) = schedule(clip, at, previous_end)? else {
                continue;
            };
            end = end.max(abs_end);
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
            players.push((player, gain));
        }
    }
    Ok((players, end))
}

/// Mix the arrangement down to a wav file, offline — no device needed.
///
/// The same scheduling as playback, but the mix lands in a file: each
/// non-empty track becomes a finite, sequentially chained source at the
/// track's gain, added to a 44.1 kHz stereo mixer, and the mix is pulled
/// until every source is done. Returns the rendered duration.
///
/// Unlike playback this does not use `Player` queues: those stay alive with
/// silence when empty (right for a device, infinite for a render).
pub fn render_to_file(tracks: &[Track], path: impl AsRef<std::path::Path>) -> Result<Duration, String> {
    let (input, source) = mixer::mixer(nz!(2), nz!(44100));
    let mut end = Duration::ZERO;
    for track in tracks {
        if track.clips().is_empty() {
            continue;
        }
        let gain = if track.muted() { 0.0 } else { track.volume() };
        let mut pending: Vec<Box<dyn Source + Send>> = Vec::new();
        let mut previous_end: Option<Duration> = None;
        for clip in track.clips() {
            let Some((clip_source, abs_end)) = schedule(clip, Duration::ZERO, previous_end)? else {
                continue;
            };
            end = end.max(abs_end);
            pending.push(Box::new(clip_source));
            previous_end = Some(abs_end);
        }
        let mut pending = pending.into_iter();
        let track_source = from_factory(move || pending.next());
        input.add(Gain::new(track_source, gain));
    }
    wav_to_file(source, path).map_err(|e| format!("cannot write wav: {e}"))?;
    Ok(end)
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

/// Scales every sample by a fixed factor — a track's gain in the mix.
struct Gain<I> {
    input: I,
    factor: f32,
}

impl<I> Gain<I> {
    fn new(input: I, factor: f32) -> Self {
        Self { input, factor }
    }
}

impl<I: Source> Iterator for Gain<I> {
    type Item = Sample;

    fn next(&mut self) -> Option<Self::Item> {
        self.input.next().map(|sample| sample * self.factor)
    }
}

impl<I: Source> Source for Gain<I> {
    fn current_span_len(&self) -> Option<usize> {
        self.input.current_span_len()
    }

    fn channels(&self) -> rodio::ChannelCount {
        self.input.channels()
    }

    fn sample_rate(&self) -> rodio::SampleRate {
        self.input.sample_rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        self.input.total_duration()
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::track::{Clip, Source, Track};
    use rodio::Source as _;
    use std::sync::Arc;

    /// A mono 16-bit PCM wav with a sine at `amp` amplitude.
    fn write_wav(path: &std::path::Path, seconds: f32, freq: f32, amp: f32) {
        let rate = 44_100u32;
        let n = (rate as f32 * seconds) as usize;
        let mut data = Vec::with_capacity(n * 2);
        for i in 0..n {
            let v = (amp * (2.0 * std::f32::consts::PI * freq * i as f32 / rate as f32).sin()
                * 32767.0) as i16;
            data.extend_from_slice(&v.to_le_bytes());
        }
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&1u16.to_le_bytes()); // mono
        wav.extend_from_slice(&rate.to_le_bytes());
        wav.extend_from_slice(&(rate * 2).to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
        wav.extend_from_slice(&data);
        std::fs::write(path, wav).unwrap();
    }

    fn clip_at(uri: &str, at: u64, len: u64) -> Clip {
        Clip::new(Arc::new(Source {
            uri: uri.to_string(),
            duration: Some(Duration::from_secs(len)),
        }))
        .at(Duration::from_secs(at))
    }

    #[test]
    fn render_mixes_the_timeline_into_a_wav() {
        let dir = std::env::temp_dir().join(format!("bo-render-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.wav");
        let b = dir.join("b.wav");
        write_wav(&a, 1.0, 440.0, 0.5);
        write_wav(&b, 0.5, 880.0, 0.5);

        let mut bed = Track::named("bed");
        bed.insert(clip_at(a.to_str().unwrap(), 0, 1)).unwrap();
        bed.insert(clip_at(b.to_str().unwrap(), 1, 1)).unwrap(); // sliced to 0.5s by the file
        let mut voice = Track::named("voice");
        voice.insert(clip_at(a.to_str().unwrap(), 0, 1)).unwrap();
        voice.set_volume(0.5);

        let out = dir.join("out.wav");
        let duration = render_to_file(&[bed, voice], &out).unwrap();
        assert_eq!(duration, Duration::from_millis(1500), "end of the last clip");

        let decoder = Decoder::new(BufReader::new(File::open(&out).unwrap())).unwrap();
        assert_eq!(decoder.channels().get(), 2, "stereo mix");
        let total = decoder.total_duration().unwrap();
        assert!(
            (total.as_secs_f64() - 1.5).abs() < 0.05,
            "rendered {total:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn muted_tracks_render_as_silence() {
        let dir = std::env::temp_dir().join(format!("bo-render-mute-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.wav");
        write_wav(&a, 0.5, 440.0, 0.5);

        let mut loud = Track::named("loud");
        loud.insert(clip_at(a.to_str().unwrap(), 0, 1)).unwrap();
        let mut silent = Track::named("silent");
        silent.insert(clip_at(a.to_str().unwrap(), 0, 1)).unwrap();
        silent.set_muted(true);

        let out = dir.join("out.wav");
        render_to_file(&[loud.clone(), silent.clone()], &out).unwrap();
        let decoder = Decoder::new(BufReader::new(File::open(&out).unwrap())).unwrap();
        let (peak, samples) = decoder.fold((0.0f32, 0u64), |(peak, n), s| (peak.max(s.abs()), n + 1));
        assert!(samples > 1000, "rendered a real mix, not a stub");
        assert!(peak > 0.1, "the loud track is audible, peak {peak}");

        let out = dir.join("out-muted.wav");
        render_to_file(&[silent], &out).unwrap();
        let decoder = Decoder::new(BufReader::new(File::open(&out).unwrap())).unwrap();
        let (peak, _) = decoder.fold((0.0f32, 0u64), |(peak, n), s| (peak.max(s.abs()), n + 1));
        assert!(peak < 1e-6, "a muted track contributes nothing, peak {peak}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
