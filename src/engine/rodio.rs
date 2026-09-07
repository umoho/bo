//! [`Rodio`]: a real audio [`Backend`] over the system output device.
//!
//! The arrangement maps onto rodio's model one to one: each non-empty track
//! becomes a *voice* — a [`Player`] holding a queue of the clips still to
//! come — at the track's own gain; each clip becomes a decoded source,
//! positioned at its in-point, cut short, delayed to its timecode, and
//! placed on the stereo field. Voices sum into the graph's internal stereo
//! bus; the master is one gain on that bus's output, and the device's mixer
//! only ever adapts the one mixed stream to its own channel count.
//!
//! A running graph is *edited* where it can be and rebuilt where it cannot.
//! A clip's gain and fade are read from parameters the graph shares with us,
//! so setting one is a store, not a re-plan; a clip placed past the end of a
//! track's queue is appended to it. What is left — a clip taken or moved —
//! rebuilds the graph, and a rebuild is built *before* the old graph is let
//! go, so the mixer is never left with nothing to pull and the device never
//! runs out of sound mid-change.
//!
//! Positioning a clip is a seek when the container can seek and a decode
//! forward when it cannot. rodio's own `skip_duration` is that decode
//! forward, and it is eager: about a millisecond per skipped second, so a
//! graph rebuilt deep into a long file would otherwise take seconds to
//! arrive.
//!
//! Every clip already has a known finite length (the put command probes
//! sources that would otherwise be open-ended), so the mix needs no probing.

use std::fs::File;
use std::io::BufReader;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rodio::mixer::{self, Mixer};
use rodio::math::nz;
use rodio::source::from_factory;
use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink, Player, Sample, Source};

use crate::engine::measure::{Measurement, Meter};
use crate::engine::timeline::{ClipPlan, Timeline};
use crate::engine::{Backend, BackendError, Change};
use crate::track::{Fade, Track};

/// A backend that actually makes sound.
pub struct Rodio {
    sink: MixerDeviceSink,
    graph: Graph,
}

impl std::fmt::Debug for Rodio {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rodio")
            .field("sink", &self.sink)
            .field("voices", &self.graph.voices.len())
            .field("master", &self.graph.master.get())
            .finish()
    }
}

impl Rodio {
    /// Open the default output device.
    pub fn try_new() -> Result<Self, String> {
        let mut sink = DeviceSinkBuilder::open_default_sink().map_err(|e| e.to_string())?;
        sink.log_on_drop(false);
        let config = sink.config();
        // The graph mixes everything into its own stereo bus and hands the
        // device's mixer that one stream; the device mixer adapts it to the
        // output's own channel count.
        let graph = Graph::new(sink.mixer().clone(), config.sample_rate().get());
        Ok(Self { sink, graph })
    }

    fn berr(msg: impl Into<String>) -> BackendError {
        BackendError::new("rodio", msg)
    }
}

impl Backend for Rodio {
    fn play(&mut self, tracks: &[Track], at: Duration) -> Result<(), BackendError> {
        self.graph.play(tracks, at).map_err(Self::berr)
    }

    fn pause(&mut self) {
        self.graph.pause();
    }

    fn resume(&mut self) {
        self.graph.resume();
    }

    fn stop(&mut self) {
        self.graph.stop();
    }

    fn set_volume(&mut self, volume: f32) {
        self.graph.set_master(volume);
    }

    fn land(&mut self, tracks: &[Track], at: Duration, change: &Change) -> bool {
        self.graph.land(tracks, at, change)
    }

    fn position(&self) -> Option<Duration> {
        Some(self.graph.position())
    }
}

/// The live graph: voices — one per non-empty track — summing into an
/// internal stereo bus, whose output carries the master gain into whatever
/// mixer the device (or a test) pulls.
///
/// The graph owns its own stereo bus instead of adding voices straight to
/// the device's mixer: every track's placed output is already a stereo pair,
/// so the bus is the one place they meet, the master is one gain on its
/// output (a store, not a walk over the voices), and the device side only
/// ever converts one already-mixed stereo stream to its own channel count.
/// Kept apart from the device that pulls it, so the whole of it — building a
/// graph, editing a running one, reading the clock — can be exercised over a
/// plain mixer with no audio device in sight.
struct Graph {
    /// The internal stereo bus every voice feeds.
    bus: Mixer,
    /// The master gain, on the bus's output. A live `set master` is one
    /// store to this cell — the running mix never needs rebuilding.
    master: GainCell,
    voices: Vec<Voice>,
    clock: Arc<Clock>,
    /// The timecode the graph was built from, and the clock's reading at that
    /// moment: where the sound is, is `at` plus the frames pulled since.
    base_at: Duration,
    base_frames: u64,
    rate: u32,
    paused: bool,
}

/// One track's place in the mix.
struct Voice {
    /// The track this voice sounds.
    track: usize,
    player: Player,
    /// The track's own gain as built — its volume, or zero when muted. Kept
    /// so the track's strip can move without a rebuild; the master is not
    /// here, it sits on the bus's output.
    gain: f32,
    /// The placement every queued clip of this voice reads, shared so a pan
    /// set while playing lands on the whole queue as one store.
    pan: GainCell,
    /// The track timecode this voice's queue runs to: a clip placed at or
    /// past it can be appended, one placed before it cannot.
    queued_until: Duration,
    /// The queued clips, with the parameters their sources read.
    clips: Vec<ClipVoice>,
}

/// One queued clip's handle on its own source chain.
struct ClipVoice {
    id: u64,
    params: ClipParams,
}

/// The parameters a clip's source chain reads while it plays, shared with the
/// graph that built it: setting a clip's gain, fade or pan is a store into
/// these, not a rebuild.
#[derive(Debug, Clone)]
struct ClipParams {
    gain: GainCell,
    fade: Arc<Mutex<Fade>>,
    /// The track's placement, shared by every clip of one voice: panning the
    /// track writes one cell, and every queued clip's panner hears it.
    pan: GainCell,
    /// How far into the clip the graph entered it, so a fade-in edited later
    /// is still measured from the clip's own start.
    into: Duration,
}

impl ClipParams {
    /// Parameters holding a plan's values, placed by the track's shared cell.
    fn new(plan: &ClipPlan, pan: &GainCell) -> Self {
        Self {
            gain: GainCell::new(plan.gain),
            fade: Arc::new(Mutex::new(plan.fade)),
            pan: pan.clone(),
            into: plan.into,
        }
    }
}

/// A gain that a running source chain reads while the graph writes it.
///
/// `std` has no atomic float, so the bits of an `f32` travel in an
/// [`AtomicU32`]: one relaxed load per sample on the audio thread, one
/// relaxed store per edit.
#[derive(Debug, Clone)]
struct GainCell(Arc<AtomicU32>);

impl GainCell {
    fn new(gain: f32) -> Self {
        Self(Arc::new(AtomicU32::new(gain.to_bits())))
    }

    fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }

    fn set(&self, gain: f32) {
        self.0.store(gain.to_bits(), Ordering::Relaxed);
    }
}

/// The audio clock: frames the device has really pulled, and whether to stop
/// counting. A paused graph is still pulled — for silence — so pausing has to
/// freeze the count rather than let the playhead run on without sound.
#[derive(Debug, Default)]
struct Clock {
    frames: AtomicU64,
    frozen: AtomicBool,
}

