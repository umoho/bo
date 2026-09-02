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
use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink, Player, Sample, Source};

use crate::engine::measure::{Measurement, Meter};
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
/// sequentially chained source at `track.gain() × master` on a 44.1 kHz
/// stereo mixer, and the mix is pulled until every source is done. Renders
/// from `from` (entering the current clip mid-way); an optional `to` cuts
/// the plan short. `master` scales every track exactly as realtime playback
/// does, so a rendered file sounds like the session. Returns the rendered
/// duration.
///
/// The wav is written as 32-bit float (rodio's native sample type), stereo,
/// 44.1 kHz. Unlike playback this does not use `Player` queues: those stay
/// alive with silence when empty (right for a device, infinite for a render).
pub fn render_to_file(
    tracks: &[Track],
    path: impl AsRef<std::path::Path>,
    from: Duration,
    to: Option<Duration>,
    master: f32,
) -> Result<Duration, String> {
    mix(tracks, Some(path.as_ref()), from, to, master, false).map(|(d, _)| d)
}

/// Render the mix and measure it in the same pass. With `path` `Some` the
/// wav is written too; `None` measures only (no file). The measurement
/// folds the exact sample stream the file writer consumes, so the numbers
/// and the file can never disagree.
pub fn render_and_measure(
    tracks: &[Track],
    path: Option<&std::path::Path>,
    from: Duration,
    to: Option<Duration>,
    master: f32,
) -> Result<(Duration, Measurement), String> {
    mix(tracks, path, from, to, master, true).map(|(d, m)| (d, m.expect("measured")))
}

/// One mix pass over the shared [`Timeline`]: build the 44.1 kHz stereo
/// mixer with one gain chain per non-empty track, then pull every sample
/// through an optional wav writer and an optional [`Meter`].
fn mix(
    tracks: &[Track],
    path: Option<&std::path::Path>,
    from: Duration,
    to: Option<Duration>,
    master: f32,
    measure: bool,
) -> Result<(Duration, Option<Measurement>), String> {
    let mut timeline = Timeline::plan(tracks, from, probe)?;
    if let Some(to) = to {
        // `to` is a track timecode; the plan's own timeline starts at `from`.
        timeline.truncate(to.saturating_sub(from));
    }
    let (input, source) = mixer::mixer(nz!(2), nz!(44100));
    for track in timeline.tracks() {
        let gain = if track.muted() { 0.0 } else { track.gain() * master };
        let mut pending: Vec<Box<dyn Source + Send>> = Vec::new();
        for clip in track.clips() {
            pending.push(Box::new(make_source(clip)?));
        }
        let mut pending = pending.into_iter();
        let track_source = from_factory(move || pending.next());
        input.add(Gain::new(track_source, gain));
    }
    let mut meter = measure.then(|| Meter::new(2, 44100));
    let mut writer = match path {
        Some(p) => {
            let spec = hound::WavSpec {
                channels: 2,
                sample_rate: 44100,
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Float,
            };
            Some(
                hound::WavWriter::create(p, spec)
                    .map_err(|e| format!("cannot create {}: {e}", p.display()))?,
            )
        }
        None => None,
    };
    for sample in source {
        if let Some(m) = meter.as_mut() {
            m.push(sample);
        }
        if let Some(w) = writer.as_mut() {
            w.write_sample(sample).map_err(|e| format!("cannot write wav: {e}"))?;
        }
    }
    if let Some(w) = writer {
        w.finalize().map_err(|e| format!("cannot write wav: {e}"))?;
    }
    Ok((timeline.end(), meter.map(Meter::finish)))
}

/// An offline [`Backend`]: `play` renders the arrangement to a wav file.
/// Transport controls are no-ops — the mix is computed eagerly, not
/// streamed, so there is nothing to pause or resume.
pub struct Renderer {
    path: std::path::PathBuf,
    master: f32,
}

impl Renderer {
    /// Render to `path` (overwritten if it exists), at full master gain.
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self {
            path: path.into(),
            master: 1.0,
        }
    }
}

impl Backend for Renderer {
    fn play(&mut self, tracks: &[Track], at: Duration) -> Result<(), BackendError> {
        render_to_file(tracks, &self.path, at, None, self.master)
            .map_err(|e| BackendError::new("render", e))?;
        Ok(())
    }

    fn pause(&mut self) {}

    fn resume(&mut self) {}

    fn stop(&mut self) {}

