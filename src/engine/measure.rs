//! Offline level measurement of a mixed stereo stream.
//!
//! A [`Meter`] folds over the same interleaved sample stream a wav writer
//! would consume, so the numbers describe exactly what a render writes (and
//! what playback plans). Nothing is cached beyond tiny accumulators: sample
//! peak, RMS, true peak (4× oversampled), per-second windows for the
//! loudest/quietest second, and — in [`lufs`] — EBU R128 loudness
//! (integrated, momentary/short-term maxima, LRA).
//!
//! Pure DSP, no rodio: tests synthesize signals directly.

use std::time::Duration;

pub mod lufs;

/// One measured span: absolute levels in dBFS and loudness in LUFS.
#[derive(Debug, Clone, PartialEq)]
pub struct Measurement {
    /// The span that was actually measured (plan end, or the cut range).
    pub span: Duration,
    /// Sample peak over all channels, dBFS.
    pub peak_db: f32,
    /// True peak (4× oversampled) over all channels, dBFS.
    pub true_peak_db: f32,
    /// RMS over all samples of all channels, dBFS.
    pub rms_db: f32,
    /// Integrated loudness, LUFS. `None` for spans under 3 s, or when the
    /// R128 gating finds nothing to measure (all-silent).
    pub integrated_lufs: Option<f32>,
    /// Loudest 400 ms block, LUFS.
    pub momentary_max_lufs: Option<f32>,
    /// Loudest 3 s window, LUFS.
    pub short_term_max_lufs: Option<f32>,
    /// Loudness range, LU.
    pub lra: Option<f32>,
    /// Start of the loudest second (full or trailing partial), as the count
    /// of whole seconds before it.
    pub loudest_1s: Option<Duration>,
    /// Start of the quietest second (full or trailing partial).
    pub quietest_1s: Option<Duration>,
}

/// A streaming fold over interleaved samples.
///
/// Feed every sample the mix produces, in order; [`Meter::finish`] returns
/// the measurement. Per-channel state (peak, upsampler) is keyed off the
/// sample position, so the channel count must stay fixed for the whole
/// stream — it does: the offline mixer always emits stereo at 44.1 kHz.
pub struct Meter {
    channels: usize,
    rate: u64,
    /// Channel of the sample being pushed.
    n: usize,
    /// Sum of |sample|^2 over the whole span (all channels).
    sum_sq: f64,
    /// Whole-span peak, dB.
    peak: f32,
    /// True-peak estimate over the 4×-oversampled stream.
    true_peak: f32,
    /// 4× upsample state, one per channel.
    up: Vec<Upsampler>,
    /// Frames (sample groups) seen so far.
    frames: u64,
    /// Current 1 s window: sum of squares and sample count (both channels).
    sec_sum_sq: f64,
    sec_samples: u64,
    /// Loudest/quietest completed-or-final window so far.
    best_sec: Option<(u64, f64)>, // (start second, per-sample mean square)
    worst_sec: Option<(u64, f64)>,
    /// EBU R128 accumulation.
    loudness: lufs::R128,
}

impl Meter {
    /// A meter for `channels` interleaved channels at `sample_rate` Hz.
    pub fn new(channels: u16, sample_rate: u32) -> Self {
        Self {
            channels: channels as usize,
            rate: u64::from(sample_rate),
            n: 0,
            sum_sq: 0.0,
            peak: 0.0,
            true_peak: 0.0,
            up: (0..channels as usize).map(|_| Upsampler::new()).collect(),
            frames: 0,
            sec_sum_sq: 0.0,
            sec_samples: 0,
            best_sec: None,
            worst_sec: None,
            loudness: lufs::R128::new(channels, sample_rate),
        }
    }

    /// Feed one interleaved sample.
    #[inline]
    pub fn push(&mut self, sample: f32) {
        let ch = self.n;
        self.n = (self.n + 1) % self.channels;
        let v = f64::from(sample);
        self.sum_sq += v * v;
        self.peak = self.peak.max(sample.abs());
        self.true_peak = self.true_peak.max(self.up[ch].push(sample));
        self.sec_sum_sq += v * v;
        self.sec_samples += 1;
        self.loudness.push(sample);
        if self.n == 0 {
            // A frame boundary: maybe a whole second has passed.
            self.frames += 1;
            if self.frames % self.rate == 0 {
                self.close_second(self.frames / self.rate - 1);
            }
        }
    }

    /// Fold the current (possibly partial) window into the candidates and
    /// reset it. Called at every completed second, and once at the end for
    /// the trailing partial window.
    fn close_second(&mut self, start: u64) {
        if self.sec_samples == 0 {
            return;
        }
        let energy = self.sec_sum_sq / self.sec_samples as f64;
        match self.best_sec {
            Some((_, e)) if e >= energy => {}
            _ => self.best_sec = Some((start, energy)),
        }
        match self.worst_sec {
            Some((_, e)) if e <= energy => {}
            _ => self.worst_sec = Some((start, energy)),
        }
        self.sec_sum_sq = 0.0;
        self.sec_samples = 0;
    }