impl Graph {
    /// An empty graph whose bus feeds `device` — the device's mixer, or a
    /// plain one a test pulls.
    ///
    /// The bus runs at `rate`, the same rate the device pulls, so the
    /// output needs no resampling on the way out. The clock tap goes on the
    /// bus once and stays. Being infinite, it also means the bus never runs
    /// dry: there is always something for the device to pull, even between
    /// one graph and the next.
    fn new(device: Mixer, rate: u32) -> Self {
        let clock = Arc::new(Clock::default());
        let rate_nz = std::num::NonZeroU32::new(rate).expect("a device rate is nonzero");
        let (bus, bus_out) = mixer::mixer(nz!(2), rate_nz);
        bus.add(ClockTap {
            clock: clock.clone(),
            channels: nz!(2),
            rate: rate_nz,
            index: 0,
        });
        let master = GainCell::new(1.0);
        // The master sits on the bus's output: whatever the device mixer
        // (or test) receives is the mixed stereo pair, scaled once. The
        // device mixer converts that one stream to its own channel count.
        device.add(LiveGain::new(bus_out, master.clone()));
        Self {
            bus,
            master,
            voices: Vec::new(),
            clock,
            base_at: Duration::ZERO,
            base_frames: 0,
            rate,
            paused: false,
        }
    }

    /// Where the sound really is, as a track timecode.
    fn position(&self) -> Duration {
        let frames = self
            .clock
            .frames
            .load(Ordering::Relaxed)
            .saturating_sub(self.base_frames);
        self.base_at + Duration::from_secs_f64(frames as f64 / self.rate.max(1) as f64)
    }

    /// Sound `tracks` from timecode `at`, replacing whatever is sounding.
    ///
    /// The new graph is built and attached *before* the old one is let go, so
    /// the mixer is never empty and the device never runs out of sound to
    /// pull. Letting go is a drop, not rodio's `clear()`: `clear()` blocks
    /// until the audio thread has drained the queue, which is a device buffer
    /// of silence waiting to happen.
    fn play(&mut self, tracks: &[Track], at: Duration) -> Result<(), String> {
        let built = self.build(tracks, at)?;
        let old = std::mem::replace(&mut self.voices, built);
        self.base_at = at;
        self.base_frames = self.clock.frames.load(Ordering::Relaxed);
        self.clock.frozen.store(self.paused, Ordering::Relaxed);
        for voice in &old {
            voice.player.set_volume(0.0);
        }
        // A dropped player goes quiet within one of its queue's control
        // ticks, a few milliseconds; the new voices are already sounding by
        // then, so the hand-over overlaps rather than gapping.
        drop(old);
        Ok(())
    }

    /// One voice per non-empty track, every clip from the shared [`Timeline`].
    ///
    /// Two phases on purpose: sources are opened, positioned and chained
    /// before any of them is attached, because a half-attached mix is audible
    /// as one — the tracks that arrived early would play alone until the slow
    /// ones caught up.
    fn build(&self, tracks: &[Track], at: Duration) -> Result<Vec<Voice>, String> {
        let timeline = Timeline::plan(tracks, at);
        let mut staged = Vec::new();
        for track in timeline.tracks() {
            let gain = if track.muted() { 0.0 } else { track.gain() };
            let pan = GainCell::new(track.pan());
            let mut clips = Vec::new();
            let mut queued_until = at;
            for clip in track.clips() {
                let params = ClipParams::new(clip, &pan);
                let source = make_source(clip, &params)?;
                queued_until += clip.delay + clip.length;
                clips.push((clip.id, params, source));
            }
            staged.push((track.index(), gain, pan, queued_until, clips));
        }
        let mut voices = Vec::new();
        for (track, gain, pan, queued_until, clips) in staged {
            // Queue the clips *before* the voice joins the mixer: rodio
            // bootstraps a new input by pulling it, and an empty queue
            // answers that pull with a few hundred samples of anti-spinlock
            // silence. A voice that arrives already loaded starts on its
            // first frame instead.
            let (player, queue) = Player::new();
            player.set_volume(gain);
            if self.paused {
                player.pause();
            }
            let queued = Self::attach(&player, clips);
            self.bus.add(queue);
            voices.push(Voice {
                track,
                player,
                gain,
                pan,
                queued_until,
                clips: queued,
            });
        }
        Ok(voices)
    }

    /// Queue staged clips onto `player`, keeping the handles to their
    /// parameters.
    fn attach(
        player: &Player,
        clips: Vec<(u64, ClipParams, Box<dyn Source + Send>)>,
    ) -> Vec<ClipVoice> {
        clips
            .into_iter()
            .map(|(id, params, source)| {
                player.append(source);
                ClipVoice { id, params }
            })
            .collect()
    }

    fn pause(&mut self) {
        self.paused = true;
        self.clock.frozen.store(true, Ordering::Relaxed);
        for voice in &self.voices {
            voice.player.pause();
        }
    }

    fn resume(&mut self) {
        self.paused = false;
        self.clock.frozen.store(false, Ordering::Relaxed);
        for voice in &self.voices {
            voice.player.play();
        }
    }

    fn stop(&mut self) {
        let old = std::mem::take(&mut self.voices);
        for voice in &old {
            voice.player.set_volume(0.0);
        }
        drop(old);
        self.paused = false;
        self.base_at = Duration::ZERO;
        self.base_frames = self.clock.frames.load(Ordering::Relaxed);
        // Nothing is sounding, so the clock has nothing to count.
        self.clock.frozen.store(true, Ordering::Relaxed);
    }

    fn set_master(&mut self, master: f32) {
        // One store to the gain on the bus's output; no voice needs
        // touching, so a live `set master` costs the same whether one track
        // or twenty are sounding.
        self.master.set(master);
    }

    /// Take an edit on the running graph; `false` means the graph cannot
    /// express it and only a rebuild will.
    fn land(&mut self, tracks: &[Track], at: Duration, change: &Change) -> bool {
        match change {
            Change::TrackGain(track) => self.land_track_gain(tracks, *track),
            Change::TrackPan(track) => self.land_track_pan(tracks, *track),
            Change::ClipParams(track, id) => self.land_clip_params(tracks, *track, *id),
            Change::Appended(track) => self.land_appended(tracks, at, *track),
            Change::Structure => false,
        }
    }

    /// A track's volume or mute: one gain, written to the player that is
    /// already sounding it.
    fn land_track_gain(&mut self, tracks: &[Track], track: usize) -> bool {
        let Some(voice) = self.voices.iter_mut().find(|v| v.track == track) else {
            // No voice: the track is empty or its clips have all finished, so
            // there is nothing sounding for a gain to act on.
            return true;
        };
        let gain = match tracks.get(track) {
            Some(t) if !t.muted() => t.volume(),
            _ => 0.0,
        };
        voice.gain = gain;
        voice.player.set_volume(gain);
        true
    }

    /// A track's placement: one store to the pan cell every queued clip of
    /// its voice reads.
    fn land_track_pan(&mut self, tracks: &[Track], track: usize) -> bool {
        let Some(voice) = self.voices.iter_mut().find(|v| v.track == track) else {
            // No voice: the track is empty or its clips have all finished, so
            // there is nothing sounding for a pan to act on.
            return true;
        };
        let pan = match tracks.get(track) {
            Some(t) => t.pan(),
            None => 0.0,
        };
        voice.pan.set(pan);
        true
    }

