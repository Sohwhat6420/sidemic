//! Echo cancelling for the phone mic. The phone usually sits by the laptop's speakers, so it hears the call and the
//! game and sends them back: friends heard themselves (21 Oct 2026). WebRTC's AEC3 (the `aec3` crate) takes out
//! whatever the default speaker played, using a WASAPI loopback recording of it as the reference.
//!
//! It runs on the laptop, on the phone audio as it arrives and before the jitter queue, because the queue skips
//! and pauses samples and the canceller needs one unbroken timeline. Measured 21 Oct on a spoken clip through the
//! Pebble: the echo was −25 dB (as loud as speech); AEC3 on the phone's own timeline took it to −41…−76 dB, but
//! after the queue's skips only to −36 dB. It costs ~3 % of one laptop core.
//!
//! Lining the two up: both streams are counted in 48 kHz samples on the laptop's performance counter. Phone sample
//! `k` is paired with the speaker sample at `anchor + k`, where the anchor is the earliest any phone audio has
//! arrived (arrival − samples so far, the minimum over 20 s) less `LEAD`. That pairs each phone sample with
//! speaker audio from a little before the phone could have heard it, which is the order AEC3 needs; AEC3 itself
//! finds the remaining delay (phone latency + speaker latency, ~50–150 ms). The phone's and the laptop's clocks
//! drift apart slowly, so the anchor follows the minimum when it moves by more than `REANCHOR`.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use aec3::api::config::EchoCanceller3Config;
use aec3::nodes::audio::AudioFormat;
use aec3::pipelines::linear::{self, LinearPipeline};

use crate::log;
use crate::wasapi::{self, Capture, RATE};

const MS: i64 = RATE as i64 / 1000;
const FRAME: usize = RATE as usize / 100; // AEC3 works in 10 ms frames
const KEEP: usize = RATE as usize * 3; // speaker history kept
const LEAD: i64 = 20 * MS;
const REANCHOR: i64 = 25 * MS;
/// With no loopback packet for this long, the speaker is taken to be silent up to this long ago.
const IDLE: i64 = 30 * MS;
/// A frame waits at most this long for the speaker recording to catch up, then goes through as it is.
const MAX_WAIT: Duration = Duration::from_millis(60);

fn now_idx() -> i64 {
    idx(wasapi::qpc_100ns())
}

fn idx(qpc_100ns: u64) -> i64 {
    (qpc_100ns as i128 * RATE as i128 / 10_000_000) as i64
}

/// What the default speaker played, by sample index on the performance-counter clock.
#[derive(Default)]
struct Timeline {
    start: i64,
    buf: VecDeque<f32>,
    last_packet: i64,
}

impl Timeline {
    fn end(&self) -> i64 {
        self.start + self.buf.len() as i64
    }

    fn write(&mut self, at: i64, s: &[f32]) {
        self.last_packet = now_idx();
        if self.buf.is_empty() || at > self.end() + KEEP as i64 {
            self.buf.clear();
            self.start = at;
        }
        let end = self.end();
        let skip = if at < end { (end - at) as usize } else { 0 };
        if at > end {
            self.buf.extend(std::iter::repeat(0.0).take((at - end) as usize));
        }
        if skip < s.len() {
            self.buf.extend(&s[skip..]);
        }
        if self.buf.len() > KEEP {
            let drop = self.buf.len() - KEEP;
            self.buf.drain(..drop);
            self.start += drop as i64;
        }
    }

    /// How far the speaker is known: what has been recorded, or, while nothing plays, up to `IDLE` ago.
    fn known(&self, now: i64) -> i64 {
        if now - self.last_packet > IDLE {
            self.end().max(now - IDLE)
        } else {
            self.end()
        }
    }

    fn read(&self, at: i64, out: &mut [f32]) {
        for (i, o) in out.iter_mut().enumerate() {
            let j = at + i as i64 - self.start;
            *o = if j >= 0 && (j as usize) < self.buf.len() { self.buf[j as usize] } else { 0.0 };
        }
    }
}

