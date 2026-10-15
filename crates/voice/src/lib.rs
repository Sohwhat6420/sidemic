//! TRUE Phone Link voice chain. Mono f32 in [-1, 1] at 48 kHz, processed in 10 ms blocks (480 samples).
//!
//! ```text
//! high-pass 90 Hz -> noise suppression -> presence peak 3 kHz -> speech leveller + expander -> look-ahead limiter
//! ```
//!
//! The same code runs on the laptop (offline benchmarks) and on the phone (JNI from the phone server), so a
//! benchmark result is a result for what ships. The leveller and limiter are a port of the phone server's
//! `Dsp.java`, which stays as the no-native-library fallback.

mod dsp;
mod ns;

pub use dsp::{Biquad, Leveller, Limiter};
pub use ns::{NoiseSuppressor, NsKind};

pub const SAMPLE_RATE: u32 = 48_000;
pub const BLOCK: usize = 480;

#[derive(Clone, Debug)]
pub struct Params {
    pub high_pass_hz: f32,
    pub presence_hz: f32,
    pub presence_db: f32,
    pub ns: NsKind,
    /// DeepFilterNet only: the most it may attenuate noise, in dB (100 = unlimited). Lower keeps more "air".
    pub ns_atten_db: f32,
    /// Bypass the leveller and limiter (to benchmark noise suppression alone).
    pub level: bool,
    pub target_db: f32,
    pub max_gain_db: f32,
    pub min_gain_db: f32,
    pub gate_db: f32,
    pub expand_db: f32,
    pub ceiling_db: f32,
    pub rise_db_per_sec: f32,
    pub fall_db_per_sec: f32,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            high_pass_hz: 90.0,
            presence_hz: 3000.0,
            presence_db: 3.0,
            ns: NsKind::None,
            ns_atten_db: 100.0,
            level: true,
            target_db: -20.0,
            max_gain_db: 24.0,
            min_gain_db: -12.0,
            gate_db: -50.0,
            expand_db: -8.0,
            ceiling_db: -1.5,
            rise_db_per_sec: 6.0,
            fall_db_per_sec: 60.0,
        }
    }
}

impl Params {
    /// The "perfect mic" preset, chosen by the 15 Oct 2026 DNSMOS benchmark on Ethan's quiet-room takes
    /// (voice-recognition source): DeepFilterNet 3 capped at 35 dB beat both uncapped (muffled voice, SIG 2.7)
    /// and 18 dB (noisier, BAK 2.9); a 30-40 dB cap scored OVRL 2.83 / BAK 3.5-3.9 / SIG 3.1-3.3 against the raw
    /// 1.15 / 1.17 / 1.24. The expander is off: with DFN it changed scores by <0.05 and only risks word tails.
    pub fn perfect() -> Self {
        Self { ns: NsKind::Dfn, ns_atten_db: 35.0, expand_db: 0.0, ..Self::default() }
    }
}

pub struct Chain {
    high_pass: Biquad,
    ns: NoiseSuppressor,
    presence: Biquad,
    leveller: Option<Leveller>,
    limiter: Option<Limiter>,
    scratch: Vec<f32>,
}

impl Chain {
    pub fn new(p: &Params) -> Result<Self, String> {
        let fs = SAMPLE_RATE as f32;
        Ok(Self {
            high_pass: Biquad::high_pass(fs, p.high_pass_hz, std::f32::consts::FRAC_1_SQRT_2),
            ns: NoiseSuppressor::new(p.ns, p.ns_atten_db)?,
            presence: Biquad::peaking(fs, p.presence_hz, 1.0, p.presence_db),
            leveller: p.level.then(|| Leveller::new(fs, p)),
            limiter: p.level.then(|| Limiter::new(fs, p.ceiling_db)),
            scratch: vec![0.0; BLOCK],
        })
    }

    /// Algorithmic delay in samples (noise-suppression look-ahead plus the limiter's look-ahead).
    pub fn latency_samples(&self) -> usize {
        self.ns.latency_samples() + self.limiter.as_ref().map_or(0, |l| l.latency_samples())
    }

    /// Processes exactly one 480-sample block in place.
    pub fn process(&mut self, block: &mut [f32]) {
        assert_eq!(block.len(), BLOCK, "blocks are 10 ms (480 samples at 48 kHz)");
        for x in block.iter_mut() {
            *x = self.high_pass.process(*x);
        }
        self.scratch.copy_from_slice(block);
        self.ns.process(&self.scratch, block);
        for x in block.iter_mut() {
            *x = self.presence.process(*x);
        }
        if let Some(leveller) = &mut self.leveller {
            leveller.process(block);
        }
        if let Some(limiter) = &mut self.limiter {
            limiter.process(block);
        }
        for x in block.iter_mut() {
            *x = x.clamp(-1.0, 1.0);
        }
    }
}