    /// Finish the stream and produce the measurement.
    pub fn finish(mut self) -> Measurement {
        if self.sec_samples > 0 {
            // Trailing partial window: its start is the whole-second count
            // before it began.
            let partial_frames = self.sec_samples / self.channels as u64;
            self.close_second((self.frames - partial_frames) / self.rate);
        }
        let span = if self.frames > 0 {
            Duration::from_secs_f64(self.frames as f64 / self.rate as f64)
        } else {
            Duration::ZERO
        };
        let total = (self.frames * self.channels as u64) as f64;
        let rms = if total > 0.0 {
            (self.sum_sq / total).sqrt() as f32
        } else {
            0.0
        };
        let db = |v: f32| {
            if v > 0.0 {
                20.0 * v.log10()
            } else {
                f32::NEG_INFINITY
            }
        };
        // EBU R128 needs a few seconds of program for its gates; shorter
        // spans get absolute levels only.
        let loudness: Option<&lufs::R128> = (self.frames >= 3 * self.rate).then_some(&self.loudness);
        Measurement {
            span,
            peak_db: db(self.peak),
            true_peak_db: db(self.true_peak),
            rms_db: db(rms),
            integrated_lufs: loudness.and_then(|l| l.integrated()),
            momentary_max_lufs: loudness.and_then(|l| l.momentary_max()),
            short_term_max_lufs: loudness.and_then(|l| l.short_term_max()),
            lra: loudness.and_then(|l| l.lra()),
            loudest_1s: self.best_sec.map(|(s, _)| Duration::from_secs(s)),
            quietest_1s: self.worst_sec.map(|(s, _)| Duration::from_secs(s)),
        }
    }
}

/// 4× oversampling for true-peak estimation: zero-phase interpolation with a
/// windowed-sinc kernel whose cutoff is 0.9× Nyquist, evaluated at the three
/// quarter positions behind the newest sample (a one-sample-plus delay keeps
/// the kernel's symmetric neighborhood inside the ring).
///
/// The kernel is precomputed on the only offsets it is ever asked for —
/// integer offsets plus one of {0.25, 0.5, 0.75} — so each sample costs a
/// couple of hundred multiply-adds, no transcendentals.
struct Upsampler {
    /// One-sided kernel support in input samples.
    half: usize,
    /// Delay applied before evaluating, so the neighborhood never needs a
    /// sample newer than the one just pushed.
    delay: usize,
    /// Ring of the last `2*half + 2` samples, `ring[i % len]`.
    ring: Vec<f32>,
    /// Absolute count of samples pushed.
    count: u64,
    /// Kernel table: `phi[phase][m + half]` = φ(m + phase), phase index 0..3
    /// for offsets 0.25, 0.5, 0.75, m in −half..=half.
    phi: Vec<[f32; 3]>,
}

impl Upsampler {
    fn new() -> Self {
        const HALF: usize = 24;
        let a = 0.9f32; // 2 × cutoff (0.45 cycles/sample)
        let mut phi = vec![[0.0f32; 3]; 2 * HALF + 1];
        for (m, row) in phi.iter_mut().enumerate() {
            let m = m as f32 - HALF as f32;
            for (p, phase) in [0.25f32, 0.5, 0.75].into_iter().enumerate() {
                let u = m + phase;
                if u.abs() > HALF as f32 {
                    continue;
                }
                let au = a * u.abs();
                let sinc = if au < 1e-6 {
                    1.0
                } else {
                    (std::f32::consts::PI * au).sin() / (std::f32::consts::PI * au)
                };
                let win = 0.54 - 0.46 * (std::f32::consts::PI * (u.abs() / HALF as f32 + 1.0)).cos();
                row[p] = a * sinc * win;
            }
        }
        Self {
            half: HALF,
            delay: HALF + 1,
            ring: vec![0.0; 2 * HALF + 2],
            count: 0,
            phi,
        }
    }

