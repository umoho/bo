//! [`Rodio`]: a real audio [`Backend`] over the system output device.
//!
//! The arrangement maps onto rodio's model one to one: each non-empty track
//! becomes a [`Player`] (a mixing node on the device's mixer) whose gain is
//! the track's volume times the master; each clip becomes a decoded source,
//! skipped into, cut short, and delayed so it lands at its timecode. The mix
//! is rebuilt from the tracks on every `play`, which is how the engine's
//! re-plan-on-seek works in sound.
//!
//! Every clip already has a known finite length (the put command probes
//! sources that would otherwise be open-ended), so the mix needs no probing.

use std::fs::File;
use std::io::BufReader;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use rodio::mixer::{self, Mixer};
use rodio::math::nz;
use rodio::source::from_factory;
use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink, Player, Sample, Source};

use crate::engine::measure::{Measurement, Meter};
use crate::engine::timeline::{ClipPlan, Timeline};
use crate::engine::{Backend, BackendError};
use crate::track::{Fade, Track};

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
    // The fade-in window is shortened by however far into the clip the
    // playhead already is; the fade-out is always at the clip's end.
    let fade = Fade {
        fade_in: plan.fade.fade_in.saturating_sub(plan.into),
        fade_in_from: plan.fade.fade_in_from,
        fade_out: plan.fade.fade_out,
        fade_out_to: plan.fade.fade_out_to,
        shape: plan.fade.shape,
    };
    Ok(apply_fade(
        // Skip to the in-point plus however far into the clip the playhead
        // already is; the decoder drains exactly `from + into` of samples,
        // so the entry point is sample-accurate for every decodable format.
        decoder
            .skip_duration(plan.from + plan.into)
            .take_duration(plan.length),
        fade,
        plan.length,
    )
    .amplify(plan.gain)
    .delay(plan.delay))
}

/// A [`Source`] that applies a [`Fade`] envelope to a finite span: fade in
/// from the start, fade out into the end. `fade_in` is measured from the
/// span's start, already shortened by any skipped lead-in.
struct FadeSource<I> {
    input: I,
    fade: Fade,
    length: Duration,
    /// Duration of one interleaved sample of `input`.
    per_sample: Duration,
    /// Position of the next sample to emit.
    pos: Duration,
}

fn apply_fade<I>(input: I, fade: Fade, length: Duration) -> FadeSource<I>
where
    I: Source,
{
    let per_sample = Duration::from_secs_f64(
        1.0 / (input.sample_rate().get() as f64 * input.channels().get() as f64),
    );
    FadeSource {
        input,
        fade,
        length,
        per_sample,
        pos: Duration::ZERO,
    }
}

impl<I: Source> Iterator for FadeSource<I> {
    type Item = Sample;

    fn next(&mut self) -> Option<Sample> {
        let sample = self.input.next()?;
        let gain = self.fade.gain_at(self.pos, self.length);
        self.pos += self.per_sample;
        Some(sample * gain)
    }
}

