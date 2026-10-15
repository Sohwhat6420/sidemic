//! Noise suppression back ends. Each takes and returns one 480-sample block at 48 kHz.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NsKind {
    None,
    /// RNNoise (nnnoiseless, BSD-3-Clause): tiny, ~10 ms.
    Rnnoise,
    /// DeepFilterNet 3 (MIT/Apache-2.0): far stronger, heavier, ~40 ms look-ahead.
    Dfn,
}

impl std::str::FromStr for NsKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "none" => Ok(Self::None),
            "rnnoise" => Ok(Self::Rnnoise),
            "dfn" => Ok(Self::Dfn),
            _ => Err(format!("unknown noise suppressor '{s}' (none|rnnoise|dfn)")),
        }
    }
}

pub enum NoiseSuppressor {
    None,
    #[cfg(feature = "rnnoise")]
    Rnnoise(Box<nnnoiseless::DenoiseState<'static>>, Vec<f32>, Vec<f32>),
    #[cfg(feature = "dfn")]
    Dfn(Box<df::tract::DfTract>),
}

impl NoiseSuppressor {
    pub fn new(kind: NsKind, atten_db: f32) -> Result<Self, String> {
        match kind {
            NsKind::None => Ok(Self::None),
            #[cfg(feature = "rnnoise")]
            NsKind::Rnnoise => Ok(Self::Rnnoise(nnnoiseless::DenoiseState::new(), vec![0.0; 480], vec![0.0; 480])),
            #[cfg(feature = "dfn")]
            NsKind::Dfn => {
                use df::tract::{DfParams, DfTract, RuntimeParams};
                let rp = RuntimeParams::default_with_ch(1).with_atten_lim(atten_db);
                let model = DfTract::new(DfParams::default(), &rp).map_err(|e| e.to_string())?;
                if model.hop_size != 480 || model.sr != 48_000 {
                    return Err(format!("unexpected DFN geometry: hop {} sr {}", model.hop_size, model.sr));
                }
                Ok(Self::Dfn(Box::new(model)))
            }
            #[allow(unreachable_patterns)]
            other => Err(format!("{other:?} was not compiled in")),
        }
    }

    pub fn latency_samples(&self) -> usize {
        match self {
            Self::None => 0,
            #[cfg(feature = "rnnoise")]
            Self::Rnnoise(..) => 480,
            #[cfg(feature = "dfn")]
            Self::Dfn(m) => m.fft_size - m.hop_size + m.lookahead * m.hop_size,
        }
    }

    pub fn process(&mut self, input: &[f32], output: &mut [f32]) {
        match self {
            Self::None => output.copy_from_slice(input),
            #[cfg(feature = "rnnoise")]
            Self::Rnnoise(state, inb, outb) => {
                // nnnoiseless works in the i16 range.
                for (d, s) in inb.iter_mut().zip(input) {
                    *d = s * 32768.0;
                }
                state.process_frame(outb, inb);
                for (d, s) in output.iter_mut().zip(outb.iter()) {
                    *d = s / 32768.0;
                }
            }
            #[cfg(feature = "dfn")]
            Self::Dfn(m) => {
                let noisy = ndarray::ArrayView2::from_shape((1, input.len()), input).expect("shape");
                let enh = ndarray::ArrayViewMut2::from_shape((1, output.len()), output).expect("shape");
                if m.process(noisy, enh).is_err() {
                    // Never go silent on a model hiccup: pass the block through.
                    output.copy_from_slice(input);
                }
            }
        }
    }
}
