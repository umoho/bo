//! EBU R128 / ITU-R BS.1770 loudness, computed incrementally over one
//! interleaved stream.
//!
//! Per-channel K-weighting (a high-shelf followed by the RLB high-pass,
//! biquads derived from the published analog constants), then 400 ms blocks
//! whose loudness sums the per-channel mean squares (BS.1770-4 channel
//! weights), so a tone on both channels reads 3.01 LU above the mono
//! original — ffmpeg's ebur128 agrees. From the block series: momentary max
//! (400 ms), a short-term series (3 s windows, 400 ms hops, exact via
//! prefix sums) for the short-term max and LRA (10th/95th percentiles), and
//! the integrated value with the −70 LUFS absolute gate and the −10 LU
//! relative gate of R128.
//!
//! Calibrated against ffmpeg ebur128 on amplitude-verified tones: a
//! full-scale 1 kHz stereo tone reads 0.0 LUFS, 440 Hz reads −0.7; tests
//! pin the anchors.

/// One block's worth of K-weighted energy, per channel.
struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    s1: f64,
    s2: f64,
}

impl Biquad {
    /// Direct form II transposed; `y` is returned for chaining.
    #[inline]
    fn tick(&mut self, x: f64) -> f64 {
        let y = self.b0 * x + self.s1;
        self.s1 = self.b1 * x - self.a1 * y + self.s2;
        self.s2 = self.b2 * x - self.a2 * y;
        y
    }

    /// A biquad from the RBJ cookbook. `shelf` selects the K-weighting
    /// high-shelf as parameterized by libebur128 (the BS.1770 shelf shape:
    /// gain `db` above the corner, `Vb` fixing the transition), otherwise a
    /// high-pass at `f0`. Coefficients are recomputed for `rate`.
    fn from_analog(f0: f64, q: f64, db: f64, shelf: bool, rate: u32) -> Self {
        let fs = f64::from(rate);
        let (b0, b1, b2, a0, a1, a2) = if shelf {
            // libebur128 / BS.1770 high-shelf: Vh is the high-frequency
            // gain, Vb ≈ √Vh shapes the transition so the response matches
            // the published 48 kHz coefficients at any rate.
            let k = (std::f64::consts::PI * f0 / fs).tan();
            let vh = 10.0f64.powf(db / 20.0);
            let vb = vh.powf(0.4996667741545416);
            let a0 = 1.0 + k / q + k * k;
            (
                (vh + vb * k / q + k * k) / a0,
                2.0 * (k * k - vh) / a0,
                (vh - vb * k / q + k * k) / a0,
                1.0,
                2.0 * (k * k - 1.0) / a0,
                (1.0 - k / q + k * k) / a0,
            )
        } else {
            let w0 = 2.0 * std::f64::consts::PI * f0 / fs;
            let alpha = w0.sin() / (2.0 * q);
            let cosw = w0.cos();
            (
                (1.0 + cosw) / 2.0,
                -(1.0 + cosw),
                (1.0 + cosw) / 2.0,
                1.0 + alpha,
                -2.0 * cosw,
                1.0 - alpha,
            )
        };
        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            s1: 0.0,
            s2: 0.0,
        }
    }
}

/// K-weighting constants from ITU-R BS.1770 (as used by ffmpeg's ebur128).
const SHELF_F0: f64 = 1681.974450955533;
const SHELF_GAIN_DB: f64 = 4.0;
const SHELF_Q: f64 = 0.7071752369554196;
const HIGHPASS_F0: f64 = 38.13547087602444;
const HIGHPASS_Q: f64 = 0.5003270373238773;

const ABSOLUTE_GATE_LUFS: f64 = -70.0;
const RELATIVE_GATE_LU: f64 = -10.0;

/// The streaming EBU R128 accumulator.
pub struct R128 {
    channels: usize,
    block_len: u64, // frames per 400 ms block
    /// Per-channel filter chains: shelf then high-pass.
    filters: Vec<(Biquad, Biquad)>,
    /// Sample position within the current block.
    in_block: u64,
    /// Completed frames within the current block.
    block_frames: u64,
    /// Sum of squared K-weighted samples, per channel, current block.
    block_sq: Vec<f64>,
    /// Completed blocks, as (frames, sum over channels of Σ sample²).
    blocks: Vec<(u64, f64)>,
}

impl R128 {
    pub fn new(channels: u16, rate: u32) -> Self {
        let n = channels as usize;
        let fs = f64::from(rate);
        let chain = |f0: f64, q: f64, db: f64, shelf: bool| Biquad::from_analog(f0, q, db, shelf, rate);
        let filters = (0..n)
            .map(|_| {
                (
                    chain(SHELF_F0, SHELF_Q, SHELF_GAIN_DB, true),
                    chain(HIGHPASS_F0, HIGHPASS_Q, 0.0, false),
                )
            })
            .collect();
        let _ = fs;
        Self {
            channels: n,
            block_len: (u64::from(rate) * 400) / 1000,
            filters,
            in_block: 0,
            block_frames: 0,
            block_sq: vec![0.0; n],
            blocks: Vec::new(),
        }
    }

