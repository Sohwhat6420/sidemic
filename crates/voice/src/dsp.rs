//! Filters, the speech leveller and the look-ahead limiter (a port of the phone server's Dsp.java).

use crate::Params;

pub fn db_to_lin(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// RBJ audio-EQ-cookbook biquad, transposed direct form II.
#[derive(Clone, Debug)]
pub struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl Biquad {
    fn from_coeffs(b0: f64, b1: f64, b2: f64, a0: f64, a1: f64, a2: f64) -> Self {
        Self {
            b0: (b0 / a0) as f32,
            b1: (b1 / a0) as f32,
            b2: (b2 / a0) as f32,
            a1: (a1 / a0) as f32,
            a2: (a2 / a0) as f32,
            z1: 0.0,
            z2: 0.0,
        }
    }

    pub fn high_pass(fs: f32, f0: f32, q: f32) -> Self {
        let w = 2.0 * std::f64::consts::PI * f0 as f64 / fs as f64;
        let (sin, cos) = w.sin_cos();
        let alpha = sin / (2.0 * q as f64);
        Self::from_coeffs((1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha)
    }

    pub fn peaking(fs: f32, f0: f32, q: f32, gain_db: f32) -> Self {
        let a = 10f64.powf(gain_db as f64 / 40.0);
        let w = 2.0 * std::f64::consts::PI * f0 as f64 / fs as f64;
        let (sin, cos) = w.sin_cos();
        let alpha = sin / (2.0 * q as f64);
        Self::from_coeffs(1.0 + alpha * a, -2.0 * cos, 1.0 - alpha * a, 1.0 + alpha / a, -2.0 * cos, 1.0 - alpha / a)
    }

    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }
}

/// Lifts a distant, quiet voice towards a target level slowly, pulls a loud one down quickly, holds the gain
/// between words (so the gaps are not lifted) and eases a downward expander in once a word has ended.
#[derive(Clone, Debug)]
pub struct Leveller {
    fs: f32,
    target_db: f32,
    max_gain_db: f32,
    min_gain_db: f32,
    gate_db: f32,
    expand_db: f32,
    rise: f32,
    fall: f32,
    env_coef: f32,
    env: f64,
    gain_db: f32,
    expand_gain_db: f32,
    hold_samples: usize,
    hold_remaining: usize,
}

impl Leveller {
    pub fn new(fs: f32, p: &Params) -> Self {
        Self {
            fs,
            target_db: p.target_db,
            max_gain_db: p.max_gain_db,
            min_gain_db: p.min_gain_db,
            gate_db: p.gate_db,
            expand_db: p.expand_db,
            rise: p.rise_db_per_sec,
            fall: p.fall_db_per_sec,
            env_coef: (-1.0 / (0.050 * fs as f64)).exp() as f32, // ~50 ms RMS window
            env: 1e-9,
            gain_db: 0.0,
            expand_gain_db: 0.0,
            hold_samples: (0.200 * fs) as usize,
            hold_remaining: 0,
        }
    }

    pub fn process(&mut self, buf: &mut [f32]) {
        let n = buf.len();
        let c = self.env_coef as f64;
        for &x in buf.iter() {
            self.env = c * self.env + (1.0 - c) * (x as f64) * (x as f64);
        }
        let level_db = (10.0 * (self.env + 1e-12).log10()) as f32;
        let block_sec = n as f32 / self.fs;
        let speech = level_db > self.gate_db;

        let mut target = self.gain_db;
        if speech {
            target = (self.target_db - level_db).clamp(self.min_gain_db, self.max_gain_db);
            self.hold_remaining = self.hold_samples;
        }
        let start_gain = self.gain_db;
        self.gain_db = if target > self.gain_db {
            target.min(self.gain_db + self.rise * block_sec)
        } else {
            target.max(self.gain_db - self.fall * block_sec)
        };

        let start_expand = self.expand_gain_db;
        self.expand_gain_db = if speech || self.hold_remaining > 0 {
            (self.expand_gain_db + 200.0 * block_sec).min(0.0) // opens in ~40 ms
        } else {
            (self.expand_gain_db - 40.0 * block_sec).max(self.expand_db) // closes in ~200 ms
        };
        self.hold_remaining = self.hold_remaining.saturating_sub(n);

        for (i, x) in buf.iter_mut().enumerate() {
            let t = (i + 1) as f32 / n as f32;
            let g = start_gain + (self.gain_db - start_gain) * t + start_expand + (self.expand_gain_db - start_expand) * t;
            *x *= db_to_lin(g);
        }
    }
}

/// Look-ahead limiter: required gain -> sliding minimum over the look-ahead -> release -> box average over the
/// look-ahead, applied to audio delayed by (look-ahead - 1). The average reaches the required gain exactly when
/// the peak leaves the delay line and ramps there smoothly, so there is no gain step and no inter-sample
/// overshoot (the old step limiter measured +1.4 dBTP on 15 Oct 2026).
#[derive(Clone, Debug)]
pub struct Limiter {
    look: usize,
    required: Vec<f32>,
    smoothed: Vec<f32>,
    smoothed_sum: f64,
    pos: usize,
    delay: Vec<f32>,
    delay_pos: usize,
    ceiling: f32,
    release: f32,
    gain: f32,
}

impl Limiter {
    pub fn new(fs: f32, ceiling_db: f32) -> Self {
        let look = (0.002 * fs) as usize; // 96 samples at 48 kHz
        Self {
            look,
            required: vec![1.0; look],
            smoothed: vec![1.0; look],
            smoothed_sum: look as f64,
            pos: 0,
            delay: vec![0.0; look - 1],
            delay_pos: 0,
            ceiling: db_to_lin(ceiling_db),
            release: 1.0 - (-1.0 / (0.050 * fs as f64)).exp() as f32,
            gain: 1.0,
        }
    }

    pub fn latency_samples(&self) -> usize {
        self.look - 1
    }

    pub fn process(&mut self, buf: &mut [f32]) {
        for x in buf.iter_mut() {
            let a = x.abs();
            self.required[self.pos] = if a > self.ceiling { self.ceiling / a } else { 1.0 };
            let hold = self.required.iter().copied().fold(1.0f32, f32::min);
            self.gain = if hold < self.gain { hold } else { self.gain + (hold - self.gain) * self.release };
            self.smoothed_sum += (self.gain - self.smoothed[self.pos]) as f64;
            self.smoothed[self.pos] = self.gain;
            self.pos = (self.pos + 1) % self.look;
            let g = (self.smoothed_sum / self.look as f64) as f32;

            let out = self.delay[self.delay_pos];
            self.delay[self.delay_pos] = *x;
            self.delay_pos = (self.delay_pos + 1) % self.delay.len();
            *x = out * g;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limiter_never_exceeds_ceiling() {
        let mut lim = Limiter::new(48_000.0, -1.5);
        let ceiling = db_to_lin(-1.5);
        let mut buf: Vec<f32> = (0..48_000).map(|i| 2.0 * ((i as f32) * 0.05).sin() * if i % 7000 < 300 { 1.0 } else { 0.2 }).collect();
        lim.process(&mut buf);
        let peak = buf.iter().fold(0f32, |m, x| m.max(x.abs()));
        assert!(peak <= ceiling * 1.0001, "peak {peak} > ceiling {ceiling}");
    }

    #[test]
    fn limiter_is_transparent_below_ceiling() {
        let mut lim = Limiter::new(48_000.0, -1.5);
        let input: Vec<f32> = (0..4800).map(|i| 0.3 * ((i as f32) * 0.01).sin()).collect();
        let mut buf = input.clone();
        lim.process(&mut buf);
        let d = lim.latency_samples();
        for i in d..buf.len() {
            assert!((buf[i] - input[i - d]).abs() < 1e-6);
        }
    }
}