    fn set_volume(&mut self, volume: f32) {
        self.master = volume;
    }
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

/// Decode a file far enough to learn its length. Pure decoding — no device
/// needed, so it works headless (`bo probe <uri>`, tests, CI).
pub fn probe(uri: &str) -> Result<Duration, String> {
    let file = File::open(uri).map_err(|e| format!("cannot open {uri}: {e}"))?;
    let decoder = Decoder::new(BufReader::new(file))
        .map_err(|e| format!("cannot decode {uri}: {e}"))?;
    decoder
        .total_duration()
        .ok_or_else(|| format!("cannot determine the length of {uri}"))
}

/// Measure every distinct source in the arrangement: the uri plus its length,
/// or the reason it could not be measured. Duplicate uris are probed once.
pub fn probe_sources(tracks: &[Track]) -> Vec<(String, Result<Duration, String>)> {
    let mut seen = std::collections::HashSet::new();
    let mut results = Vec::new();
    for track in tracks {
        for clip in track.clips() {
            let uri = clip.source.uri.as_str();
            if !seen.insert(uri) {
                continue;
            }
            results.push((uri.to_string(), probe(uri)));
        }
    }
    results
}

/// Probe every distinct source in the arrangement, returning one problem
/// string per source that cannot be opened, decoded, or measured. Duplicate
/// uris are probed once.
pub fn check_sources(tracks: &[Track]) -> Vec<String> {
    probe_sources(tracks)
        .into_iter()
        .filter_map(|(_, result)| result.err())
        .collect()
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
        write_wav_full(path, seconds, freq, amp, 44_100, 1, 16)
    }

    /// A PCM wav at `rate` Hz with `channels` interleaved channels and
    /// `bits` per sample (16 or 24; 32 = IEEE float), a sine at `freq` and
    /// `amp` amplitude on every channel.
    fn write_wav_full(
        path: &std::path::Path,
        seconds: f32,
        freq: f32,
        amp: f32,
        rate: u32,
        channels: u16,
        bits: u16,
    ) {
        let frames = (rate as f32 * seconds) as usize;
        let bytes = u32::from(bits) / 8;
        let fmt_tag: u16 = if bits == 32 { 3 } else { 1 };
        let mut data = Vec::with_capacity(frames * channels as usize * bytes as usize);
        for i in 0..frames {
            let v = amp * (2.0 * std::f32::consts::PI * freq * i as f32 / rate as f32).sin();
            for _ in 0..channels {
                match bits {
                    16 => data.extend_from_slice(&((v * 32767.0) as i16).to_le_bytes()),
                    24 => {
                        let s = (v * 8_388_607.0) as i32;
                        data.extend_from_slice(&s.to_le_bytes()[..3]);
                    }
                    32 => data.extend_from_slice(&v.to_le_bytes()),
                    _ => unreachable!(),
                }
            }
        }
        let block_align = channels * bytes as u16;
        let byte_rate = rate * u32::from(block_align);
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&fmt_tag.to_le_bytes());
        wav.extend_from_slice(&channels.to_le_bytes());
        wav.extend_from_slice(&rate.to_le_bytes());
        wav.extend_from_slice(&byte_rate.to_le_bytes());
        wav.extend_from_slice(&block_align.to_le_bytes());
        wav.extend_from_slice(&bits.to_le_bytes());
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
        let duration = render_to_file(&[bed, voice], &out, Duration::ZERO, None, 1.0).unwrap();
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
        render_to_file(&[loud.clone(), silent.clone()], &out, Duration::ZERO, None, 1.0).unwrap();
        let decoder = Decoder::new(BufReader::new(File::open(&out).unwrap())).unwrap();
        let (peak, samples) = decoder.fold((0.0f32, 0u64), |(peak, n), s| (peak.max(s.abs()), n + 1));
        assert!(samples > 1000, "rendered a real mix, not a stub");
        assert!(peak > 0.1, "the loud track is audible, peak {peak}");