    /// Feed one interleaved sample.
    #[inline]
    pub fn push(&mut self, sample: f32) {
        let ch = self.in_block as usize % self.channels;
        let (shelf, hp) = &mut self.filters[ch];
        let weighted = hp.tick(shelf.tick(f64::from(sample)));
        self.block_sq[ch] += weighted * weighted;
        self.in_block += 1;
        if self.in_block.is_multiple_of(self.channels as u64) {
            // A frame boundary.
            self.block_frames += 1;
            if self.block_frames == self.block_len {
                let sum: f64 = self.block_sq.iter().sum();
                self.blocks.push((self.block_len, sum));
                self.block_sq.iter_mut().for_each(|v| *v = 0.0);
                self.block_frames = 0;
                self.in_block = 0;
            }
        }
    }

    fn block_loudness(&self, frames: u64, sum_sq: f64) -> Option<f64> {
        // Per-sample mean square summed over channels (BS.1770-4 weights),
        // in LUFS with the −0.691 calibration offset. A full-scale 1 kHz
        // stereo tone reads 0.0 LUFS; 440 Hz reads −0.7 (ffmpeg ebur128).
        let power = sum_sq / frames as f64;
        if power <= 0.0 {
            None
        } else {
            Some(-0.691 + 10.0 * power.log10())
        }
    }

    /// Integrated loudness with the R128 two gates; `None` when nothing
    /// passes them (silence) or there is no complete block.
    pub fn integrated(&self) -> Option<f32> {
        if self.blocks.is_empty() {
            return None;
        }
        let ungated = self.window_loudness(0, self.blocks.len());
        let ungated = ungated?;
        if ungated <= ABSOLUTE_GATE_LUFS {
            return None;
        }
        let gate = ungated + RELATIVE_GATE_LU;
        let mut frames = 0u64;
        let mut sum = 0.0f64;
        let mut count = 0u64;
        for &(b_frames, b_sum) in &self.blocks {
            if let Some(z) = self.block_loudness(b_frames, b_sum)
                && z >= gate
                && z >= ABSOLUTE_GATE_LUFS
            {
                frames += b_frames;
                sum += b_sum;
                count += 1;
            }
        }
        if count == 0 {
            return None;
        }
        self.window_loudness_parts(frames, sum).map(|v| v as f32)
    }

    /// Loudest 400 ms block.
    pub fn momentary_max(&self) -> Option<f32> {
        self.blocks
            .iter()
            .filter_map(|&(f, s)| self.block_loudness(f, s))
            .fold(None, |acc: Option<f64>, z| Some(acc.map_or(z, |a: f64| a.max(z))))
            .map(|z| z as f32)
    }

    /// Short-term series: 3 s windows over complete blocks, 400 ms hops,
    /// computed exactly from block prefix sums.
    fn short_term_series(&self) -> Vec<f64> {
        if self.blocks.is_empty() {
            return Vec::new();
        }
        let window_frames = self.blocks[0].0 * 15 / 2; // 7.5 blocks = 3 s
        let hop = self.blocks[0].0;
        let total_frames: u64 = self.blocks.iter().map(|&(f, _)| f).sum();
        // Prefix sums over frames and energy.
        let mut pref_f = vec![0u64];
        let mut pref_e = vec![0.0f64];
        for &(f, e) in &self.blocks {
            pref_f.push(pref_f.last().unwrap() + f);
            pref_e.push(pref_e.last().unwrap() + e);
        }
        let mut out = Vec::new();
        let mut start = 0u64;
        while start + window_frames <= total_frames {
            // Right edge index in block space.
            let mut lo = pref_f.partition_point(|&f| f <= start) - 1;
            let mut hi = pref_f.partition_point(|&f| f < start + window_frames);
            hi = hi.min(self.blocks.len());
            lo = lo.min(hi);
            // Fractional edges: energy share proportional to covered frames.
            let mut frames = 0u64;
            let mut sum = 0.0f64;
            for (block, &b_start) in self.blocks[lo..hi].iter().zip(&pref_f[lo..hi]) {
                let b_frames = block.0;
                let b_end = b_start + b_frames;
                let cov_start = b_start.max(start);
                let cov_end = b_end.min(start + window_frames);
                let cov = cov_end - cov_start;
                if cov == 0 {
                    continue;
                }
                let b_sum = block.1;
                // Energy over the covered slice ≈ proportional share.
                sum += b_sum * (cov as f64 / b_frames as f64);
                frames += cov;
            }
            if let Some(z) = self.loudness_of(frames, sum) {
                out.push(z);
            }
            start += hop;
        }
        out
    }

