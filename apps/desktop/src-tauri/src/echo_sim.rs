//! Offline echo experiments: where does the phone's speech leveller sit relative to AEC3, and what does that cost?
//!
//! On the real path the phone mic hears the laptop speaker (the echo), the phone's chain (high-pass, presence,
//! leveller, limiter) processes it, and only then the laptop runs AEC3 with the unprocessed speaker audio as its
//! reference. The 27 Oct live runs showed the leveller lifting an echo-only signal by ~15 dB and AEC3 reporting it
//! removed ~1 dB. This simulates exactly that with a real speech clip and prints the level left on the cable for
//! each variant. Run: `cargo test -p tpl-desktop echo_sim -- --ignored --nocapture` (a few minutes in debug).

use super::*;
use std::path::Path;
use tpl_voice::{Chain, NsKind, Params, BLOCK};

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

/// What the phone mic hears: the speaker audio `lag` later, as a direct path and two reflections, scaled so its
/// RMS over the whole clip is `level_db`. No talker, no noise: everything on the cable is echo.
fn heard(play: &[f32], lag_ms: usize, level_db: f32) -> Vec<f32> {
    let lag = lag_ms * MS as usize;
    let tap = |n: usize, d: usize, g: f32| if n >= d { g * play[n - d] } else { 0.0 };
    let raw: Vec<f32> = (0..play.len())
        .map(|n| tap(n, lag, 0.4) + tap(n, lag + 9 * MS as usize, 0.15) + tap(n, lag + 23 * MS as usize, -0.1))
        .collect();
    let k = 10f32.powf((level_db - rms_db(&raw)) / 20.0);
    raw.iter().map(|s| s * k).collect()
}

fn chain(p: &Params, x: &[f32]) -> Vec<f32> {
    let mut c = Chain::new(p).unwrap();
    let mut out = x.to_vec();
    out.resize(x.len().div_ceil(BLOCK) * BLOCK, 0.0);
    for b in out.chunks_exact_mut(BLOCK) {
        c.process(b);
    }
    out.truncate(x.len());
    out
}

fn aec(mic: &[f32], reference: &[f32]) -> Vec<f32> {
    let format = AudioFormat::ten_ms(RATE, 1);
    let mut pipe = linear::builder(format, format)
        .aec3_config(config())
        .enable_noise_suppression(false)
        .enable_gain_controller2(false)
        .build()
        .unwrap();
    let mut out = Vec::with_capacity(mic.len());
    let (mut o, mut r, mut m) = (vec![0.0f32; FRAME], vec![0.0f32; FRAME], vec![0.0f32; FRAME]);
    for k in 0..mic.len() / FRAME {
        m.copy_from_slice(&mic[k * FRAME..(k + 1) * FRAME]);
        r.copy_from_slice(&reference[k * FRAME..(k + 1) * FRAME]);
        pipe.handle_render_frame(&r).unwrap();
        pipe.process_capture_frame(&m, &mut o).unwrap();
        out.extend_from_slice(&o);
    }
    out
}

/// The phone chain as shipped (no noise suppression here: DeepFilterNet is not built into this test).
fn shipped() -> Params {
    Params { ns: NsKind::None, expand_db: 0.0, ..Params::default() }
}

/// RMS level of the second half, in dBFS (the first half is AEC3 learning).
fn tail_db(x: &[f32]) -> f32 {
    rms_db(&x[x.len() / 2..])
}

#[test]
#[ignore = "minutes in debug: cargo test -p tpl-desktop echo_sim -- --ignored --nocapture"]
fn echo_sim_where_the_leveller_sits() {
    let clip = std::env::var("TPL_ECHO_CLIP")
        .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../../../../tools/.echo-work/clip.wav").into());
    let mut play = load(Path::new(&clip));
    let secs = std::env::var("TPL_ECHO_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(14usize);
    play.truncate(RATE as usize * secs);
    let n = play.len() / FRAME * FRAME;
    play.truncate(n);
    let lag_ms = 116; // what AEC3 found live on 26 Oct
    eprintln!("clip {:.1} s, lag {lag_ms} ms, speaker audio {:.1} dBFS rms", n as f32 / RATE as f32, rms_db(&play));
    eprintln!("level left on the cable, dBFS rms of the last half (lower is better); 'mic' = echo at the phone mic");
    eprintln!("  mic    | A chain>AEC3 | A, no AEC | B AEC3>chain | C no leveller>AEC3 | D maxgain 12 | E gate -40 | F maxgain 0");
    let mut rows = Vec::new();
    for level in [-52.0f32, -46.0, -40.0] {
        let mic = heard(&play, lag_ms, level);
        let (mic, play) = (&mic, &play);
        let r = std::thread::scope(|s| {
            let a = s.spawn(move || { let c = chain(&shipped(), mic); (tail_db(&c), tail_db(&aec(&c, play))) });
            let b = s.spawn(move || tail_db(&chain(&shipped(), &aec(mic, play))));
            let c = s.spawn(move || tail_db(&aec(&chain(&Params { level: false, ..shipped() }, mic), play)));
            let d = s.spawn(move || tail_db(&aec(&chain(&Params { max_gain_db: 12.0, ..shipped() }, mic), play)));
            let e = s.spawn(move || tail_db(&aec(&chain(&Params { gate_db: -40.0, ..shipped() }, mic), play)));
            let f = s.spawn(move || tail_db(&aec(&chain(&Params { max_gain_db: 0.0, ..shipped() }, mic), play)));
            (a.join().unwrap(), b.join().unwrap(), c.join().unwrap(), d.join().unwrap(), e.join().unwrap(), f.join().unwrap())
        });
        eprintln!(
            "  {:5.0}  | {:6.1}  (raw {:6.1}) | {:12.1} | {:18.1} | {:12.1} | {:10.1} | {:11.1}",
            level, (r.0).1, (r.0).0, r.1, r.2, r.3, r.4, r.5
        );
        rows.push((level, r));
    }
    // Sanity on the harness, not on the hypothesis: the echo must come out of the phone chain at all.
    assert!(rows.iter().all(|(_, r)| (r.0).0.is_finite() && (r.0).1.is_finite()));
}