        let out = dir.join("out-muted.wav");
        render_to_file(&[silent], &out, Duration::ZERO, None, 1.0).unwrap();
        let decoder = Decoder::new(BufReader::new(File::open(&out).unwrap())).unwrap();
        let (peak, _) = decoder.fold((0.0f32, 0u64), |(peak, n), s| (peak.max(s.abs()), n + 1));
        assert!(peak < 1e-6, "a muted track contributes nothing, peak {peak}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn render_applies_the_master_gain() {
        // Regression: master used to be playback-only; a render ignored it,
        // so `set master 0.25` and `set master 1.0` produced identical files.
        let dir = std::env::temp_dir().join(format!("bo-render-master-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.wav");
        write_wav(&a, 0.5, 440.0, 0.5);
        let mut track = Track::named("bed");
        track.insert(clip_at(a.to_str().unwrap(), 0, 1)).unwrap();

        let full = dir.join("full.wav");
        let quarter = dir.join("quarter.wav");
        render_to_file(&[track.clone()], &full, Duration::ZERO, None, 1.0).unwrap();
        render_to_file(&[track], &quarter, Duration::ZERO, None, 0.25).unwrap();

        let peak = |path: &std::path::Path| {
            let decoder = Decoder::new(BufReader::new(File::open(path).unwrap())).unwrap();
            decoder.fold((0.0f32, 0u64), |(peak, n), s| (peak.max(s.abs()), n + 1)).0
        };
        let ratio = peak(&quarter) / peak(&full);
        assert!(
            (ratio - 0.25).abs() < 0.02,
            "master scales the mix: quarter/full peak ratio {ratio}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn render_duration_matches_probe_at_any_sample_rate() {
        // Reported: a 48 kHz source probed at 1.0 s rendered ~0.5 s with no
        // warning anywhere (probe, check, render all silent). Probe and
        // render must agree whatever the source rate, channel count or bit
        // depth; the offline mixer resamples everything to 44.1 kHz stereo.
        let dir = std::env::temp_dir().join(format!("bo-render-rate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (rate, channels, bits) in [
            (44_100u32, 1u16, 16u16),
            (48_000, 1, 16),
            (48_000, 2, 16),
            (48_000, 2, 24),
            (48_000, 2, 32), // IEEE float
        ] {
            let a = dir.join(format!("a-{rate}-{channels}-{bits}.wav"));
            write_wav_full(&a, 1.0, 440.0, 0.5, rate, channels, bits);
            let probed = probe(a.to_str().unwrap()).unwrap();
            assert!(
                (probed.as_secs_f64() - 1.0).abs() < 0.05,
                "{rate} Hz {channels}ch {bits}bit probed {probed:?}"
            );
            let mut track = Track::named("a");
            track.insert(Clip::new(Arc::new(Source {
                uri: a.to_str().unwrap().to_string(),
                duration: Some(probed),
            })))
            .unwrap();
            let out = dir.join(format!("out-{rate}-{channels}-{bits}.wav"));
            render_to_file(&[track], &out, Duration::ZERO, None, 1.0).unwrap();
            let decoder = Decoder::new(BufReader::new(File::open(&out).unwrap())).unwrap();
            let total = decoder.total_duration().unwrap();
            assert!(
                (total.as_secs_f64() - 1.0).abs() < 0.05,
                "{rate} Hz {channels}ch {bits}bit rendered {total:?}, expected ~1.0 s"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn measure_folds_the_mix_without_writing() {
        // A 10 s 440 Hz sine at amplitude 0.5: peak -6.02 dBFS, RMS -9.03,
        // integrated loudness near the RMS of a mid-range tone.
        let dir = std::env::temp_dir().join(format!("bo-measure-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.wav");
        write_wav_full(&a, 10.0, 440.0, 0.5, 44_100, 2, 16);
        let mut track = Track::named("a");
        track.insert(clip_at(a.to_str().unwrap(), 0, 10)).unwrap();

        // Measure only: no file appears.
        let (duration, m) =
            render_and_measure(&[track.clone()], None, Duration::ZERO, None, 1.0).unwrap();
        assert!((duration.as_secs_f64() - 10.0).abs() < 0.05);
        assert!((m.peak_db - (-6.02)).abs() < 0.05, "peak {}", m.peak_db);
        assert!((m.rms_db - (-9.03)).abs() < 0.05, "rms {}", m.rms_db);
        // ffmpeg ebur128 reads a 440 Hz tone at −6.02 dBFS as −9.7 LUFS
        // (K-weighting shelves above 1 kHz, so a 440 Hz tone sits ~0.7 LU
        // below its RMS); calibrated against ffmpeg.
        assert!((m.integrated_lufs.unwrap() - (-9.7)).abs() < 0.3, "lufs {:?}", m.integrated_lufs);
        assert!(!dir.join("none.wav").exists());

        // Measure while writing: same numbers, plus a real file.
        let out = dir.join("out.wav");
        let (_, m2) = render_and_measure(
            &[track],
            Some(&out),
            Duration::ZERO,
            None,
            1.0,
        )
        .unwrap();
        assert!((m2.peak_db - m.peak_db).abs() < 1e-6, "file and measure agree");
        assert!(out.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn probe_measures_a_wav_and_reports_missing_files() {
        let dir = std::env::temp_dir().join(format!("bo-probe-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.wav");
        write_wav(&a, 0.5, 440.0, 0.5);
        let d = probe(a.to_str().unwrap()).unwrap();
        assert!((d.as_secs_f64() - 0.5).abs() < 0.05, "probed {d:?}");
        assert!(probe("/nonexistent.wav").is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
