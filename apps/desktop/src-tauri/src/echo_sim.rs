//! Offline echo experiments: where does the phone's speech leveller sit relative to AEC3, and what does that cost?
//!
//! On the real path the phone mic hears the laptop speaker (the echo), the phone's chain (high-pass, presence,
//! leveller, limiter) processes it, and only then the laptop runs AEC3 with the unprocessed speaker audio as its
//! reference. The 27 Oct live runs showed the leveller lifting an echo-only signal by ~15 dB and AEC3 reporting it
//! removed ~1 dB. This simulates exactly that with a real speech clip and prints the level left on the cable for
//! each variant. Run: `cargo test -p tpl-desktop --bin true-phone-link echo_sim -- --ignored --nocapture` (about 20 s).

use super::*;
use std::path::Path;
use tpl_voice::{Biquad, Chain, Leveller, Limiter, NsKind, Params, BLOCK, SAMPLE_RATE};

/// Mono 16-bit PCM WAV -> f32 at `RATE` (linear resample).
fn load(path: &Path) -> Vec<f32> {
    let b = std::fs::read(path).expect("clip");
    let (mut pos, mut rate, mut data) = (12usize, 0u32, &b[0..0]);
    while pos + 8 <= b.len() {
        let id = &b[pos..pos + 4];
        let len = u32::from_le_bytes(b[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body = &b[pos + 8..(pos + 8 + len).min(b.len())];
        if id == b"fmt " {
            rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
        } else if id == b"data" {
            data = body;
        }
        pos += 8 + len + (len & 1);
    }
    let src: Vec<f32> = data.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0).collect();
    let n = (src.len() as u64 * RATE as u64 / rate as u64) as usize;
    (0..n)
        .map(|i| {
            let x = i as f64 * rate as f64 / RATE as f64;
            let (a, f) = (x as usize, (x - x.floor()) as f32);
            src[a.min(src.len() - 1)] * (1.0 - f) + src[(a + 1).min(src.len() - 1)] * f
        })
        .collect()
}

fn rms_db(x: &[f32]) -> f32 {
    let e = x.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / x.len().max(1) as f64;
    (10.0 * (e + 1e-12).log10()) as f32
}

/// RMS level of the second half, in dBFS (the first half is AEC3 learning).
fn tail_db(x: &[f32]) -> f32 {
    rms_db(&x[x.len() / 2..])
}

/// What the phone mic hears: the speaker audio `lag` later, as a direct path and two reflections, scaled so its
/// RMS over the second half of the clip (the half that gets scored) is `level_db`. No talker, no noise: everything
/// on the cable is echo.
fn heard(play: &[f32], lag_ms: usize, level_db: f32) -> Vec<f32> {
    let lag = lag_ms * MS as usize;
    let tap = |n: usize, d: usize, g: f32| if n >= d { g * play[n - d] } else { 0.0 };
    let raw: Vec<f32> = (0..play.len())
        .map(|n| tap(n, lag, 0.4) + tap(n, lag + 9 * MS as usize, 0.15) + tap(n, lag + 23 * MS as usize, -0.1))
        .collect();
    let k = 10f32.powf((level_db - tail_db(&raw)) / 20.0);
    raw.iter().map(|s| s * k).collect()
}

/// Which parts of the phone chain run (high-pass and presence always do; noise suppression is not built in here).
#[derive(Clone, Copy)]
struct Stages {
    leveller: bool,
    limiter: bool,
    max_gain_db: f32,
}

const SHIPPED: Stages = Stages { leveller: true, limiter: true, max_gain_db: 24.0 };

fn params(st: Stages) -> Params {
    Params { ns: NsKind::None, expand_db: 0.0, max_gain_db: st.max_gain_db, ..Params::default() }
}

/// The phone chain with the leveller and the limiter switchable apart (the library only offers both or neither).
/// With `SHIPPED` it must equal `tpl_voice::Chain`; the test asserts that before trusting any other row.
fn chain(st: Stages, x: &[f32]) -> Vec<f32> {
    let (p, fs) = (params(st), SAMPLE_RATE as f32);
    let mut hp = Biquad::high_pass(fs, p.high_pass_hz, std::f32::consts::FRAC_1_SQRT_2);
    let mut presence = Biquad::peaking(fs, p.presence_hz, 1.0, p.presence_db);
    let mut leveller = st.leveller.then(|| Leveller::new(fs, &p));
    let mut limiter = st.limiter.then(|| Limiter::new(fs, p.ceiling_db));
    let mut out = x.to_vec();
    out.resize(x.len().div_ceil(BLOCK) * BLOCK, 0.0);
    for b in out.chunks_exact_mut(BLOCK) {
        for v in b.iter_mut() {
            *v = presence.process(hp.process(*v));
        }
        if let Some(l) = &mut leveller {
            l.process(b);
        }
        if let Some(l) = &mut limiter {
            l.process(b);
        }
        for v in b.iter_mut() {
            *v = v.clamp(-1.0, 1.0);
        }
    }
    out.truncate(x.len());
    out
}

fn library_chain(x: &[f32]) -> Vec<f32> {
    let mut c = Chain::new(&params(SHIPPED)).unwrap();
    let mut out = x.to_vec();
    out.resize(x.len().div_ceil(BLOCK) * BLOCK, 0.0);
    for b in out.chunks_exact_mut(BLOCK) {
        c.process(b);
    }
    out.truncate(x.len());
    out
}

/// AEC3 over the mic with `reference` as the speaker. Like `Canceller::process`, a frame AEC3 cannot produce is passed
/// through unchanged (not left as zeros); the second value counts them.
fn aec(mic: &[f32], reference: &[f32]) -> (Vec<f32>, usize) {
    let format = AudioFormat::ten_ms(RATE, 1);
    let mut pipe = linear::builder(format, format)
        .aec3_config(config())
        .enable_noise_suppression(false)
        .enable_gain_controller2(false)
        .build()
        .unwrap();
    let mut out = Vec::with_capacity(mic.len());
    let (mut o, mut r, mut m) = (vec![0.0f32; FRAME], vec![0.0f32; FRAME], vec![0.0f32; FRAME]);
    let mut passed = 0;
    for k in 0..mic.len() / FRAME {
        m.copy_from_slice(&mic[k * FRAME..(k + 1) * FRAME]);
        r.copy_from_slice(&reference[k * FRAME..(k + 1) * FRAME]);
        pipe.handle_render_frame(&r).unwrap();
        if pipe.process_capture_frame(&m, &mut o).unwrap_or(false) {
            out.extend_from_slice(&o);
        } else {
            passed += 1;
            out.extend_from_slice(&m);
        }
    }
    (out, passed)
}

#[test]
#[ignore = "about 20 s: cargo test -p tpl-desktop --bin true-phone-link echo_sim -- --ignored --nocapture"]
fn echo_sim_where_the_leveller_sits() {
    let clip = std::env::var("TPL_ECHO_CLIP")
        .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../../../../tools/.echo-work/clip.wav").into());
    let mut play = load(Path::new(&clip));
    let secs = std::env::var("TPL_ECHO_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(14usize);
    play.truncate(RATE as usize * secs);
    let n = play.len() / FRAME * FRAME;
    play.truncate(n);
    let lag_ms = 116; // what AEC3 found live on 26 Oct

    // The replica has to be the library's chain, or the table says nothing about the shipped path.
    let probe = heard(&play, lag_ms, -46.0);
    let worst = library_chain(&probe)
        .iter()
        .zip(chain(SHIPPED, &probe))
        .map(|(a, b)| (a - b).abs())
        .fold(0.0f32, f32::max);
    assert!(worst < 1e-5, "the replica differs from tpl_voice::Chain by {worst}");

    eprintln!("clip {:.1} s, lag {lag_ms} ms, speaker audio {:.1} dBFS rms", n as f32 / RATE as f32, rms_db(&play));
    eprintln!("level left on the cable, dBFS rms of the last half (lower is better); 'mic' = echo at the phone mic");
    eprintln!("  mic  | A chain>AEC3 | (A raw) | B AEC3>chain | C no lev/lim | G lev only | H lim only | D lev 12 dB | F lev 0 dB");
    let (mut passed_total, mut all_finite) = (0usize, true);
    for level in [-52.0f32, -46.0, -40.0] {
        let mic = heard(&play, lag_ms, level);
        let (mic, play) = (&mic, &play);
        let st = |leveller, limiter, max_gain_db| Stages { leveller, limiter, max_gain_db };
        let (a_raw, a, b, c, g, h, d, f) = std::thread::scope(|s| {
            let variant = |stages: Stages| {
                s.spawn(move || {
                    let (o, p) = aec(&chain(stages, mic), play);
                    (tail_db(&o), p)
                })
            };
            let a_raw = tail_db(&chain(SHIPPED, mic));
            let a = variant(SHIPPED);
            let b = s.spawn(move || {
                let (o, p) = aec(mic, play);
                (tail_db(&chain(SHIPPED, &o)), p)
            });
            let (c, g, h) = (variant(st(false, false, 24.0)), variant(st(true, false, 24.0)), variant(st(false, true, 24.0)));
            let (d, f) = (variant(st(true, true, 12.0)), variant(st(true, true, 0.0)));
            (a_raw, a.join().unwrap(), b.join().unwrap(), c.join().unwrap(), g.join().unwrap(), h.join().unwrap(), d.join().unwrap(), f.join().unwrap())
        });
        let row = [a, b, c, g, h, d, f];
        passed_total += row.iter().map(|(_, p)| *p).sum::<usize>();
        all_finite &= a_raw.is_finite() && row.iter().all(|(v, _)| v.is_finite());
        eprintln!(
            "  {:4.0} | {:12.1} | {:7.1} | {:12.1} | {:12.1} | {:10.1} | {:10.1} | {:11.1} | {:10.1}",
            level, a.0, a_raw, b.0, c.0, g.0, h.0, d.0, f.0
        );
    }
    eprintln!("frames AEC3 could not produce (passed through unchanged), all variants: {passed_total}");
    // Sanity on the harness, not on the hypothesis.
    assert!(all_finite);
}