    /// A clip's gain or fade: written to the parameters its source reads.
    fn land_clip_params(&mut self, tracks: &[Track], track: usize, id: u64) -> bool {
        let Some(queued) = self
            .voices
            .iter_mut()
            .find(|v| v.track == track)
            .and_then(|v| v.clips.iter_mut().find(|c| c.id == id))
        else {
            // Not queued: the clip already sounded, or is gone from the
            // arrangement. Either way nothing is waiting to be retuned.
            return true;
        };
        let Some(clip) = tracks
            .get(track)
            .and_then(|t| t.clips().iter().find(|c| c.id == id))
        else {
            return true;
        };
        queued.params.gain.set(clip.gain);
        if let Ok(mut fade) = queued.params.fade.lock() {
            *fade = clip.fade;
        }
        true
    }

    /// Clips placed past the end of a track's queue.
    ///
    /// A rodio queue can be extended but not edited, so this is the one shape
    /// of arrangement change a running graph can take. A clip placed *before*
    /// material already queued — into a gap, say — is what still needs a
    /// rebuild.
    fn land_appended(&mut self, tracks: &[Track], at: Duration, track: usize) -> bool {
        let Some(data) = tracks.get(track) else {
            return true;
        };
        let existing = self.voices.iter().position(|v| v.track == track);
        let from = match existing {
            Some(i) if self.voices[i].queued_until >= at => self.voices[i].queued_until,
            Some(_) => {
                // The queue ends behind the playhead: planning from here
                // would re-enter a clip that is already queued.
                return false;
            }
            None => at,
        };
        // A queue can be extended, not re-ordered: anything on the track that
        // is not queued yet and belongs before the tail needs a rebuild.
        if let Some(i) = existing {
            let voice = &self.voices[i];
            let unqueued = |c: &crate::track::Clip| {
                !voice.clips.iter().any(|q| q.id == c.id) && c.at < from
            };
            if data.clips().iter().any(unqueued) {
                return false;
            }
        }
        // Planned from the queue's own end, so what comes back is exactly the
        // clips that are not queued yet, each with the silence it needs in
        // front of it — the scheduling stays in one place.
        let plan = Timeline::plan(std::slice::from_ref(data), from);
        let Some(planned) = plan.tracks().first() else {
            return true; // nothing out there to queue
        };
        let gain = if planned.muted() { 0.0 } else { planned.gain() };
        // Pan new clips with the voice's own cell when there is one, so a pan
        // set later still lands on what is appended now; a fresh voice takes
        // the planned value.
        let pan = match existing {
            Some(i) => self.voices[i].pan.clone(),
            None => GainCell::new(planned.pan()),
        };
        let mut staged = Vec::new();
        let mut queued_until = from;
        for clip in planned.clips() {
            let params = ClipParams::new(clip, &pan);
            // A source that cannot be built is not this command's problem to
            // report: refuse the live landing, and the rebuild it forces will
            // say why.
            let Ok(source) = make_source(clip, &params) else {
                return false;
            };
            queued_until += clip.delay + clip.length;
            staged.push((clip.id, params, source));
        }
        match existing {
            Some(i) => {
                let voice = &mut self.voices[i];
                let queued = Self::attach(&voice.player, staged);
                voice.clips.extend(queued);
                voice.queued_until = queued_until;
            }
            None => {
                let (player, queue) = Player::new();
                player.set_volume(gain);
                if self.paused {
                    player.pause();
                }
                let queued = Self::attach(&player, staged);
                self.bus.add(queue);
                self.voices.push(Voice {
                    track,
                    player,
                    gain,
                    pan,
                    queued_until,
                    clips: queued,
                });
            }
        }
        true
    }
}

/// A mixer input that sounds nothing and counts the frames it is pulled for:
/// the audio clock. Being infinite, it also keeps the mixer from ever running
/// dry between one graph and the next.
struct ClockTap {
    clock: Arc<Clock>,
    channels: std::num::NonZero<u16>,
    rate: std::num::NonZero<u32>,
    /// Which channel of the current frame comes next.
    index: u16,
}

impl Iterator for ClockTap {
    type Item = Sample;

    fn next(&mut self) -> Option<Sample> {
        if self.index == 0 && !self.clock.frozen.load(Ordering::Relaxed) {
            self.clock.frames.fetch_add(1, Ordering::Relaxed);
        }
        self.index = (self.index + 1) % self.channels.get();
        Some(0.0)
    }
}

impl Source for ClockTap {
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> rodio::ChannelCount {
        self.channels
    }

    fn sample_rate(&self) -> rodio::SampleRate {
        self.rate
    }

    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

/// Places one clip's signal on the stereo bus. How it places depends on the
/// source's own channel layout, which is fixed for the whole clip:
///
/// * mono — a point source. It moves with a constant-power law: the two
///   gains are the cosine and sine of the angle the position maps to, so
///   their squares always sum to one and the source neither gains nor loses
///   loudness as it travels between the speakers (or, at center, against a
///   plain mono file).
/// * stereo — a sound with width. It is *balanced*: the far side is
///   attenuated down to silence while the near side is untouched, so its
///   width is kept as the center of gravity moves.
/// * wider — downmixed to the front pair first (5.1 by the BS.775 center
///   and surround coefficients, so a voice on the center channel survives;
///   other layouts by keeping the first two channels), then balanced.
///
/// The position is read from a shared cell, so a pan set while playing
/// lands on the running chain as one store.
struct Panner<I> {
    input: I,
    pan: GainCell,
    /// Channels per input frame; fixed by the clip's own layout.
    ch: usize,
    /// Samples of the input frame being assembled.
    buf: Vec<Sample>,
    /// Output samples ready to hand out — at most one stereo frame.
    out: [Option<Sample>; 2],
    out_at: usize,
    done: bool,
}

impl<I: Source> Panner<I> {
    fn new(input: I, pan: GainCell) -> Self {
        let ch = input.channels().get() as usize;
        Self {
            input,
            pan,
            ch: ch.max(1),
            buf: Vec::with_capacity(ch),
            out: [None, None],
            out_at: 2,
            done: false,
        }
    }
}

impl<I: Source> Iterator for Panner<I> {
    type Item = Sample;

    fn next(&mut self) -> Option<Sample> {
        loop {
            if self.out_at < 2 {
                let s = self.out[self.out_at];
                self.out_at += 1;
                if s.is_some() {
                    return s;
                }
                continue;
            }
            if self.done {
                return None;
            }
            while self.buf.len() < self.ch {
                match self.input.next() {
                    Some(s) => self.buf.push(s),
                    None => {
                        // A trailing half frame is dropped.
                        self.done = true;
                        return None;
                    }
                }
            }
            let p = self.pan.get().clamp(-1.0, 1.0);
            let (gl, gr) = if p <= 0.0 {
                (1.0, 1.0 + p)
            } else {
                (1.0 - p, 1.0)
            };
            let (l, r) = match self.ch {
                1 => {
                    // A point source, constant power across the speakers:
                    // -1→(1,0), 0→(√½,√½), +1→(0,1).
                    let a = (p + 1.0) * std::f32::consts::FRAC_PI_4;
                    (self.buf[0] * a.cos(), self.buf[0] * a.sin())
                }
                6 => {
                    // 5.1 in L R C LFE Ls Rs order: BS.775 downmix to the
                    // front pair (center and surrounds at −3.01 dB, LFE
                    // dropped), then balanced.
                    let c = std::f32::consts::FRAC_1_SQRT_2;
                    let l = self.buf[0] + c * self.buf[2] + c * self.buf[4];
                    let r = self.buf[1] + c * self.buf[2] + c * self.buf[5];
                    (l * gl, r * gr)
                }
                _ => {
                    // Stereo, or a layout we cannot place: the front pair,
                    // balanced (a stereo pair untouched at center).
                    (self.buf[0] * gl, self.buf[1] * gr)
                }
            };
            self.buf.clear();
            self.out = [Some(l), Some(r)];
            self.out_at = 0;
        }
    }
}

impl<I: Source> Source for Panner<I> {
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> rodio::ChannelCount {
        nz!(2)
    }