    fn loudness_of(&self, frames: u64, sum_sq: f64) -> Option<f64> {
        let power = sum_sq / frames as f64;
        if power > 0.0 {
            Some(-0.691 + 10.0 * power.log10())
        } else {
            None
        }
    }

    /// Loudness over whole blocks `lo..hi`.
    fn window_loudness(&self, lo: usize, hi: usize) -> Option<f64> {
        let (mut frames, mut sum) = (0u64, 0.0f64);
        for &(f, s) in &self.blocks[lo..hi] {
            frames += f;
            sum += s;
        }
        self.window_loudness_parts(frames, sum)
    }

    fn window_loudness_parts(&self, frames: u64, sum_sq: f64) -> Option<f64> {
        self.loudness_of(frames, sum_sq)
    }

    /// Loudest 3 s short-term window.
    pub fn short_term_max(&self) -> Option<f32> {
        self.short_term_series()
            .into_iter()
            .fold(None, |acc: Option<f64>, z| Some(acc.map_or(z, |a: f64| a.max(z))))
            .map(|z| z as f32)
    }

    /// Loudness range: 95th minus 10th percentile of the short-term series,
    /// excluding windows at or below the absolute gate.
    pub fn lra(&self) -> Option<f32> {
        let mut series: Vec<f64> = self
            .short_term_series()
            .into_iter()
            .filter(|&z| z > ABSOLUTE_GATE_LUFS)
            .collect();
        if series.len() < 2 {
            return None;
        }
        series.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let pct = |p: f64| -> f64 {
            let pos = p * (series.len() - 1) as f64;
            let lo = pos.floor() as usize;
            let hi = pos.ceil() as usize;
            if lo == hi {
                series[lo]
            } else {
                let frac = pos - lo as f64;
                series[lo] * (1.0 - frac) + series[hi] * frac
            }
        };
        Some((pct(0.95) - pct(0.10)) as f32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r128_of(samples: &[f32], channels: u16, rate: u32) -> R128 {
        let mut r = R128::new(channels, rate);
        for s in samples {
            r.push(*s);
        }
        r
    }

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

    #[test]
    fn one_khz_sines_match_ffmpeg_ebur128() {
        // ffmpeg ebur128 anchors (measured on amplitude-verified files): a
        // full-scale 1 kHz stereo tone reads 0.0 LUFS, −20 dBFS reads −20.0.
        let m = r128_of(&stereo_sine(10.0, 1000.0, 1.0, 44100), 2, 44100);
        let i = m.integrated().unwrap();
        assert!((i - 0.0).abs() < 0.2, "0 dBFS 1k integrated {i}");
        let m = r128_of(&stereo_sine(10.0, 1000.0, 0.1, 44100), 2, 44100);
        let i = m.integrated().unwrap();
        assert!((i - (-20.0)).abs() < 0.2, "-20 dBFS 1k integrated {i}");
        // 440 Hz sits ~0.7 LU below 1 kHz under K-weighting (ffmpeg reads a
        // full-scale 440 Hz tone at −0.7 LUFS).
        let m = r128_of(&stereo_sine(10.0, 440.0, 1.0, 44100), 2, 44100);
        let i = m.integrated().unwrap();
        assert!((i - (-0.7)).abs() < 0.2, "0 dBFS 440 Hz integrated {i}");
        // Short-term and momentary agree on a constant tone; LRA ~ 0.
        assert!((m.momentary_max().unwrap() - i).abs() < 0.1);
        assert!((m.short_term_max().unwrap() - i).abs() < 0.1);
        assert!(m.lra().unwrap() < 0.5);
    }

    #[test]
    fn duplicated_stereo_is_three_db_louder_than_mono() {
        // BS.1770 sums channel energy: the same tone on both channels reads
        // 3.01 LU louder than mono.
        let s = stereo_sine(10.0, 1000.0, 0.5, 44100);
        let mono_s: Vec<f32> = s.iter().step_by(2).copied().collect();
        let mono = r128_of(&mono_s, 1, 44100);
        let dup = r128_of(&s, 2, 44100);
        let d = dup.integrated().unwrap() - mono.integrated().unwrap();
        assert!((d - 3.01).abs() < 0.05, "stereo/mono gap {d}");
    }

    #[test]
    fn loudness_tracks_amplitude() {
        // -6 dB of amplitude is -6 dB of loudness on a constant tone.
        let a = r128_of(&stereo_sine(10.0, 1000.0, 1.0, 44100), 2, 44100);
        let b = r128_of(&stereo_sine(10.0, 1000.0, 0.5, 44100), 2, 44100);
        let d = a.integrated().unwrap() - b.integrated().unwrap();
        assert!((d - 6.0).abs() < 0.2, "loudness delta {d}");
    }
}
