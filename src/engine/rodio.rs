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

use crate::engine::timeline::{ClipPlan, Timeline};
use crate::engine::{Backend, BackendError};
use crate::track::Track;

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
/// One planned clip as a rodio source chain: decoded, skipped into, cut to
/// its scheduled length, and delayed to its timecode. Shared by playback and
/// render, exactly like the [`Timeline`] it consumes.
fn make_source(plan: &ClipPlan) -> Result<impl Source + Send + 'static, String> {
    let file = File::open(&plan.uri).map_err(|e| format!("cannot open {}: {e}", plan.uri))?;
    let decoder = Decoder::new(BufReader::new(file))
        .map_err(|e| format!("cannot decode {}: {e}", plan.uri))?;
    Ok(decoder.skip_duration(plan.into).take_duration(plan.length).delay(plan.delay))
}

/// One player per non-empty track on `mixer`, every clip from the shared
/// [`Timeline`]. Returns the players with the gains they were built with, and
/// the end of the arrangement.
fn build_mix(
    mixer: &Mixer,
    tracks: &[Track],
    at: Duration,
    master: f32,
) -> Result<(Vec<(Player, f32)>, Duration), String> {
    let timeline = Timeline::plan(tracks, at, probe)?;
    let mut players = Vec::new();
    for track in timeline.tracks() {
        let gain = if track.muted() { 0.0 } else { track.gain() * master };
        let mut current: Option<Player> = None;
        for clip in track.clips() {
            let source = make_source(clip)?;
            match &current {
                Some(player) => player.append(source),
                None => {
                    let player = Player::connect_new(mixer);
                    player.set_volume(gain);
                    player.append(source);
                    current = Some(player);
                }
            }
        }
        if let Some(player) = current {
            players.push((player, gain));
        }
    }
    Ok((players, timeline.end()))
}

/// Mix the arrangement down to a wav file, offline — no device needed.
///
/// The same [`Timeline`] as playback, but each track becomes a finite,
/// sequentially chained source at the track's gain on a 44.1 kHz stereo
/// mixer, and the mix is pulled until every source is done. Returns the
/// rendered duration.
///
/// Unlike playback this does not use `Player` queues: those stay alive with
/// silence when empty (right for a device, infinite for a render).
pub fn render_to_file(tracks: &[Track], path: impl AsRef<std::path::Path>) -> Result<Duration, String> {
    let timeline = Timeline::plan(tracks, Duration::ZERO, probe)?;
    let (input, source) = mixer::mixer(nz!(2), nz!(44100));
    for track in timeline.tracks() {
        let gain = if track.muted() { 0.0 } else { track.gain() };
        let mut pending: Vec<Box<dyn Source + Send>> = Vec::new();
        for clip in track.clips() {
            pending.push(Box::new(make_source(clip)?));
        }
        let mut pending = pending.into_iter();
        let track_source = from_factory(move || pending.next());
        input.add(Gain::new(track_source, gain));
    }
    wav_to_file(source, path).map_err(|e| format!("cannot write wav: {e}"))?;
    Ok(timeline.end())
}

/// An offline [`Backend`]: `play` renders the arrangement to a wav file.
/// Transport controls are no-ops — the mix is computed eagerly, not
/// streamed, so there is nothing to pause or resume.
pub struct Renderer {
    path: std::path::PathBuf,
}

impl Renderer {
    /// Render to `path` (overwritten if it exists).
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl Backend for Renderer {
    fn play(&mut self, tracks: &[Track], _at: Duration) -> Result<(), BackendError> {
        render_to_file(tracks, &self.path).map_err(|e| BackendError::new("render", e))?;
        Ok(())
    }

    fn pause(&mut self) {}

    fn resume(&mut self) {}

    fn stop(&mut self) {}

    fn set_volume(&mut self, _volume: f32) {}
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

/// Probe every distinct source in the arrangement, returning one problem
/// string per source that cannot be opened, decoded, or measured. Duplicate
/// uris are probed once.
pub fn check_sources(tracks: &[Track]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut problems = Vec::new();
    for track in tracks {
        for clip in track.clips() {
            let uri = clip.source.uri.as_str();
            if !seen.insert(uri) {
                continue;
            }
            if let Err(e) = probe(uri) {
                problems.push(e);
            }
        }
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Player;
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
        // b is a 0.5s file at 1s; the model's length matches the file.
        bed.insert(
            Clip::new(Arc::new(Source {
                uri: b.to_str().unwrap().to_string(),
                duration: Some(Duration::from_millis(500)),
            }))
            .at(Duration::from_secs(1)),
        )
        .unwrap();
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
    fn renderer_backend_renders_on_play() {
        let dir = std::env::temp_dir().join(format!("bo-renderer-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.wav");
        write_wav(&a, 0.3, 440.0, 0.5);
        let out = dir.join("out.wav");

        let mut player = Player::new(Renderer::new(&out));
        let mut track = Track::named("bed");
        track.insert(clip_at(a.to_str().unwrap(), 0, 1)).unwrap();
        player.add_track(track);
        player.play().unwrap();

        let decoder = Decoder::new(BufReader::new(File::open(&out).unwrap())).unwrap();
        let total = decoder.total_duration().unwrap();
        assert!(
            (total.as_secs_f64() - 0.3).abs() < 0.05,
            "the transport rendered a real file: {total:?}"
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