    fn sample_rate(&self) -> rodio::SampleRate {
        self.input.sample_rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        self.input.total_duration()
    }
}

/// Build one clip's source chain, from its in-point in the source to a
/// placed stereo pair at its timecode, enveloped and scaled by the params
/// the graph shares.
///
/// Shared by playback and render, exactly like the [`Timeline`] it consumes.
///
/// Gain and envelope are read from `params` rather than baked in, so a chain
/// that is already playing can be retuned; a render builds the same params
/// and simply never changes them.
fn make_source(plan: &ClipPlan, params: &ClipParams) -> Result<Box<dyn Source + Send>, String> {
    let decoder = positioned(&plan.uri, plan.from + plan.into)?;
    let faded = apply_fade(decoder.take_duration(plan.length), params, plan.length);
    // The clip's own silence (its delay) stays in the source's own layout;
    // the panner after it turns whatever that layout is into the stereo pair
    // the bus carries. Silence pans to silence, so the gap's timing is
    // untouched.
    let placed = Panner::new(faded.delay(plan.delay), params.pan.clone());
    Ok(Box::new(LiveGain::new(
        placed,
        params.gain.clone(),
    )))
}

/// Open `uri` with its first sample at `target`.
///
/// By seeking when the container can: symphonia lands on the nearest keyframe
/// and then decodes forward to the exact sample, and rodio's own wav reader
/// lands on the exact frame, so the entry point stays sample-accurate. By
/// decoding forward when it cannot — which is also what a `target` of zero
/// costs nothing to do.
fn positioned(uri: &str, target: Duration) -> Result<Box<dyn Source + Send>, String> {
    if !target.is_zero()
        && let Ok(mut decoder) = seekable(uri)
        && decoder.try_seek(target).is_ok()
    {
        return Ok(Box::new(decoder));
    }
    decoded_forward(uri, target)
}

/// The exact but slow way in: decode forward from the top of the file,
/// dropping exactly `target` of samples. What a container that cannot seek
/// falls back to.
fn decoded_forward(uri: &str, target: Duration) -> Result<Box<dyn Source + Send>, String> {
    let decoder = plain(uri)?;
    Ok(Box::new(decoder.skip_duration(target)))
}

/// A decoder over `uri` that can seek. rodio only builds a seekable one when
/// it is told how long the stream is, which for a local file is its size.
fn seekable(uri: &str) -> Result<Decoder<BufReader<File>>, String> {
    let file = File::open(uri).map_err(|e| format!("cannot open {uri}: {e}"))?;
    let bytes = file
        .metadata()
        .map_err(|e| format!("cannot open {uri}: {e}"))?
        .len();
    Decoder::builder()
        .with_data(BufReader::new(file))
        .with_byte_len(bytes)
        .build()
        .map_err(|e| format!("cannot decode {uri}: {e}"))
}

/// A decoder over `uri`, with no seek in it.
fn plain(uri: &str) -> Result<Decoder<BufReader<File>>, String> {
    let file = File::open(uri).map_err(|e| format!("cannot open {uri}: {e}"))?;
    Decoder::new(BufReader::new(file)).map_err(|e| format!("cannot decode {uri}: {e}"))
}

/// Scales every sample by a gain that can change while it plays.
struct LiveGain<I> {
    input: I,
    gain: GainCell,
}

impl<I> LiveGain<I> {
    fn new(input: I, gain: GainCell) -> Self {
        Self { input, gain }
    }
}

impl<I: Source> Iterator for LiveGain<I> {
    type Item = Sample;

    fn next(&mut self) -> Option<Sample> {
        let gain = self.gain.get();
        self.input.next().map(|sample| sample * gain)
    }
}

impl<I: Source> Source for LiveGain<I> {
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

/// A [`Source`] that applies a [`Fade`] envelope to a finite span: fade in
/// from the start, fade out into the end.
///
/// The envelope is read from a cell shared with the graph, so a fade edited
/// while the clip plays takes effect within a few milliseconds — the window
/// rodio gives its own volume and pause controls. `into` is how far into the
/// clip this chain started, which shortens the fade-in: the ramp belongs to
/// the clip, not to one particular listen of it.
struct FadeSource<I> {
    input: I,
    cell: Arc<Mutex<Fade>>,
    /// The envelope in force, as last read from `cell`.
    fade: Fade,
    into: Duration,
    length: Duration,
    /// Duration of one interleaved sample of `input`.
    per_sample: Duration,
    /// Position of the next sample to emit.
    pos: Duration,
    /// Samples until `cell` is read again.
    until_refresh: u32,
    /// Samples between reads: five milliseconds of this source's audio.
    refresh_every: u32,
}

/// The envelope as it applies to a span entered `into` late.
fn effective(fade: &Fade, into: Duration) -> Fade {
    Fade {
        fade_in: fade.fade_in.saturating_sub(into),
        ..*fade
    }
}

fn apply_fade<I>(input: I, params: &ClipParams, length: Duration) -> FadeSource<I>
where
    I: Source,
{
    let per_frame = input.sample_rate().get() as u64 * input.channels().get() as u64;
    let per_sample = Duration::from_secs_f64(1.0 / per_frame as f64);
    let refresh_every = u32::try_from(per_frame / 200).unwrap_or(u32::MAX).max(1);
    let fade = params
        .fade
        .lock()
        .map(|f| effective(&f, params.into))
        .unwrap_or_default();
    FadeSource {
        input,
        cell: params.fade.clone(),
        fade,
        into: params.into,
        length,
        per_sample,
        pos: Duration::ZERO,
        until_refresh: refresh_every,
        refresh_every,
    }
}

impl<I: Source> Iterator for FadeSource<I> {
    type Item = Sample;