impl<I: Source> Source for FadeSource<I> {
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

/// One player per non-empty track on `mixer`, every clip from the shared
/// [`Timeline`]. Returns the players with the gains they were built with, and
/// the end of the arrangement.
fn build_mix(
    mixer: &Mixer,
    tracks: &[Track],
    at: Duration,
    master: f32,
) -> Result<(Vec<(Player, f32)>, Duration), String> {
    let timeline = Timeline::plan(tracks, at);
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
///
/// The wav is staged in a temporary file next to the target and renamed over
/// it only after a clean finalize, so a failed render leaves the previous
/// file (or nothing) untouched instead of a half-written wav. Samples are
/// consumed in whole stereo frames: an odd trailing sample — one channel's
/// final ~23µs, possible when a range cuts a stereo source mid-frame — is
/// dropped, so the writer always finalizes a frame-aligned stream and the
/// file and the meter can never disagree.
fn mix(
    tracks: &[Track],
    path: Option<&std::path::Path>,
    from: Duration,
    to: Option<Duration>,
    master: f32,
    measure: bool,
) -> Result<(Duration, Option<Measurement>), String> {
    let mut timeline = Timeline::plan(tracks, from);
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
    // Stage the file next to its target so the final rename stays on one
    // filesystem; `tempfile` also deletes the staging file on any early
    // return, which is what clears a failed render.
    let staging: Option<tempfile::NamedTempFile> = match path {
        Some(p) => {
            let dir = p
                .parent()
                .filter(|d| !d.as_os_str().is_empty())
                .unwrap_or_else(|| std::path::Path::new("."));
            let tmp = tempfile::Builder::new()
                .prefix(".bo-render-")
                .suffix(".tmp")
                .tempfile_in(dir)
                .map_err(|e| format!("cannot create a temporary file in {}: {e}", dir.display()))?;
            // tempfile creates 0600; a rendered wav should follow the usual
            // umask-default visibility, so pin it to 0644 before the rename.
            tmp.as_file()
                .set_permissions(std::fs::Permissions::from_mode(0o644))
                .map_err(|e| format!("cannot set permissions on a temporary file: {e}"))?;
            Some(tmp)
        }
        None => None,
    };
    let mut writer = match staging.as_ref() {
        Some(tmp) => {
            let spec = hound::WavSpec {
                channels: 2,
                sample_rate: 44100,
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Float,
            };
            Some(
                hound::WavWriter::create(tmp.path(), spec)
                    .map_err(|e| format!("cannot write wav: {e}"))?,
            )
        }
        None => None,
    };
    let mut meter = measure.then(|| Meter::new(2, 44100));
    // Consume whole frames: buffer two interleaved samples, feed both the
    // meter and the writer, and drop a trailing half-frame so the stream the
    // file receives is exactly the stream that was measured.
    let mut frame: [f32; 2] = [0.0; 2];
    let mut filled = 0usize;
    for sample in source {
        frame[filled] = sample;
        filled += 1;
        if filled == 2 {
            if let Some(m) = meter.as_mut() {
                m.push(frame[0]);
                m.push(frame[1]);
            }
            if let Some(w) = writer.as_mut() {
                w.write_sample(frame[0]).map_err(|e| format!("cannot write wav: {e}"))?;
                w.write_sample(frame[1]).map_err(|e| format!("cannot write wav: {e}"))?;
            }
            filled = 0;
        }
    }
    if let Some(w) = writer {
        w.finalize().map_err(|e| format!("cannot write wav: {e}"))?;
    }
    if let Some(tmp) = staging {
        let target = path.expect("a staged wav always has a target path");
        tmp.persist(target)
            .map_err(|e| format!("cannot write {}: {}", target.display(), e.error))?;
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

/// How a source's length was learned.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SourceLength {
    /// The container states its own length (a wav/flac header, an mp3 with a
    /// Xing/Info frame, …): exact.
    Exact(Duration),
    /// No container length: the source was decoded to its end and the length
    /// follows from the samples heard. Within one encoder frame of the truth
    /// for lossy codecs; exact for lossless ones.
    Estimated(Duration),
}

impl SourceLength {
    /// The length, however it was learned.
    #[must_use]
    pub fn duration(self) -> Duration {
        match self {
            Self::Exact(d) | Self::Estimated(d) => d,
        }
    }
}

/// Learn a source's length — from its container when the container states
/// one, otherwise by decoding to the end. Pure decoding, no device needed,
/// so it works headless (`bo probe <uri>`, tests, CI). Fails only when the
/// file cannot be opened or decoded at all.
pub fn measure(uri: &str) -> Result<SourceLength, String> {
    let file = File::open(uri).map_err(|e| format!("cannot open {uri}: {e}"))?;
    let decoder = Decoder::new(BufReader::new(file))
        .map_err(|e| format!("cannot decode {uri}: {e}"))?;
    Ok(measure_source(decoder))
}

/// Classify a decoder's length: exact when the container states it, otherwise
/// estimated by decoding to the end.
fn measure_source<D: Source>(decoder: D) -> SourceLength {
    match decoder.total_duration() {
        Some(d) => SourceLength::Exact(d),
        None => SourceLength::Estimated(decode_to_end(decoder)),
    }
}

/// Decode `source` to its end and report how long it played: samples heard
/// over rate × channels. The fallback for containers that state no length —
/// mp3 without a Xing/Info frame, for example.
fn decode_to_end<D: Source>(mut source: D) -> Duration {
    let rate = source.sample_rate().get() as f64;
    let channels = source.channels().get() as f64;
    let mut samples = 0u64;
    for _ in source.by_ref() {
        samples += 1;
    }
    Duration::from_secs_f64(samples as f64 / (rate * channels))
}

/// Measure a source's length as a plain duration, exact or estimated — the
/// form put and planning need. `bo probe` reports which kind it got.
pub fn probe(uri: &str) -> Result<Duration, String> {
    measure(uri).map(SourceLength::duration)
}

/// Measure every distinct source in the arrangement: the uri plus how its
/// length was learned, or why it could not be measured. Duplicate uris are
/// probed once.
pub fn probe_sources(tracks: &[Track]) -> Vec<(String, Result<SourceLength, String>)> {
    let mut seen = std::collections::HashSet::new();
    let mut results = Vec::new();
    for track in tracks {
        for clip in track.clips() {
            let uri = clip.source.uri.as_str();
            if !seen.insert(uri) {
                continue;
            }
            results.push((uri.to_string(), measure(uri)));
        }
    }
    results
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

    /// A wav whose content changes every whole second: second `i` is a sine at
    /// `freqs[i]`. Lets a test tell *which* second of the source a render
    /// actually contains by measuring the dominant frequency of its output.
    fn write_stepped_wav(path: &std::path::Path, freqs: &[f32], rate: u32, channels: u16, bits: u16) {
        let seconds = freqs.len() as u32;
        let frames = (rate * seconds) as usize;
        let bytes = u32::from(bits) / 8;
        let fmt_tag: u16 = if bits == 32 { 3 } else { 1 };
        let mut data = Vec::with_capacity(frames * channels as usize * bytes as usize);
        for i in 0..frames {
            let second = (i / rate as usize).min(freqs.len() - 1);
            let freq = freqs[second];
            let v = 0.5 * (2.0 * std::f32::consts::PI * freq * i as f32 / rate as f32).sin();
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

    /// The dominant frequency of the first `window` seconds of a stereo wav,
    /// channel 0, by zero-crossing count — plenty for whole-second tone
    /// steps an octave apart.
    fn freq_of(path: &std::path::Path, window: f32) -> f32 {
        let decoder = Decoder::new(BufReader::new(File::open(path).unwrap())).unwrap();
        let n = (decoder.sample_rate().get() as f32 * window) as usize;
        let mut crossings = 0u64;
        let mut prev: Option<f32> = None;
        for (i, s) in decoder.enumerate() {
            if i % 2 == 1 {
                continue; // channel 1
            }
            if i / 2 >= n {
                break;
            }
            if let Some(p) = prev
                && (p < 0.0) != (s < 0.0)
            {
                crossings += 1;
            }
            prev = Some(s);
        }
        crossings as f32 / (2.0 * window)
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
        Clip::new(
            Arc::new(Source {
                uri: uri.to_string(),
            }),
            Duration::from_secs(len),
        )
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
            Clip::new(
                Arc::new(Source {
                    uri: b.to_str().unwrap().to_string(),
                }),
                Duration::from_millis(500),
            )
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
            track.insert(Clip::new(
                Arc::new(Source {
                    uri: a.to_str().unwrap().to_string(),
                }),
                probed,
            ))
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
    fn render_starts_at_the_clip_in_point() {
        // A sliced clip must read the source from its in-point, not from the
        // top of the file. The source's 1st/2nd/3rd seconds are 440/880/1760
        // Hz, so the rendered audio identifies which second it really came
        // from. Regression: `from > 0` used to be silently ignored and the
        // clip played the source's start.
        let dir = std::env::temp_dir().join(format!("bo-inpoint-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("steps.wav");
        write_stepped_wav(&src, &[440.0, 880.0, 1760.0], 44_100, 1, 16);

        let mut track = Track::named("a");
        track
            .insert(
                Clip::sliced(
                    Arc::new(Source {
                        uri: src.to_str().unwrap().to_string(),
                    }),
                    Duration::from_secs(2),
                    Duration::from_secs(3),
                )
                .at(Duration::ZERO),
            )
            .unwrap();
        let out = dir.join("out.wav");
        render_to_file(&[track], &out, Duration::ZERO, None, 1.0).unwrap();
        let f = freq_of(&out, 0.5);
        assert!(
            (f - 1760.0).abs() < 40.0,
            "clip from=2s must start at the source's 3rd second (1760 Hz), got {f:.0} Hz"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn render_enters_a_midway_clip_at_from_plus_playhead_offset() {
        // Seeking into a clip must enter it at `from + (playhead - at)`: a
        // clip sliced 1..3 s of a stepped source, entered 1 s in, plays the
        // source's 2 s mark (1760 Hz) — not the in-point's content (880 Hz)
        // and certainly not the file's start (440 Hz).
        let dir = std::env::temp_dir().join(format!("bo-inpoint-mid-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("steps.wav");
        write_stepped_wav(&src, &[440.0, 880.0, 1760.0], 44_100, 1, 16);

        let mut track = Track::named("a");
        track
            .insert(
                Clip::sliced(
                    Arc::new(Source {
                        uri: src.to_str().unwrap().to_string(),
                    }),
                    Duration::from_secs(1),
                    Duration::from_secs(3),
                )
                .at(Duration::ZERO),
            )
            .unwrap();
        let out = dir.join("out.wav");
        // Playhead 1 s into a clip that spans 0..2 s of the track.
        render_to_file(&[track], &out, Duration::from_secs(1), None, 1.0).unwrap();
        let f = freq_of(&out, 0.5);
        assert!(
            (f - 1760.0).abs() < 40.0,
            "entered mid-way must skip from+offset=2s (1760 Hz), got {f:.0} Hz"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn in_point_is_sample_accurate_across_rates_channels_and_depths() {
        // The in-point guarantee holds whatever the source's sample rate,
        // channel count or bit depth: a slice from 2 s of a stepped source
        // must play its 3rd second, and entering a 1..3 s clip 1 s in must
        // play the source's 2 s mark too.
        let dir = std::env::temp_dir().join(format!("bo-inpoint-matrix-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (n, (rate, channels, bits)) in [
            (44_100u32, 1u16, 16u16),
            (48_000, 1, 16),
            (48_000, 2, 16),
            (48_000, 2, 24),
            (48_000, 2, 32), // IEEE float
        ]
        .into_iter()
        .enumerate()
        {
            let src = dir.join(format!("steps-{rate}-{channels}-{bits}.wav"));
            write_stepped_wav(&src, &[440.0, 880.0, 1760.0], rate, channels, bits);
            let uri = src.to_str().unwrap().to_string();

            // Whole-slice case: from 2 s, straight render from the start.
            let mut track = Track::named("a");
            track
                .insert(
                    Clip::sliced(
                        Arc::new(Source { uri: uri.clone() }),
                        Duration::from_secs(2),
                        Duration::from_secs(3),
                    )
                    .at(Duration::ZERO),
                )
                .unwrap();
            let out = dir.join(format!("out-{n}a.wav"));
            render_to_file(&[track], &out, Duration::ZERO, None, 1.0).unwrap();
            let f = freq_of(&out, 0.5);
            assert!(
                (f - 1760.0).abs() < 40.0,
                "{rate} Hz {channels}ch {bits}bit slice: {f:.0} Hz, want 1760"
            );

            // Mid-clip case: a 1..3 s clip entered 1 s in starts at 2 s.
            let mut track = Track::named("b");
            track
                .insert(
                    Clip::sliced(
                        Arc::new(Source { uri }),
                        Duration::from_secs(1),
                        Duration::from_secs(3),
                    )
                    .at(Duration::ZERO),
                )
                .unwrap();
            let out = dir.join(format!("out-{n}b.wav"));
            render_to_file(&[track], &out, Duration::from_secs(1), None, 1.0).unwrap();
            let f = freq_of(&out, 0.5);
            assert!(
                (f - 1760.0).abs() < 40.0,
                "{rate} Hz {channels}ch {bits}bit mid-clip: {f:.0} Hz, want 1760"
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
        // ffmpeg ebur128 reads a 440 Hz stereo tone at −6.02 dBFS as
        // −6.7 LUFS (channel-summed, K-weighting shelves above 1 kHz);
        // calibrated against ffmpeg on amplitude-verified files.
        assert!((m.integrated_lufs.unwrap() - (-6.7)).abs() < 0.3, "lufs {:?}", m.integrated_lufs);
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

    #[test]
    fn measure_reports_container_lengths_as_exact() {
        // A wav header states its own length, so measure is exact and probe
        // reports it as such.
        let dir = std::env::temp_dir().join(format!("bo-measure-exact-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.wav");
        write_wav_full(&a, 1.0, 440.0, 0.5, 44_100, 2, 16);
        let length = measure(a.to_str().unwrap()).unwrap();
        assert!(
            matches!(length, SourceLength::Exact(_)),
            "a wav states its length: {length:?}"
        );
        let d = length.duration();
        assert!((d.as_secs_f64() - 1.0).abs() < 0.05, "{d:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn measure_decodes_sources_that_state_no_length() {
        // A container that states no length (an mp3 without a Xing/Info
        // frame, or a data-less wav) must not fail probe: the file is
        // decoded to its end and the length marked estimated. Only a file
        // that cannot be opened or decoded at all is an error.
        let dir = std::env::temp_dir().join(format!("bo-measure-est-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // A zero-frame wav decodes fine but states no length.
        let zero = dir.join("zero.wav");
        {
            let spec = hound::WavSpec {
                channels: 2,
                sample_rate: 44_100,
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Float,
            };
            let w = hound::WavWriter::create(&zero, spec).unwrap();
            w.finalize().unwrap();
        }
        let length = measure(zero.to_str().unwrap()).unwrap();
        assert!(
            matches!(length, SourceLength::Estimated(_)),
            "no frames in the header, so the length is decoded: {length:?}"
        );
        assert_eq!(length.duration(), Duration::ZERO);

        // An infinite source (SineWave) never states a length either; a
        // bounded take lands close to its bound.
        use rodio::source::SineWave;
        let length = measure_source(SineWave::new(440.0).take_duration(Duration::from_secs(1)));
        assert!(matches!(length, SourceLength::Estimated(_)), "{length:?}");
        let d = length.duration();
        assert!((d.as_secs_f64() - 1.0).abs() < 0.05, "{d:?}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn render_cuts_stereo_sources_on_whole_frames() {
        // A stereo source cut at 0.5 s makes rodio's take_duration emit an
        // odd number of interleaved samples (44103), which hound used to
        // reject at finalize as "not a multiple of the number of channels".
        // The writer now consumes whole frames and drops the trailing
        // half-frame, so any range finalizes cleanly.
        let dir = std::env::temp_dir().join(format!("bo-render-frame-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.wav");
        write_wav_full(&a, 1.0, 440.0, 0.5, 44_100, 2, 16);

        let mut track = Track::named("a");
        track.insert(clip_at(a.to_str().unwrap(), 0, 1)).unwrap();
        let out = dir.join("out.wav");
        render_to_file(
            &[track],
            &out,
            Duration::ZERO,
            Some(Duration::from_millis(500)),
            1.0,
        )
        .unwrap();

        let decoder = Decoder::new(BufReader::new(File::open(&out).unwrap())).unwrap();
        let total = decoder.total_duration().unwrap();
        assert!(
            (total.as_secs_f64() - 0.5).abs() < 0.05,
            "rendered {total:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn render_overwrites_atomically_and_leaves_no_staging_file() {
        let dir = std::env::temp_dir().join(format!("bo-render-atomic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.wav");
        write_wav(&a, 1.0, 440.0, 0.5);

        let mut track = Track::named("a");
        track.insert(clip_at(a.to_str().unwrap(), 0, 1)).unwrap();
        let out = dir.join("out.wav");
        // First render creates the file; the second renames over it. Neither
        // may leave a `.bo-render-*` staging file behind.
        render_to_file(&[track.clone()], &out, Duration::ZERO, None, 1.0).unwrap();
        render_to_file(&[track], &out, Duration::ZERO, None, 1.0).unwrap();

        let staging: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(".bo-render-")
            })
            .collect();
        assert!(staging.is_empty(), "staging files left behind: {staging:?}");
        let decoder = Decoder::new(BufReader::new(File::open(&out).unwrap())).unwrap();
        assert!((decoder.total_duration().unwrap().as_secs_f64() - 1.0).abs() < 0.05);
        std::fs::remove_dir_all(&dir).ok();
    }
}
