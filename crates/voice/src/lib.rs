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
    /// The leveller's fall rate and gain ceiling were eased on 11 Nov 2026 (from 60 dB/s and +24 dB): at 60 dB/s
    /// and a 50 ms level window it pulled loud syllables down inside words, so the gain swung 6-8 dB within speech
    /// (p95 slew 17-31 dB/s on three real takes). At 20 dB/s and +18 dB the swing is 5-6 dB, the slew 6-17 dB/s,
    /// and the loudness stays within 2 dB.
    fn default() -> Self {
        Self {
            high_pass_hz: 90.0,
            presence_hz: 3000.0,
            presence_db: 3.0,
            ns: NsKind::None,
            ns_atten_db: 100.0,
            level: true,
            target_db: -20.0,
            max_gain_db: 18.0,
            min_gain_db: -12.0,
            gate_db: -50.0,
            expand_db: -8.0,
            ceiling_db: -1.5,
            rise_db_per_sec: 6.0,
            fall_db_per_sec: 20.0,
        }
    }
}

impl Params {
    /// The chain for a noise-suppression strength (the user's "Voice cleaning" setting). The expander is off with
    /// any suppressor: with DFN it changed DNSMOS by <0.05 and only risks word tails.
    pub fn for_strength(s: Strength) -> Self {
        Self { ns: s.ns(), ns_atten_db: s.atten_db(), expand_db: 0.0, ..Self::default() }
    }

    /// The 15 Oct 2026 pick: DeepFilterNet 3 capped at 35 dB, now `Strength::Strong`. It won on DNSMOS, which
    /// scores at 16 kHz and so never saw what it cost above 8 kHz (see `Strength`).
    pub fn perfect() -> Self {
        Self::for_strength(Strength::Strong)
    }
}

/// How hard noise suppression works: the user's "Voice cleaning" setting.
///
/// DeepFilterNet's attenuation cap *is* a dry mix: libDF blends `10^(-cap/20)` of the unprocessed spectrum back
/// into its output (12 dB = 25 % dry, 18 dB = 13 %, 35 dB = 2 %), phase-aligned in the STFT domain, so a lighter
/// setting keeps the voice's own highs and word tails instead of re-synthesising them.
///
/// Measured 11 Nov 2026 on three raw takes (noise floor subtracted, relative to 1-4 kHz): the 35 dB cap took the
/// voice's 8-16 kHz down by 19-28 dB and the 30-200 ms after each word by 12-18 dB, and left 51-56 dB between
/// speech and the floor between words (raw 16-20 dB): a dull voice floating in dead silence. 18 dB (with the
/// eased leveller) keeps 8-16 kHz within 12 dB and the tails within 5-10 dB, at DNSMOS SIG +0.2-0.3 and OVRL
/// within 0.2 of 35 dB.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Strength {
    /// No suppression: high-pass, presence, leveller and limiter only.
    Off,
    /// DeepFilterNet 3 capped at 12 dB: the most natural, for a quiet room.
    Light,
    /// DeepFilterNet 3 capped at 18 dB: the default.
    #[default]
    Medium,
    /// DeepFilterNet 3 capped at 35 dB: the 15 Oct 2026 setting, for a noisy room.
    Strong,
}

impl Strength {
    pub const ALL: [Strength; 4] = [Strength::Off, Strength::Light, Strength::Medium, Strength::Strong];

    pub fn ns(self) -> NsKind {
        match self {
            Strength::Off => NsKind::None,
            _ => NsKind::Dfn,
        }
    }

    /// DeepFilterNet's attenuation cap in dB (100 = unlimited; unused when `ns()` is `None`).
    pub fn atten_db(self) -> f32 {
        match self {
            Strength::Off => 100.0,
            Strength::Light => 12.0,
            Strength::Medium => 18.0,
            Strength::Strong => 35.0,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Strength::Off => "off",
            Strength::Light => "light",
            Strength::Medium => "medium",
            Strength::Strong => "strong",
        }
    }
}