    /// Push one input sample; return the max magnitude over the sample and
    /// the three reconstructed quarter positions one delay behind it.
    fn push(&mut self, sample: f32) -> f32 {
        let len = self.ring.len() as u64;
        let n = self.count;
        self.ring[(n % len) as usize] = sample;
        self.count += 1;
        let mut peak = sample.abs();
        if n < self.delay as u64 {
            return peak;
        }
        // Evaluate the reconstruction at n - delay + phase for each phase.
        let base = n - self.delay as u64; // integer sample before the point
        let m0 = (base as i64) - self.half as i64;
        for p in 0..3 {
            let mut acc = 0.0f32;
            for m in 0..2 * self.half + 1 {
                let i = m0 + m as i64;
                if i < 0 {
                    continue;
                }
                acc += self.ring[(i as u64 % len) as usize] * self.phi[m][p];
            }
            peak = peak.max(acc.abs());
        }
        peak
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    /// Interleaved stereo samples of a sine at `amp` on both channels.
    fn stereo_sine(seconds: f32, freq: f32, amp: f32, rate: u32) -> Vec<f32> {
        let n = (rate as f32 * seconds) as usize;
        let mut out = Vec::with_capacity(n * 2);
        for i in 0..n {
            let v = amp * (2.0 * std::f32::consts::PI * freq * i as f32 / rate as f32).sin();
            out.push(v);
            out.push(v);
        }
        out
    }

    fn meter_of(samples: &[f32], channels: u16, rate: u32) -> Measurement {
        let mut m = Meter::new(channels, rate);
        for s in samples {
            m.push(*s);
        }
        m.finish()
    }

    #[test]
    fn peak_and_rms_of_a_sine() {
        // Full-scale sine: peak 0 dBFS, RMS -3.01 dBFS.
        let m = meter_of(&stereo_sine(1.0, 440.0, 1.0, 44100), 2, 44100);
        assert!((m.peak_db - 0.0).abs() < 0.01, "peak {}", m.peak_db);
        assert!((m.rms_db - (-3.01)).abs() < 0.02, "rms {}", m.rms_db);
        assert!((m.span.as_secs_f64() - 1.0).abs() < 0.01, "{:?}", m.span);
        // Half amplitude: -6.02 dB everywhere.
        let m = meter_of(&stereo_sine(1.0, 440.0, 0.5, 44100), 2, 44100);
        assert!((m.peak_db - (-6.02)).abs() < 0.02, "peak {}", m.peak_db);
        assert!((m.rms_db - (-9.03)).abs() < 0.02, "rms {}", m.rms_db);
    }

    #[test]
    fn loudest_and_quietest_seconds() {
        // 3 s: quiet, loud, quietest.
        let mut s = stereo_sine(1.0, 440.0, 0.1, 44100);
        s.extend(stereo_sine(1.0, 440.0, 1.0, 44100));
        s.extend(stereo_sine(1.0, 440.0, 0.05, 44100));
        let m = meter_of(&s, 2, 44100);
        assert_eq!(m.loudest_1s, Some(secs(1)), "the loud second");
        assert_eq!(m.quietest_1s, Some(secs(2)), "the quietest second");
    }

    #[test]
    fn true_peak_finds_inter_sample_peaks() {
        // Low frequency: sample peak ≈ true peak.
        let m = meter_of(&stereo_sine(1.0, 440.0, 1.0, 44100), 2, 44100);
        assert!(m.true_peak_db >= m.peak_db - 1e-3);
        assert!((m.true_peak_db - 0.0).abs() < 0.05, "tp {}", m.true_peak_db);
        // A commensurate tone whose crests always fall between samples:
        // 7350 Hz is exactly fs/6, a 60° phase step that never lands on a
        // crest, so the sample peak is sin(60°) = −1.25 dBFS while the true
        // peak is 0 dBFS. The 4× interpolator must recover it.
        let m = meter_of(&stereo_sine(1.0, 7350.0, 1.0, 44100), 2, 44100);
        assert!(m.peak_db < -1.0, "sample peak {:.2} should miss the crest", m.peak_db);
        assert!(m.peak_db > -1.6, "sample peak {:.2} sane", m.peak_db);
        assert!(m.true_peak_db > -0.1, "true peak {:.2} should find it", m.true_peak_db);
        // Never below the sample peak, whatever the tone.
        let m = meter_of(&stereo_sine(0.5, 997.0, 0.5, 44100), 2, 44100);
        assert!(m.true_peak_db >= m.peak_db - 1e-3);
    }

    #[test]
    fn silence_measures_as_minus_infinity() {
        let m = meter_of(&[0.0f32; 44100 * 2], 2, 44100);
        assert_eq!(m.peak_db, f32::NEG_INFINITY);
        assert_eq!(m.rms_db, f32::NEG_INFINITY);
        assert_eq!(m.integrated_lufs, None);
    }

    #[test]
    fn meter_handles_partial_final_second() {
        // 2.5 s: the trailing half second is still a candidate.
        let s = stereo_sine(2.5, 440.0, 0.5, 44100);
        let m = meter_of(&s, 2, 44100);
        assert!(m.loudest_1s.is_some());
        assert!((m.span.as_secs_f64() - 2.5).abs() < 0.01, "{:?}", m.span);
    }
}