/// AEC3 tuned to cut his voice less while both talk. Through a small speaker and a room, the echo only correlates
/// 0.66–0.85 with what was played (no clock drift: under 0.2 ms over 10 s), so the linear filter removes only
/// ~10 dB and the suppressor does the rest, which is what cuts a voice talking over the echo. Swept offline on the
/// POCO's recording of the Pebble with a second voice mixed in (21 Oct 2026); against the defaults this has the
/// worst half second of voice at −22 dB instead of −38 and 3 dropouts instead of 5, for echo left at −38…−42 dB
/// instead of −48…−63 (from −25 with none). Longer filters and higher ERLE limits made no difference. Live, his voice
/// stayed at −25…−30 dBFS under the clip (defaults: −56…−77) and the echo alone came through at −35…−45 (defaults:
/// −60…−75); he chose his voice over the echo, with Discord's own echo cancellation on top.
fn config() -> EchoCanceller3Config {
    let mut c = EchoCanceller3Config::default();
    let dn = &mut c.suppressor.dominant_nearend_detection;
    // Call it "he is talking" sooner and hold that longer.
    dn.enr_threshold = 2.0;
    dn.snr_threshold = 5.0;
    dn.trigger_threshold = 2;
    dn.hold_duration = 100;
    // While he is talking, suppress a quarter as readily.
    for m in [&mut c.suppressor.nearend_tuning.mask_lf, &mut c.suppressor.nearend_tuning.mask_hf] {
        m.enr_transparent *= 4.0;
        m.enr_suppress *= 4.0;
    }
    c
}

/// Records the default speaker for as long as it lives.
pub struct SpeakerTap {
    timeline: Arc<Mutex<Timeline>>,
    stop: Arc<AtomicBool>,
}