impl std::str::FromStr for Strength {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        Strength::ALL
            .into_iter()
            .find(|k| k.as_str() == s)
            .ok_or_else(|| format!("unknown strength '{s}' (off|light|medium|strong)"))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strengths_round_trip_and_get_lighter_in_order() {
        for s in Strength::ALL {
            assert_eq!(s.as_str().parse::<Strength>(), Ok(s));
        }
        assert!("loud".parse::<Strength>().is_err());
        assert!("Medium".parse::<Strength>().is_err(), "settings are stored lower-case");
        assert_eq!(Strength::default(), Strength::Medium);
        assert_eq!(Strength::Off.ns(), NsKind::None);
        for s in [Strength::Light, Strength::Medium, Strength::Strong] {
            assert_eq!(s.ns(), NsKind::Dfn);
        }
        assert!(Strength::Light.atten_db() < Strength::Medium.atten_db());
        assert!(Strength::Medium.atten_db() < Strength::Strong.atten_db());
        assert_eq!(Strength::Strong.atten_db(), 35.0, "Strong is the 15 Oct 2026 setting");
    }

    #[test]
    fn a_strength_sets_only_the_suppressor_and_the_expander() {
        let p = Params::for_strength(Strength::Medium);
        let d = Params::default();
        assert_eq!((p.ns, p.ns_atten_db, p.expand_db), (NsKind::Dfn, 18.0, 0.0));
        assert_eq!((p.max_gain_db, p.fall_db_per_sec, p.target_db), (d.max_gain_db, d.fall_db_per_sec, d.target_db));
        assert_eq!(Params::perfect().ns_atten_db, 35.0);
    }

    #[test]
    fn the_leveller_is_the_eased_one() {
        let d = Params::default();
        assert_eq!((d.rise_db_per_sec, d.fall_db_per_sec, d.max_gain_db), (6.0, 20.0, 18.0));
    }

    /// Steady noise is what DeepFilterNet removes completely, so on it the output level is set by the cap alone:
    /// each strength must take noise down by about its cap, and in order (11 Nov 2026: 12.0, 18.0 and 35.0 dB).
    #[cfg(feature = "dfn")]
    #[test]
    #[cfg_attr(debug_assertions, ignore = "tract only builds the DFN graph in release: cargo test --release -p tpl-voice")]
    fn each_strength_takes_steady_noise_down_by_about_its_cap() {
        let mut seed = 0x2545_f491_u32;
        let noise: Vec<f32> = (0..BLOCK * 200)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                (seed as f32 / u32::MAX as f32 - 0.5) * 0.06 // white noise around -35 dBFS
            })
            .collect();
        let level = |s: Strength| {
            let mut chain = Chain::new(&Params { level: false, ..Params::for_strength(s) }).unwrap();
            let mut out = Vec::new();
            for block in noise.chunks(BLOCK) {
                let mut b = block.to_vec();
                chain.process(&mut b);
                out.extend(b);
            }
            let tail = &out[out.len() / 2..]; // after DFN has settled
            10.0 * (tail.iter().map(|x| (x * x) as f64).sum::<f64>() / tail.len() as f64).log10()
        };
        let off = level(Strength::Off);
        let cut: Vec<f64> = [Strength::Light, Strength::Medium, Strength::Strong].iter().map(|&s| off - level(s)).collect();
        eprintln!("noise taken down by: light {:.1} dB, medium {:.1} dB, strong {:.1} dB", cut[0], cut[1], cut[2]);
        assert!(cut[0] > 6.0 && cut[0] < 12.0 + 2.0, "light {:.1}", cut[0]);
        assert!(cut[1] > cut[0] + 3.0 && cut[1] < 18.0 + 2.0, "medium {:.1}", cut[1]);
        assert!(cut[2] > cut[1] + 6.0, "strong {:.1}", cut[2]);
    }
}
