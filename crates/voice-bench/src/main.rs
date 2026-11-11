//! Offline runner for the voice chain: WAV in (48 kHz, 16-bit, mono or stereo) -> chain -> WAV out.
//!
//!   voice-bench IN.wav OUT.wav [strength=off|light|medium|strong] [mix=avg|left|right] [ns=none|rnnoise|dfn]
//!               [atten=100] [level=1|0] [target=-20] [maxgain=18] [mingain=-12] [rise=6] [fall=20] [gate=-50]
//!               [expand=-8] [ceiling=-1.5] [presence=3] [hp=90]
//!
//! `strength=` starts from the app's "Voice cleaning" preset; keys after it override single values.
//! Prints the real-time factor, so the same run doubles as a CPU benchmark.

use std::time::Instant;
use tpl_voice::{Chain, NsKind, Params, Strength, BLOCK};

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        return Err("usage: voice-bench IN.wav OUT.wav [key=value ...]".into());
    }
    let mut p = Params::default();
    let mut mix = "avg".to_string();
    for kv in &args[2..] {
        let (k, v) = kv.split_once('=').ok_or(format!("expected key=value, got {kv}"))?;
        let f = || v.parse::<f32>().map_err(|e| format!("{k}: {e}"));
        match k {
            "strength" => p = Params::for_strength(v.parse::<Strength>()?),
            "mix" => mix = v.to_string(),
            "ns" => p.ns = v.parse::<NsKind>()?,
            "atten" => p.ns_atten_db = f()?,
            "level" => p.level = v != "0",
            "target" => p.target_db = f()?,
            "maxgain" => p.max_gain_db = f()?,
            "gate" => p.gate_db = f()?,
            "expand" => p.expand_db = f()?,
            "ceiling" => p.ceiling_db = f()?,
            "presence" => p.presence_db = f()?,
            "hp" => p.high_pass_hz = f()?,
            "rise" => p.rise_db_per_sec = f()?,
            "fall" => p.fall_db_per_sec = f()?,
            "mingain" => p.min_gain_db = f()?,
            _ => return Err(format!("unknown key {k}")),
        }
    }

    let mut reader = hound::WavReader::open(&args[0]).map_err(|e| e.to_string())?;
    let spec = reader.spec();
    if spec.sample_rate != 48_000 || spec.bits_per_sample != 16 {
        return Err(format!("need 48 kHz 16-bit, got {} Hz {}-bit", spec.sample_rate, spec.bits_per_sample));
    }
    let ch = spec.channels as usize;
    let samples: Vec<i16> = reader.samples::<i16>().collect::<Result<_, _>>().map_err(|e| e.to_string())?;
    let mono: Vec<f32> = samples
        .chunks_exact(ch)
        .map(|f| {
            let v = match (ch, mix.as_str()) {
                (1, _) => f[0] as f32,
                (_, "left") => f[0] as f32,
                (_, "right") => f[1] as f32,
                _ => (f[0] as f32 + f[1] as f32) * 0.5,
            };
            v / 32768.0
        })
        .collect();

    let mut chain = Chain::new(&p)?;
    let latency = chain.latency_samples();
    let mut out = Vec::with_capacity(mono.len() + latency);
    let started = Instant::now();
    let mut block = vec![0f32; BLOCK];
    // Feed the input, then flush the algorithmic delay with silence so the output lines up with the input.
    let total = mono.len() + latency;
    let mut i = 0;
    while i < total {
        for (j, b) in block.iter_mut().enumerate() {
            *b = mono.get(i + j).copied().unwrap_or(0.0);
        }
        chain.process(&mut block);
        out.extend_from_slice(&block);
        i += BLOCK;
    }
    let elapsed = started.elapsed().as_secs_f64();
    let audio_secs = mono.len() as f64 / 48_000.0;

    let spec_out = hound::WavSpec { channels: 1, sample_rate: 48_000, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut w = hound::WavWriter::create(&args[1], spec_out).map_err(|e| e.to_string())?;
    for &x in out.iter().skip(latency).take(mono.len()) {
        w.write_sample((x * 32767.0).round().clamp(-32768.0, 32767.0) as i16).map_err(|e| e.to_string())?;
    }
    w.finalize().map_err(|e| e.to_string())?;
    println!(
        "{} -> {}  ns={:?} latency={:.1} ms  real-time factor {:.3} ({:.1}x faster than real time)",
        args[0],
        args[1],
        p.ns,
        latency as f64 / 48.0,
        elapsed / audio_secs,
        audio_secs / elapsed
    );
    Ok(())
}