    fn next(&mut self) -> Option<Sample> {
        let sample = self.input.next()?;
        if self.until_refresh == 0 {
            // A fade edited while this clip plays lands here, within one
            // refresh window of being set.
            if let Ok(fade) = self.cell.lock() {
                self.fade = effective(&fade, self.into);
            }
            self.until_refresh = self.refresh_every;
        }
        self.until_refresh -= 1;
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
    mix(tracks, Some(path.as_ref()), from, to, master, false, false).map(|(d, _)| d)
}

/// Render the mix as a mono delivery: every stereo frame folded to
/// `(L+R)/2`, so a broadcast / single-speaker audience hears the same
/// centered energy the stereo mix carries, without opposite-phase content
/// vanishing. Same staging and frame rules as [`render_to_file`].
pub fn render_to_file_mono(
    tracks: &[Track],
    path: impl AsRef<std::path::Path>,
    from: Duration,
    to: Option<Duration>,
    master: f32,
) -> Result<Duration, String> {
    mix(tracks, Some(path.as_ref()), from, to, master, false, true).map(|(d, _)| d)
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
    mix(tracks, path, from, to, master, true, false).map(|(d, m)| (d, m.expect("measured")))
}

/// Measure the mono fold of the mix — the numbers a mono delivery is judged
/// by — without (or with) writing it.
pub fn render_and_measure_mono(
    tracks: &[Track],
    path: Option<&std::path::Path>,
    from: Duration,
    to: Option<Duration>,
    master: f32,
) -> Result<(Duration, Measurement), String> {
    mix(tracks, path, from, to, master, true, true).map(|(d, m)| (d, m.expect("measured")))
}

/// One mix pass over the shared [`Timeline`]: build the 44.1 kHz stereo
/// mixer with one gain chain per non-empty track, then pull every sample
/// through the master, an optional wav writer and an optional [`Meter`].
///
/// The master sits on the bus's output — one scaling of the whole mix —
/// exactly where it sits in live playback, so a rendered file sounds like
/// the session. The meter folds the same master-scaled stream the writer
/// consumes, so the numbers describe what a listener hears.
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
    mono: bool,
) -> Result<(Duration, Option<Measurement>), String> {
    let mut timeline = Timeline::plan(tracks, from);
    if let Some(to) = to {
        // `to` is a track timecode; the plan's own timeline starts at `from`.
        timeline.truncate(to.saturating_sub(from));
    }
    let (input, source) = mixer::mixer(nz!(2), nz!(44100));
    for track in timeline.tracks() {
        let gain = if track.muted() { 0.0 } else { track.gain() };
        let pan = GainCell::new(track.pan());
        let mut pending: Vec<Box<dyn Source + Send>> = Vec::new();
        for clip in track.clips() {
            // A render never retunes a clip as it goes, but it builds the same
            // parameters a live graph would, so both sides share one chain.
            pending.push(make_source(clip, &ClipParams::new(clip, &pan))?);
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
                channels: if mono { 1 } else { 2 },
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
    let mut meter = measure.then(|| Meter::new(if mono { 1 } else { 2 }, 44100));
    // The master scales the whole mixed stream once, just before it is
    // written and measured — the render's own bus output.
    let source = Gain::new(source, master);
    // Consume whole frames: a stereo pair each time (mono folds the pair to
    // `(L+R)/2`), feed the meter and the writer, and drop a trailing
    // half-frame so the stream the file receives is exactly the stream that
    // was measured.
    let mut frame: [f32; 2] = [0.0; 2];
    let mut filled = 0usize;
    if mono {
        for sample in source {
            frame[filled] = sample;
            filled += 1;
            if filled == 2 {
                let m = (frame[0] + frame[1]) * 0.5;
                if let Some(meter) = meter.as_mut() {
                    meter.push(m);
                }
                if let Some(w) = writer.as_mut() {
                    w.write_sample(m).map_err(|e| format!("cannot write wav: {e}"))?;
                }
                filled = 0;
            }
        }
    } else {
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

/// What probing a source learned: its length and its channel layout.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Probing {
    /// How long the source plays, and how that was learned.
    pub length: SourceLength,
    /// Interleaved channels per frame, as decoded. A mono source reads 1, a
    /// stereo pair 2, a surround file its own count.
    pub channels: u16,
}

/// Learn a source's length — from its container when the container states
/// one, otherwise by decoding to the end — and its channel count. Pure
/// decoding, no device needed, so it works headless (`bo probe <uri>`, tests,
/// CI). Fails only when the file cannot be opened or decoded at all.
pub fn measure(uri: &str) -> Result<Probing, String> {
    let file = File::open(uri).map_err(|e| format!("cannot open {uri}: {e}"))?;
    let decoder = Decoder::new(BufReader::new(file))
        .map_err(|e| format!("cannot decode {uri}: {e}"))?;
    let channels = decoder.channels().get();
    let length = measure_source(decoder);
    Ok(Probing { length, channels })
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
/// form put and planning need.
pub fn probe(uri: &str) -> Result<Duration, String> {
    measure(uri).map(|p| p.length.duration())
}

/// Measure every distinct source in the arrangement: the uri plus what was
/// learned, or why it could not be measured. Duplicate uris are probed once.
pub fn probe_sources(tracks: &[Track]) -> Vec<(String, Result<Probing, String>)> {
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

    /// A wav whose channels carry different amplitudes of the same sine, so
    /// a render can be told which channels still carry signal. Written in
    /// the order of `amps` (L R C LFE Ls Rs when six are given).
    fn write_split_wav(path: &std::path::Path, seconds: u32, rate: u32, amps: &[f32]) {
        let channels = amps.len() as u16;
        let frames = (rate * seconds) as usize;
        let mut data = Vec::with_capacity(frames * amps.len());
        for i in 0..frames {
            let v = (2.0 * std::f32::consts::PI * 440.0 * i as f32 / rate as f32).sin();
            for amp in amps {
                data.extend_from_slice(&((v * amp * 32767.0) as i16).to_le_bytes());
            }
        }
        let bytes = 2u32;
        let block_align = channels * bytes as u16;
        let byte_rate = rate * u32::from(block_align);
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&channels.to_le_bytes());
        wav.extend_from_slice(&rate.to_le_bytes());
        wav.extend_from_slice(&byte_rate.to_le_bytes());
        wav.extend_from_slice(&block_align.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
        wav.extend_from_slice(&data);
        std::fs::write(path, wav).unwrap();
    }

    /// Peak of each channel of a rendered wav, in order.
    fn channel_peaks(path: &std::path::Path) -> Vec<f32> {
        let decoder = Decoder::new(BufReader::new(File::open(path).unwrap())).unwrap();
        let channels = decoder.channels().get() as usize;
        let mut peaks = vec![0.0f32; channels];
        for (n, s) in decoder.enumerate() {
            let ch = n % channels;
            peaks[ch] = peaks[ch].max(s.abs());
        }
        peaks
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
    fn a_centered_mono_source_shares_its_energy_across_the_pair() {
        // A mono source used to be copied to both sides at full gain, which
        // made it 3 dB louder than the file itself. Placed at center it now
        // shares its energy: each side carries √½ of the sample.
        let dir = std::env::temp_dir().join(format!("bo-pan-center-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.wav");
        write_wav(&a, 0.5, 440.0, 0.5);
        let mut track = Track::named("a");
        track.insert(clip_at(a.to_str().unwrap(), 0, 1)).unwrap();

        let out = dir.join("out.wav");
        render_to_file(&[track], &out, Duration::ZERO, None, 1.0).unwrap();
        let peaks = channel_peaks(&out);
        assert_eq!(peaks.len(), 2, "stereo render");
        let want = 0.5 * std::f32::consts::FRAC_1_SQRT_2;
        for (i, p) in peaks.iter().enumerate() {
            assert!((p - want).abs() < 0.02, "channel {i} peaked at {p}, want {want}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn pan_sends_a_mono_source_hard_to_one_side() {
        let dir = std::env::temp_dir().join(format!("bo-pan-side-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.wav");
        write_wav(&a, 0.5, 440.0, 0.5);
        let uri = a.to_str().unwrap();

        let mut left = Track::named("left");
        left.insert(clip_at(uri, 0, 1)).unwrap();
        left.set_pan(-1.0);
        let lout = dir.join("left.wav");
        render_to_file(&[left], &lout, Duration::ZERO, None, 1.0).unwrap();
        let lp = channel_peaks(&lout);
        assert!((lp[0] - 0.5).abs() < 0.02, "left side full, got {}", lp[0]);
        assert!(lp[1] < 0.01, "right side silent, got {}", lp[1]);

        let mut right = Track::named("right");
        right.insert(clip_at(uri, 0, 1)).unwrap();
        right.set_pan(1.0);
        let rout = dir.join("right.wav");
        render_to_file(&[right], &rout, Duration::ZERO, None, 1.0).unwrap();
        let rp = channel_peaks(&rout);
        assert!(rp[0] < 0.01, "left side silent, got {}", rp[0]);
        assert!((rp[1] - 0.5).abs() < 0.02, "right side full, got {}", rp[1]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn balance_moves_a_stereo_source_keeping_its_width() {
        // A stereo source is balanced, not panned: the far side is
        // attenuated to silence while the near side is untouched, so the
        // signal's own left/right shape survives.
        let dir = std::env::temp_dir().join(format!("bo-pan-balance-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.wav");
        write_split_wav(&a, 1, 44_100, &[0.5, 0.25]);
        let uri = a.to_str().unwrap();

        let mut center = Track::named("center");
        center.insert(clip_at(uri, 0, 1)).unwrap();
        let cout = dir.join("center.wav");
        render_to_file(&[center], &cout, Duration::ZERO, None, 1.0).unwrap();
        let cp = channel_peaks(&cout);
        assert!((cp[0] - 0.5).abs() < 0.02 && (cp[1] - 0.25).abs() < 0.02,
                "center leaves the pair alone: {cp:?}");

        let mut right = Track::named("right");
        right.insert(clip_at(uri, 0, 1)).unwrap();
        right.set_pan(1.0);
        let rout = dir.join("right.wav");
        render_to_file(&[right], &rout, Duration::ZERO, None, 1.0).unwrap();
        let rp = channel_peaks(&rout);
        assert!(rp[0] < 0.01, "left dropped at hard right: {}", rp[0]);
        assert!((rp[1] - 0.25).abs() < 0.02, "right untouched: {}", rp[1]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_surround_center_channel_survives_the_stereo_downmix() {
        // The fault this guards: a >2ch source used to be cut to its first
        // two channels, and in 5.1 the voice lives on the center channel —
        // the third one. The BS.775 downmix keeps it, at −3.01 dB on each
        // side of the stereo pair.
        let dir = std::env::temp_dir().join(format!("bo-pan-surround-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.wav");
        write_split_wav(&a, 1, 44_100, &[0.0, 0.0, 0.5, 0.0, 0.0, 0.0]); // L R C LFE Ls Rs
        let mut track = Track::named("a");
        track.insert(clip_at(a.to_str().unwrap(), 0, 1)).unwrap();

        let out = dir.join("out.wav");
        render_to_file(&[track], &out, Duration::ZERO, None, 1.0).unwrap();
        let peaks = channel_peaks(&out);
        let want = 0.5 * std::f32::consts::FRAC_1_SQRT_2;
        assert!((peaks[0] - want).abs() < 0.02, "center reached the left side: {}", peaks[0]);
        assert!((peaks[1] - want).abs() < 0.02, "center reached the right side: {}", peaks[1]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_mono_render_folds_the_stereo_pair() {
        // Mono delivery folds every frame to (L+R)/2, so a broadcast or a
        // single speaker hears the centered energy without opposite-phase
        // content vanishing. A stereo source at L=.5 R=.25 folds to .375.
        let dir = std::env::temp_dir().join(format!("bo-mono-fold-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.wav");
        write_split_wav(&a, 1, 44_100, &[0.5, 0.25]);
        let mut track = Track::named("a");
        track.insert(clip_at(a.to_str().unwrap(), 0, 1)).unwrap();

        let stereo = dir.join("stereo.wav");
        render_to_file(&[track.clone()], &stereo, Duration::ZERO, None, 1.0).unwrap();
        let mono = dir.join("mono.wav");
        render_to_file_mono(&[track], &mono, Duration::ZERO, None, 1.0).unwrap();

        let peak = |p: &std::path::Path| {
            let d = Decoder::new(BufReader::new(File::open(p).unwrap())).unwrap();
            d.fold(0.0f32, |m, s| m.max(s.abs()))
        };
        assert_eq!(
            Decoder::new(BufReader::new(File::open(&mono).unwrap()))
                .unwrap()
                .channels()
                .get(),
            1,
            "a mono delivery is one channel"
        );
        assert!(
            (peak(&mono) - 0.375).abs() < 0.02,
            "fold of (0.5 + 0.25)/2: {}",
            peak(&mono)
        );
        assert!(
            (peak(&stereo) - 0.5).abs() < 0.02,
            "the stereo original keeps its left level: {}",
            peak(&stereo)
        );
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
        let probing = measure(a.to_str().unwrap()).unwrap();
        assert!(
            matches!(probing.length, SourceLength::Exact(_)),
            "a wav states its length: {probing:?}"
        );
        assert_eq!(probing.channels, 2, "the probe reports the layout");
        let d = probing.length.duration();
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
        let probing = measure(zero.to_str().unwrap()).unwrap();
        assert!(
            matches!(probing.length, SourceLength::Estimated(_)),
            "no frames in the header, so the length is decoded: {probing:?}"
        );
        assert_eq!(probing.length.duration(), Duration::ZERO);

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

    // -- the live graph, over a mixer with no device in sight ---------------

    /// A mono wav whose samples encode their own frame index: a 1 Hz
    /// sawtooth, so any pulled sample says which frame of the source it came
    /// from. A rebuild that loses or repeats audio shows up as a jump in the
    /// recovered index.
    fn write_index_wav(path: &std::path::Path, seconds: u32) {
        let rate = 44_100u32;
        let frames = (rate * seconds) as usize;
        let mut data = Vec::with_capacity(frames * 2);
        for i in 0..frames {
            let v = (i % rate as usize) as f32 / rate as f32 * 2.0 - 1.0;
            data.extend_from_slice(&((v * 32767.0) as i16).to_le_bytes());
        }
        let byte_rate = rate * 2;
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&1u16.to_le_bytes()); // mono
        wav.extend_from_slice(&rate.to_le_bytes());
        wav.extend_from_slice(&byte_rate.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
        wav.extend_from_slice(&data);
        std::fs::write(path, wav).unwrap();
    }

    /// The frame index a sample of [`write_index_wav`] came from, to within
    /// the quantization of 16-bit audio.
    fn index_of(sample: Sample) -> usize {
        let v = (sample.clamp(-1.0, 1.0) + 1.0) / 2.0;
        (v * 44_100.0).round() as usize % 44_100
    }

    /// Pull `frames` stereo frames out of a mixer.
    fn pull(output: &mut impl Iterator<Item = Sample>, frames: usize) -> Vec<Sample> {
        (0..frames * 2)
            .map(|_| output.next().expect("a mixer with a clock tap never ends"))
            .collect()
    }

    /// The loudest sample of channel 0.
    fn peak(samples: &[Sample]) -> f32 {
        samples
            .iter()
            .step_by(2)
            .fold(0.0, |loudest, s| loudest.max(s.abs()))
    }

    /// A graph over a plain 44.1 kHz stereo mixer, and the output to pull.
    fn graph_on_a_mixer() -> (Graph, mixer::MixerSource) {
        let (mixer, output) = mixer::mixer(nz!(2), nz!(44100));
        (Graph::new(mixer, 44_100), output)
    }

    #[test]
    fn a_rebuild_keeps_the_sound_going() {
        // The reported fault: `apply` during playback was audible as a short
        // interruption. It came from tearing the graph down before building
        // the next one, which left the mixer with nothing to pull and the
        // device writing silence for a whole buffer. Building first and
        // letting go second means the sound never stops, so this pulls across
        // twenty rebuilds and looks for a quiet moment.
        let dir = std::env::temp_dir().join(format!("bo-graph-gap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tone = dir.join("tone.wav");
        write_wav(&tone, 8.0, 440.0, 0.5);
        let mut track = Track::named("a");
        track.insert(clip_at(tone.to_str().unwrap(), 0, 8)).unwrap();
        let tracks = [track];

        let (mut graph, mut output) = graph_on_a_mixer();
        graph.play(&tracks, Duration::ZERO).unwrap();
        let mut quietest = f32::MAX;
        for _ in 0..20 {
            // A tenth of a second of sound, then a rebuild from where the
            // audio really is — exactly what `apply` does.
            let samples = pull(&mut output, 4_410);
            graph.play(&tracks, graph.position()).unwrap();
            for window in samples.chunks(512) {
                quietest = quietest.min(peak(window));
            }
        }
        assert!(
            quietest > 0.1,
            "the quietest 256 frames across 20 rebuilds peaked at {quietest}: the sound stopped"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_rebuild_picks_up_the_frame_it_left_off_on() {
        // The playhead used to be wall-clock bookkeeping, so a rebuild
        // re-entered ahead of what had actually been heard and the audio in
        // between was lost. A graph that counts the frames it is pulled for
        // reports where the sound really is, and a rebuild from that reading
        // resumes on the very next frame.
        let dir = std::env::temp_dir().join(format!("bo-graph-clock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ramp = dir.join("ramp.wav");
        write_index_wav(&ramp, 8);
        let mut track = Track::named("a");
        track.insert(clip_at(ramp.to_str().unwrap(), 0, 8)).unwrap();
        // A mono source reads back at full amplitude only hard against one
        // side: centered, the constant-power law shares its energy across the
        // pair, and the frame index lives in the sample's amplitude.
        track.set_pan(-1.0);
        let tracks = [track];

        let (mut graph, mut output) = graph_on_a_mixer();
        graph.play(&tracks, Duration::ZERO).unwrap();
        let first = pull(&mut output, 4_410);
        // Within the quantization of 16-bit audio: frame zero is stored as
        // -32767, which reads back a hair above the bottom of the sawtooth.
        assert!(
            index_of(first[0]) <= 1,
            "the graph starts at frame zero, got {}",
            index_of(first[0])
        );
        assert!(
            (graph.position().as_secs_f64() - 0.1).abs() < 1e-9,
            "a tenth of a second pulled, position {:?}",
            graph.position()
        );

        graph.play(&tracks, graph.position()).unwrap();
        let next = pull(&mut output, 4_410);
        // Past the few milliseconds an old voice takes to let go, the frames
        // must continue where they left off: nothing lost, nothing repeated.
        for (k, sample) in next.iter().step_by(2).enumerate().skip(1_000) {
            let want = 4_410 + k;
            let got = index_of(*sample);
            assert!(
                (got as i64 - want as i64).abs() <= 1,
                "frame {k} after the rebuild carries source frame {got}, expected {want}"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_paused_graph_holds_its_position() {
        let dir = std::env::temp_dir().join(format!("bo-graph-pause-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tone = dir.join("tone.wav");
        write_wav(&tone, 4.0, 440.0, 0.5);
        let mut track = Track::named("a");
        track.insert(clip_at(tone.to_str().unwrap(), 0, 4)).unwrap();
        let tracks = [track];

        let (mut graph, mut output) = graph_on_a_mixer();
        graph.play(&tracks, Duration::ZERO).unwrap();
        let _ = pull(&mut output, 4_410);
        graph.pause();
        let held = graph.position();
        // A paused queue is still pulled, for silence; the clock must not run
        // on without sound, or the next rebuild would enter ahead of it.
        let paused = pull(&mut output, 4_410);
        assert!(
            peak(&paused[paused.len() - 2_000..]) < 1e-6,
            "a paused graph is silent"
        );
        assert_eq!(graph.position(), held, "and its clock is held");
        graph.resume();
        let resumed = pull(&mut output, 4_410);
        assert!(
            peak(&resumed[resumed.len() - 2_000..]) > 0.1,
            "resuming brings the sound back"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn gains_land_on_the_running_graph() {
        // A track's volume and a clip's gain used to be baked into the graph
        // when it was built, so changing one meant rebuilding — and the
        // rebuild was the interruption. Both are values the running chain
        // reads, so setting one is a store.
        let dir = std::env::temp_dir().join(format!("bo-graph-gain-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tone = dir.join("tone.wav");
        write_wav(&tone, 8.0, 440.0, 0.5);
        let uri = tone.to_str().unwrap();
        let mut track = Track::named("a");
        let id = track.insert(clip_at(uri, 0, 8)).unwrap();
        // Hard left, so the mono tone reads back at its written amplitude on
        // channel 0 (the constant-power law shares it across the pair when
        // centered).
        track.set_pan(-1.0);

        let (mut graph, mut output) = graph_on_a_mixer();
        graph.play(&[track.clone()], Duration::ZERO).unwrap();
        let full = peak(&pull(&mut output, 4_410));
        assert!(full > 0.4, "the tone is audible, peak {full}");

        // A track's gain, landed on the graph that is sounding it.
        let mut quieter = track.clone();
        quieter.set_volume(0.25);
        assert!(graph.land(&[quieter], graph.position(), &Change::TrackGain(0)));
        let after = pull(&mut output, 4_410);
        // Past the window in which the queue picks the new value up.
        let tail = peak(&after[2_000..]);
        assert!(
            (tail - full * 0.25).abs() < 0.02,
            "a quarter gain: {tail} against {}",
            full * 0.25
        );
        assert_eq!(graph.voices.len(), 1, "the same voice, retuned");

        // A clip's gain, landed on the chain playing it.
        let mut clipped = track.clone();
        clipped.clip_mut(id).unwrap().gain = 0.5;
        assert!(graph.land(&[clipped], graph.position(), &Change::ClipParams(0, id)));
        let after = pull(&mut output, 4_410);
        let tail = peak(&after[2_000..]);
        assert!(
            (tail - full * 0.25 * 0.5).abs() < 0.02,
            "half the clip's gain on top: {tail} against {}",
            full * 0.125
        );

        // A mute is a gain of zero, by the same route.
        let mut muted = track.clone();
        muted.set_muted(true);
        assert!(graph.land(&[muted], graph.position(), &Change::TrackGain(0)));
        let after = pull(&mut output, 4_410);
        assert!(
            peak(&after[2_000..]) < 1e-6,
            "a muted track contributes nothing"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_pan_lands_on_the_running_graph() {
        // A placement is read from a shared cell, so panning a track that is
        // sounding is one store — no rebuild, same voice.
        let dir = std::env::temp_dir().join(format!("bo-graph-pan-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tone = dir.join("tone.wav");
        write_wav(&tone, 8.0, 440.0, 0.5);
        let uri = tone.to_str().unwrap();
        let mut track = Track::named("a");
        track.insert(clip_at(uri, 0, 8)).unwrap();
        track.set_pan(-1.0); // hard left: channel 0 at full written amplitude

        let (mut graph, mut output) = graph_on_a_mixer();
        graph.play(&[track.clone()], Duration::ZERO).unwrap();
        let full = peak(&pull(&mut output, 4_410));
        assert!((full - 0.5).abs() < 0.02, "hard left, peak {full}");

        let mut centered = track.clone();
        centered.set_pan(0.0);
        assert!(graph.land(&[centered], graph.position(), &Change::TrackPan(0)));
        assert_eq!(graph.voices.len(), 1, "the same voice, re-placed");
        let after = pull(&mut output, 4_410);
        let tail = peak(&after[2_000..]);
        let want = 0.5 * std::f32::consts::FRAC_1_SQRT_2;
        assert!(
            (tail - want).abs() < 0.02,
            "centered mono shares its energy: {tail} against {want}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn master_lands_on_the_running_bus() {
        // The master used to be multiplied into every voice at build time, so
        // moving it meant a walk over the voices (cheap, but the wrong place
        // conceptually — it lives on the bus). Now it is one gain on the
        // bus's output: a live `set master` is a store, and the whole mix
        // scales without any voice being touched.
        let dir = std::env::temp_dir().join(format!("bo-graph-master-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tone = dir.join("tone.wav");
        write_wav(&tone, 8.0, 440.0, 0.5);
        let mut track = Track::named("a");
        track.insert(clip_at(tone.to_str().unwrap(), 0, 8)).unwrap();
        track.set_pan(-1.0); // full amplitude on channel 0

        let (mut graph, mut output) = graph_on_a_mixer();
        graph.play(&[track], Duration::ZERO).unwrap();
        let full = peak(&pull(&mut output, 4_410));
        assert!((full - 0.5).abs() < 0.02, "full master, peak {full}");

        graph.set_master(0.25);
        let after = pull(&mut output, 4_410);
        let tail = peak(&after[2_000..]);
        assert!(
            (tail - 0.125).abs() < 0.02,
            "a quarter master on the bus output: {tail}"
        );
        assert_eq!(graph.voices.len(), 1, "no voice was rebuilt or retuned");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_clip_placed_past_the_queue_joins_it() {
        // The live-show move: queue the next item while the current one plays.
        // It joins the running queue instead of waiting for a rebuild.
        let dir = std::env::temp_dir().join(format!("bo-graph-append-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tone = dir.join("tone.wav");
        write_wav(&tone, 4.0, 440.0, 0.5);
        let uri = tone.to_str().unwrap();
        let mut track = Track::named("a");
        track.insert(clip_at(uri, 0, 1)).unwrap();

        let (mut graph, mut output) = graph_on_a_mixer();
        graph.play(&[track.clone()], Duration::ZERO).unwrap();
        let _ = pull(&mut output, 2_205); // half a second in

        let mut extended = track.clone();
        extended.insert(clip_at(uri, 1, 1)).unwrap();
        assert!(graph.land(&[extended], graph.position(), &Change::Appended(0)));
        assert_eq!(graph.voices.len(), 1, "one voice, a longer queue");

        // Two more seconds: the first clip ends half a second in, and without
        // the append everything after that would be silence.
        let across = pull(&mut output, 8_820);
        let past_the_join = &across[across.len() / 2..];
        assert!(
            peak(past_the_join) > 0.1,
            "the appended clip is sounding past the join"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_clip_placed_before_queued_material_needs_a_rebuild() {
        // A queue can be extended, not re-ordered: a clip dropped into a gap
        // ahead of material already queued is the case that still needs a
        // graph of its own, and saying so is what makes `apply` rebuild.
        let dir = std::env::temp_dir().join(format!("bo-graph-gap-2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tone = dir.join("tone.wav");
        write_wav(&tone, 4.0, 440.0, 0.5);
        let uri = tone.to_str().unwrap();
        let mut track = Track::named("a");
        track.insert(clip_at(uri, 0, 1)).unwrap();
        track.insert(clip_at(uri, 5, 1)).unwrap();

        let (mut graph, mut output) = graph_on_a_mixer();
        graph.play(&[track.clone()], Duration::ZERO).unwrap();
        let _ = pull(&mut output, 2_205);

        let mut filled = track.clone();
        filled.insert(clip_at(uri, 2, 1)).unwrap();
        assert!(
            !graph.land(&[filled.clone()], graph.position(), &Change::Appended(0)),
            "a clip ahead of queued material cannot be appended"
        );
        // The rebuild it forces does land it.
        graph.play(std::slice::from_ref(&filled), graph.position()).unwrap();
        assert!(peak(&pull(&mut output, 4_410)) > 0.1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_rebuild_that_cannot_be_built_leaves_the_sound_alone() {
        // A rebuild used to tear the graph down first, so a source that could
        // not be opened silenced a transport that went on reporting itself
        // playing. The graph is built before the old one is let go, so a
        // build that fails leaves what is sounding untouched.
        let dir = std::env::temp_dir().join(format!("bo-graph-atomic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tone = dir.join("tone.wav");
        write_wav(&tone, 4.0, 440.0, 0.5);
        let mut track = Track::named("a");
        track.insert(clip_at(tone.to_str().unwrap(), 0, 4)).unwrap();

        let (mut graph, mut output) = graph_on_a_mixer();
        graph.play(&[track.clone()], Duration::ZERO).unwrap();
        let _ = pull(&mut output, 4_410);

        // A second clip whose file is not there: the next rebuild cannot be
        // built at all.
        let mut broken = track.clone();
        let gone = dir.join("gone.wav");
        broken.insert(clip_at(gone.to_str().unwrap(), 4, 4)).unwrap();
        assert!(
            graph.play(&[broken], graph.position()).is_err(),
            "a source that cannot be opened refuses the rebuild"
        );
        let after = pull(&mut output, 4_410);
        assert!(
            peak(&after) > 0.1,
            "and the graph that was sounding keeps sounding"
        );
        assert_eq!(graph.voices.len(), 1, "the old voice is still the one");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn seeking_lands_on_the_same_sample_as_decoding_forward() {
        // An in-point is promised sample-accurate, so the fast way in is only
        // worth taking if it lands exactly where the slow way would. Both are
        // compared sample for sample, and the seek is asserted to be the path
        // actually under test.
        let dir = std::env::temp_dir().join(format!("bo-seek-exact-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.wav");
        write_wav_full(&a, 4.0, 440.0, 0.5, 44_100, 2, 16);
        let uri = a.to_str().unwrap();
        let target = Duration::from_millis(2_500);

        let mut seekable = seekable(uri).unwrap();
        assert!(
            seekable.try_seek(target).is_ok(),
            "a wav seeks, so the fast path is the one being compared"
        );

        let mut fast = positioned(uri, target).unwrap();
        let mut slow = decoded_forward(uri, target).unwrap();
        for i in 0..4_410 {
            let (seeked, decoded) = (fast.next().unwrap(), slow.next().unwrap());
            assert!(
                (seeked - decoded).abs() < 1e-6,
                "sample {i} at {target:?}: seeked {seeked}, decoded {decoded}"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