impl SpeakerTap {
    pub fn start() -> SpeakerTap {
        let timeline = Arc::new(Mutex::new(Timeline::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (t, s) = (timeline.clone(), stop.clone());
        thread::Builder::new()
            .name("tpl-speaker".into())
            .spawn(move || {
                wasapi::com_init();
                let mut tap: Option<(Capture, String)> = None;
                let mut checked = Instant::now();
                while !s.load(Ordering::Relaxed) {
                    // The default speaker can change (Pebble unplugged): follow it every 2 s.
                    if checked.elapsed() > Duration::from_secs(2) {
                        checked = Instant::now();
                        let now = wasapi::default_render().map(|(_, n)| n).ok();
                        if tap.as_ref().map(|(_, n)| n) != now.as_ref() {
                            tap = None;
                        }
                    }
                    if tap.is_none() {
                        tap = wasapi::default_render()
                            .and_then(|(d, n)| Capture::open_loopback(&d).map(|c| (c, n)))
                            .ok();
                        if tap.is_none() {
                            thread::sleep(Duration::from_secs(1));
                            continue;
                        }
                    }
                    let ok = tap
                        .as_ref()
                        .unwrap()
                        .0
                        .read_timed(|qpc, s| t.lock().unwrap().write(idx(qpc), s))
                        .is_ok();
                    if !ok {
                        tap = None;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
            })
            .expect("speaker thread");
        SpeakerTap { timeline, stop }
    }
}

impl Drop for SpeakerTap {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Takes the speaker out of the phone's audio, as it arrives. Lives on the phone stream's reader thread (AEC3's
/// pipeline isn't `Send`, so it is made there too).
pub struct Canceller {
    pipe: LinearPipeline,
    tap: SpeakerTap,
    anchor: Option<i64>,
    /// Phone samples handed to AEC3 so far.
    k: i64,
    pending: VecDeque<f32>,
    waiting_since: Option<Instant>,
    /// (when, arrival index − phone samples so far) over the last 20 s.
    lows: VecDeque<(Instant, i64)>,
    render: Vec<f32>,
    frame: Vec<f32>,
    out: Vec<f32>,
    reanchors: u32,
    metrics: Option<aec3::api::control::Metrics>,
    last_report: Instant,
}

impl Canceller {
    pub fn new() -> Option<Canceller> {
        let format = AudioFormat::ten_ms(RATE, 1);
        // The phone already removes noise and levels the voice: echo cancelling only.
        let pipe = linear::builder(format, format)
            .aec3_config(config())
            .enable_noise_suppression(false)
            .enable_gain_controller2(false)
            .export_metrics(true)
            .build();
        match pipe {
            Ok(pipe) => Some(Canceller {
                pipe,
                tap: SpeakerTap::start(),
                anchor: None,
                k: 0,
                pending: VecDeque::new(),
                waiting_since: None,
                lows: VecDeque::new(),
                render: vec![0.0; FRAME],
                frame: vec![0.0; FRAME],
                out: vec![0.0; FRAME],
                reanchors: 0,
                metrics: None,
                last_report: Instant::now(),
            }),
            Err(e) => {
                log::line(format!("echo cancelling unavailable: {e}"));
                None
            }
        }
    }

    /// Phone samples that arrived just now in; echo-cancelled samples out (whole 10 ms frames, so a few ms may
    /// wait for the next call).
    pub fn process(&mut self, input: &[f32], output: &mut Vec<f32>) {
        let now = now_idx();
        let t = Instant::now();
        self.pending.extend(input);
        let offset = now - (self.k + self.pending.len() as i64);
        self.lows.push_back((t, offset));
        while self.lows.front().is_some_and(|(w, _)| t - *w > Duration::from_secs(20)) {
            self.lows.pop_front();
        }
        let base = self.lows.iter().map(|(_, o)| *o).min().unwrap_or(offset) - LEAD;
        match self.anchor {
            None => self.anchor = Some(base),
            Some(a) if (a - base).abs() > REANCHOR => {
                self.anchor = Some(base);
                self.reanchors += 1;
                log::line(format!("echo cancelling: realigned by {} ms", (base - a) / MS));
            }
            _ => {}
        }
        let anchor = self.anchor.unwrap();

        // The extra stream delay (Settings, 0–500 ms) pairs each phone sample with speaker audio from that much
        // earlier, for a slow speaker path (Bluetooth speakers add 100–250 ms) that AEC3's own search misses.
        let extra = DELAY_MS.load(Ordering::Relaxed).min(MAX_DELAY_MS) as i64 * MS;
        while self.pending.len() >= FRAME {
            let at = anchor + self.k - extra;
            let known = self.tap.timeline.lock().unwrap().known(now);
            if at + FRAME as i64 > known {
                // The speaker recording is a few ms behind: wait for it, but never hold the mic back for long.
                let since = *self.waiting_since.get_or_insert(t);
                if since.elapsed() < MAX_WAIT {
                    break;
                }
            }
            self.waiting_since = None;
            self.tap.timeline.lock().unwrap().read(at, &mut self.render);
            for (f, s) in self.frame.iter_mut().zip(self.pending.drain(..FRAME)) {
                *f = s;
            }
            let ok = self.pipe.handle_render_frame(&self.render).is_ok()
                && self.pipe.process_capture_frame(&self.frame, &mut self.out).unwrap_or(false);
            output.extend_from_slice(if ok { &self.out } else { &self.frame });
            self.k += FRAME as i64;
        }

        // Drained every time so the metrics queue can't fill and stall the pipeline (only `metrics` is kept).
        while let Ok(Some(m)) = self.pipe.try_pull_metrics() {
            self.metrics = Some(*m.payload());
        }
        if self.last_report.elapsed() > Duration::from_secs(60) {
            self.last_report = Instant::now();
            if let Some(m) = self.metrics {
                log::line(format!(
                    "echo cancelling: delay {} ms, echo return loss {:.0} dB, removed {:.0} dB, realigned {}×",
                    m.delay_ms, m.echo_return_loss, m.echo_return_loss_enhancement, self.reanchors
                ));
            }
        }
    }
}

/// Extra delay between the speaker reference and the phone audio, in ms (see `Canceller::process`).
pub static DELAY_MS: AtomicU32 = AtomicU32::new(0);
pub const MAX_DELAY_MS: u32 = 500;

/// What to do with the phone's audio while echo cancelling is down (his choice, in Settings).
pub static FALLBACK_SILENCE: AtomicBool = AtomicBool::new(false);

/// Anything that can clean a stretch of phone audio; the real one is `Canceller`.
pub trait Clean {
    fn clean(&mut self, input: &[f32], output: &mut Vec<f32>);
}

impl Clean for Canceller {
    fn clean(&mut self, input: &[f32], output: &mut Vec<f32>) {
        self.process(input, output);
    }
}

/// Keeps echo cancelling alive. If it panics or won't build, the phone's audio carries on without it (raw, or
/// silence, as chosen) while it is rebuilt after a short, growing wait: 0.5 s, 1 s, 2 s, 4 s, then every 8 s. A
/// healthy half minute starts the waits over. This is the one place a fault in AEC3 or the speaker recording can
/// land, so the mic never stalls or takes the app down with it.
pub struct Supervised<C: Clean> {
    make: Box<dyn FnMut() -> Option<C> + Send>,
    inner: Option<C>,
    failures: u32,
    retry_at: Instant,
    healthy_since: Instant,
}

const BACKOFF_FIRST: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(8);
const HEALTHY: Duration = Duration::from_secs(30);

impl<C: Clean> Supervised<C> {
    pub fn new(make: impl FnMut() -> Option<C> + Send + 'static) -> Self {
        let mut s = Supervised {
            make: Box::new(make),
            inner: None,
            failures: 0,
            retry_at: Instant::now(),
            healthy_since: Instant::now(),
        };
        s.rebuild();
        s
    }

    fn rebuild(&mut self) {
        // `make` can panic too (a failing audio driver): that counts as a failure, not a crash.
        let made = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (self.make)())).unwrap_or(None);
        match made {
            Some(c) => {
                self.inner = Some(c);
                self.healthy_since = Instant::now();
                if self.failures > 0 {
                    log::line(format!("echo cancelling restarted (after {} failure(s))", self.failures));
                }
            }
            None => self.fail("couldn't start"),
        }
    }

    fn fail(&mut self, why: &str) {
        self.inner = None;
        self.failures += 1;
        let wait = BACKOFF_FIRST.saturating_mul(1 << (self.failures - 1).min(5)).min(BACKOFF_MAX);
        self.retry_at = Instant::now() + wait;
        log::line(format!("echo cancelling {why}; retrying in {} ms, meanwhile {}", wait.as_millis(), if FALLBACK_SILENCE.load(Ordering::Relaxed) { "silence" } else { "the raw phone mic" }));
    }

    /// Cleaned audio out; never panics and never returns less than it was given (apart from whole-frame waits).
    pub fn process(&mut self, input: &[f32], output: &mut Vec<f32>) {
        if self.inner.is_none() && Instant::now() >= self.retry_at {
            self.rebuild();
        }
        if let Some(c) = self.inner.as_mut() {
            let start = output.len();
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| c.clean(input, output)));
            if r.is_ok() {
                if self.failures > 0 && self.healthy_since.elapsed() >= HEALTHY {
                    self.failures = 0;
                }
                return;
            }
            output.truncate(start);
            self.fail("stopped (panic)");
        }
        if FALLBACK_SILENCE.load(Ordering::Relaxed) {
            output.extend(std::iter::repeat(0.0).take(input.len()));
        } else {
            output.extend_from_slice(input);
        }
    }
}

impl Supervised<Canceller> {
    /// The real thing, unless `TPL_NO_AEC` is set (A/B measurements).
    pub fn spawn() -> Option<Self> {
        std::env::var_os("TPL_NO_AEC").is_none().then(|| Supervised::new(Canceller::new))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Passes audio through, but panics on demand: the "kill the helper" test.
    struct Fake {
        die_in: Arc<Mutex<Option<u32>>>,
    }

    impl Clean for Fake {
        fn clean(&mut self, input: &[f32], output: &mut Vec<f32>) {
            let mut d = self.die_in.lock().unwrap();
            if let Some(n) = d.as_mut() {
                if *n == 0 {
                    *d = None;
                    drop(d);
                    panic!("simulated AEC3 fault");
                }
                *n -= 1;
            }
            output.extend(input.iter().map(|s| s * 0.5));
        }
    }

    /// These two tests share `FALLBACK_SILENCE`.
    static SERIAL: Mutex<()> = Mutex::new(());

    #[test]
    fn a_killed_canceller_is_back_in_under_three_seconds_and_the_mic_never_stops() {
        let _one_at_a_time = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let die_in = Arc::new(Mutex::new(None));
        let d = die_in.clone();
        let mut s = Supervised::new(move || Some(Fake { die_in: d.clone() }));
        FALLBACK_SILENCE.store(false, Ordering::Relaxed);
        let frame = vec![0.4f32; FRAME];
        let mut out = Vec::new();
        s.process(&frame, &mut out);
        assert_eq!(out[0], 0.2); // cleaned
        *die_in.lock().unwrap() = Some(0); // the next call panics
        let killed = Instant::now();
        let (mut raw, mut back) = (0, None);
        while killed.elapsed() < Duration::from_secs(4) && back.is_none() {
            out.clear();
            s.process(&frame, &mut out);
            assert_eq!(out.len(), FRAME, "audio must keep flowing while it is down");
            if out[0] == 0.4 {
                raw += 1; // the raw phone mic stood in
            } else if out[0] == 0.2 {
                back = Some(killed.elapsed());
            }
            thread::sleep(Duration::from_millis(10));
        }
        let back = back.expect("never came back");
        eprintln!("recovered in {back:?} ({raw} raw frames meanwhile)");
        assert!(raw > 0);
        assert!(back < Duration::from_secs(3), "{back:?}");
    }

    #[test]
    fn silence_stands_in_when_chosen_and_it_keeps_trying_when_it_cannot_start() {
        let _one_at_a_time = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        FALLBACK_SILENCE.store(true, Ordering::Relaxed);
        let mut s = Supervised::new(|| -> Option<Fake> { None });
        let frame = vec![0.4f32; FRAME];
        let mut out = Vec::new();
        s.process(&frame, &mut out);
        assert_eq!(out, vec![0.0; FRAME]);
        assert_eq!(s.failures, 1);
        thread::sleep(BACKOFF_FIRST + Duration::from_millis(50));
        out.clear();
        s.process(&frame, &mut out);
        assert_eq!(s.failures, 2, "it retried");
        assert!(s.retry_at > Instant::now() + Duration::from_millis(500), "the wait grew");
        FALLBACK_SILENCE.store(false, Ordering::Relaxed);
    }

    #[test]
    fn the_speaker_timeline_fills_gaps_with_silence_and_skips_overlaps() {
        let mut t = Timeline::default();
        t.write(1000, &[1.0; 10]);
        t.write(1015, &[2.0; 5]); // 5 samples of silence between
        t.write(1018, &[3.0; 4]); // overlaps the last 2: only the new 2 are kept
        assert_eq!(t.end(), 1022);
        let mut out = [9.0; 26];
        t.read(998, &mut out);
        assert_eq!(&out[..2], &[0.0, 0.0]); // before the start
        assert_eq!(&out[2..12], &[1.0; 10]);
        assert_eq!(&out[12..17], &[0.0; 5]);
        assert_eq!(&out[17..22], &[2.0; 5]);
        assert_eq!(&out[22..24], &[3.0; 2]);
        assert_eq!(&out[24..], &[0.0; 2]); // not recorded yet
    }

    /// Step 4b (26 Oct): does the extra stream delay change the echo that is left? Offline, through the same AEC3
    /// pipeline and `config()` the app uses. A speech-like clip is "played"; the phone hears 0.4 × it after
    /// `lag` ms (a room plus a slow speaker); the reference AEC3 sees is the clip `extra` ms older, exactly as
    /// `Canceller::process` pairs them (apparent lag = lag − extra). Prints the table for the investigation note.
    #[test]
    #[ignore = "50 s: cargo test -p tpl-desktop the_extra_delay -- --ignored --nocapture"]
    fn the_extra_delay_setting_changes_the_echo_left() {
        fn clip(n: usize) -> Vec<f32> {
            let mut seed = 12345u32;
            let mut lp = 0.0f32;
            (0..n)
                .map(|i| {
                    seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                    let noise = (seed >> 8) as f32 / (1u32 << 23) as f32 - 1.0;
                    lp += 0.25 * (noise - lp); // dull the noise towards a voice-like tilt
                    let t = i as f32 / RATE as f32;
                    let env = (0.5 + 0.5 * (2.0 * std::f32::consts::PI * 3.7 * t).sin()).powf(1.5);
                    lp * env * 1.6
                })
                .collect()
        }
        fn left_db(lag_ms: usize, extra_ms: usize) -> f32 {
            let total = RATE as usize * 12;
            let play = clip(total);
            let lag = lag_ms * MS as usize;
            let extra = extra_ms * MS as usize;
            // A direct path plus two room reflections, so the echo is only partly correlated with what was played.
            let tap = |n: usize, d: usize, g: f32| if n >= d { g * play[n - d] } else { 0.0 };
            let heard = |n: usize| tap(n, lag, 0.4) + tap(n, lag + 9 * MS as usize, 0.15) + tap(n, lag + 23 * MS as usize, -0.1);
            let reference = |n: usize| if n >= extra { play[n - extra] } else { 0.0 };
            let format = AudioFormat::ten_ms(RATE, 1);
            let mut pipe = linear::builder(format, format)
                .aec3_config(config())
                .enable_noise_suppression(false)
                .enable_gain_controller2(false)
                .build()
                .unwrap();
            let (mut out, mut frame, mut render) = (vec![0.0f32; FRAME], vec![0.0f32; FRAME], vec![0.0f32; FRAME]);
            let (mut before, mut after) = (0.0f64, 0.0f64);
            for k in 0..total / FRAME {
                for i in 0..FRAME {
                    frame[i] = heard(k * FRAME + i);
                    render[i] = reference(k * FRAME + i);
                }
                pipe.handle_render_frame(&render).unwrap();
                pipe.process_capture_frame(&frame, &mut out).unwrap();
                if k * FRAME >= total / 2 {
                    before += frame.iter().map(|s| (*s as f64).powi(2)).sum::<f64>();
                    after += out.iter().map(|s| (*s as f64).powi(2)).sum::<f64>();
                }
            }
            (10.0 * (after / before.max(1e-12)).max(1e-12).log10()) as f32
        }
        eprintln!("echo left (dB re. the echo, lower is better), 12 s clip, last 6 s:");
        eprintln!("  lag ms | extra 0 | extra lag-60 | extra lag+0");
        let mut rows = Vec::new();
        for lag in [60usize, 150, 300, 450] {
            let (a, b) = (left_db(lag, 0), left_db(lag, lag.saturating_sub(60)));
            let c = left_db(lag, lag);
            eprintln!("  {lag:6} | {a:7.1} | {b:12.1} | {c:11.1}");
            rows.push((lag, a, b, c));
        }
        // The setting must be able to move the echo: somewhere it changes the result by a clear margin.
        let best_gain = rows.iter().map(|(_, a, b, c)| a - b.min(*c)).fold(f32::MIN, f32::max);
        assert!(best_gain.abs() > 3.0 || rows.iter().any(|(_, a, b, c)| (a - b).abs() > 3.0 || (a - c).abs() > 3.0),
            "the extra delay changed nothing measurable: {rows:?}");
    }

    #[test]
    fn the_speaker_timeline_keeps_three_seconds() {
        let mut t = Timeline::default();
        for i in 0..400 {
            t.write(i * FRAME as i64, &[0.5; FRAME]);
        }
        assert_eq!(t.buf.len(), KEEP);
        assert_eq!(t.end(), 400 * FRAME as i64);
    }
}
