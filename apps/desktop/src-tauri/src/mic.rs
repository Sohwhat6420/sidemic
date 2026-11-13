//! The microphone switch. Discord and games listen to "CABLE Output"; this thread plays the chosen source into
//! "CABLE Input": the phone's cleaned mic (over the adb-forwarded socket), or the laptop's own mic when the phone
//! can't be reached, so nobody ever hears silence. Choosing the source here means Windows' default mic and the
//! apps' settings never change.

use std::collections::VecDeque;
use std::io::Read;
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use windows::Win32::Media::Audio::{eCapture, eRender};

use tpl_auth::Channel;

use crate::adb::{mic_port, session_key, HANDSHAKE};
use crate::echo;
use crate::wasapi::{self, Capture, Render, RATE};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    Off,
    Phone,
    Laptop,
}

/// What the mic thread reports back to the UI.
#[derive(Default)]
pub struct MicStatus {
    /// Which source is really playing into the cable: 0 off, 1 phone, 2 laptop.
    pub live: AtomicU32,
    /// Level of what's being sent, in dBFS × 10 + 1000 (so it fits an unsigned atomic).
    pub level: AtomicU32,
    /// Set when the phone stream failed and we fell back to the laptop.
    pub phone_failed: AtomicBool,
    /// Mic delay through the cable in milliseconds (our buffering; the phone adds its own 32 ms).
    pub delay_ms: AtomicU32,
    /// Set while VB-CABLE's "CABLE Input" can't be opened (not installed, or disabled): apps hear nothing.
    pub no_cable: AtomicBool,
}

pub fn spawn(rx: Receiver<Route>, status: Arc<MicStatus>, events: Sender<String>) {
    thread::Builder::new()
        .name("tpl-mic".into())
        .spawn(move || run(rx, status, events))
        .expect("mic thread");
}

/// How long to wait before looking for the laptop mic again after it couldn't be opened or stopped working.
/// Looking costs 1-2.5 ms (see wasapi.rs), so it mustn't happen on every 5 ms pass on a PC with no mic.
const LAPTOP_RETRY: Duration = Duration::from_secs(2);
/// How long to wait before reconnecting to the phone after its stream dropped (the phone may be refusing the mic).
const PHONE_RETRY: Duration = Duration::from_secs(1);
/// Once audio is flowing, this long with no bytes at all means the stream stalled (Wi-Fi gone before adb
/// noticed): drop it so the laptop mic covers instead of silence. The phone sends continuously, silence included,
/// and its screen-off bursts arrive about 190 ms apart, so 2 s is far outside normal.
const PHONE_STALL: Duration = Duration::from_secs(2);

const TARGET: usize = (RATE / 1000 * 40) as usize; // keep at least ~40 ms queued
const MAX_TARGET: usize = (RATE / 1000 * 300) as usize; // never more than 300 ms, however late the Wi-Fi

/// How much to keep queued so late audio never empties the cable: 40 ms, plus how late the link has been in the
/// last 5 s.
///
/// Lateness, not the gap between arrivals, is what drains the queue. Some phones batch Wi-Fi traffic while the
/// screen is off (a POCO X6 Pro on HyperOS delivered its mic in ~190 ms bursts, measured 17 Oct 2026), but each
/// burst carries the audio for its whole gap, so steady bursts need no extra queue; only a burst that comes later
/// than the others does. So: just before each arrival, note audio received minus time elapsed (where the queue
/// bottoms out), and keep the spread of those low points over the last 5 s on top of the 40 ms.
#[derive(Default)]
struct Jitter {
    start: Option<Instant>,
    received: i64,
    lows: VecDeque<(Instant, i64)>,
}

impl Jitter {
    /// Records `samples` arriving at `now`; returns the queue to keep, in samples.
    fn arrived(&mut self, now: Instant, samples: usize) -> usize {
        let start = *self.start.get_or_insert(now);
        let elapsed = ((now - start).as_micros() as i64 * RATE as i64) / 1_000_000;
        self.lows.push_back((now, self.received - elapsed));
        self.received += samples as i64;
        while self.lows.front().is_some_and(|(t, _)| now - *t > Duration::from_secs(5)) {
            self.lows.pop_front();
        }
        let hi = self.lows.iter().map(|(_, x)| *x).max().unwrap_or(0);
        let lo = self.lows.iter().map(|(_, x)| *x).min().unwrap_or(0);
        (TARGET + (hi - lo) as usize).min(MAX_TARGET)
    }
}

/// The playout side of `PhoneStream::take`, apart so it can be tested without a socket.
fn play(q: &mut VecDeque<f32>, buffering: &mut bool, target: usize, n: usize, out: &mut Vec<f32>) {
    if q.is_empty() && n > 0 {
        *buffering = true;
    }
    if *buffering {
        if q.len() < target {
            return;
        }
        *buffering = false;
    }
    let fast = q.len() > target * 2;
    let mut i = 0;
    while out.len() < n {
        let Some(s) = q.pop_front() else { break };
        i += 1;
        if fast && i % 200 == 0 {
            continue;
        }
        out.push(s);
    }
}

struct PhoneStream {
    ring: Arc<Mutex<VecDeque<f32>>>,
    alive: Arc<AtomicBool>,
    /// The queue to keep, in samples (see `Jitter`).
    target: Arc<AtomicUsize>,
    /// Refilling to the target before playing again (at the start, and after running dry).
    buffering: AtomicBool,
    sock: TcpStream,
}

impl PhoneStream {
    fn connect() -> std::io::Result<PhoneStream> {
        // No phone server of ours is running: nothing to connect to, and nothing to prove to it.
        let key = session_key().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotConnected, "no phone server session"))?;
        let sock = TcpStream::connect_timeout(&([127, 0, 0, 1], mic_port()).into(), Duration::from_millis(500))?;
        sock.set_nodelay(true)?;
        let ring = Arc::new(Mutex::new(VecDeque::with_capacity(RATE as usize)));
        let alive = Arc::new(AtomicBool::new(true));
        let target = Arc::new(AtomicUsize::new(TARGET));
        let (r, a, t, mut s) = (ring.clone(), alive.clone(), target.clone(), sock.try_clone()?);        thread::spawn(move || {
            // Marks the stream dead however this thread ends, a panic included, so the mic loop reconnects or
            // falls back to the laptop mic instead of waiting on a stream that will never deliver.
            struct Dead(Arc<AtomicBool>);
            impl Drop for Dead {
                fn drop(&mut self) {
                    self.0.store(false, Ordering::SeqCst);
                }
            }
            let _dead = Dead(a);
            // Prove this session's key, and have the phone prove it too, before a byte counts as audio. Until
            // then the phone hasn't opened its mic. This runs here, not on the mic loop, because it waits on the
            // phone over Wi-Fi; a failure ends the stream like any other drop, and the loop tries again.
            if let Err(e) = tpl_auth::prove_tcp(&mut s, &key, Channel::Mic, HANDSHAKE) {
                crate::log::line(format!("mic: the phone's mic socket didn't complete the session handshake ({e})"));
                return;
            }
            let mut buf = [0u8; 960];
            let mut carry: Option<u8> = None;
            let mut jitter = Jitter::default();
            // TPL_NO_AEC=1 turns echo cancelling off (for A/B measurements).
            wasapi::com_init();
            let mut aec = echo::Supervised::spawn();
            let (mut samples, mut clean) = (Vec::new(), Vec::new());
            let mut flowing = false;
            loop {
                match s.read(&mut buf) {
                    // A timeout (a stall, see PHONE_STALL) ends the stream like a close does.
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        // Not before the first audio: the phone may take a moment to start its voice chain.
                        if !flowing {
                            flowing = true;
                            let _ = s.set_read_timeout(Some(PHONE_STALL));
                        }
                        let target = jitter.arrived(Instant::now(), n / 2);
                        t.store(target, Ordering::Relaxed);
                        let mut bytes: Vec<u8> = carry.take().into_iter().collect();
                        bytes.extend_from_slice(&buf[..n]);
                        if bytes.len() % 2 == 1 {
                            carry = bytes.pop();
                        }
                        samples.clear();
                        samples.extend(bytes.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0));
                        let arrived = match aec.as_mut() {
                            Some(aec) => {
                                clean.clear();
                                aec.process(&samples, &mut clean);
                                &clean
                            }
                            None => &samples,
                        };
                        let mut q = r.lock().unwrap();
                        q.extend(arrived);
                        // Far more than a burst's worth queued (the link stalled and caught up): jump back.
                        if q.len() > target * 5 {
                            let drop = q.len() - target;
                            q.drain(..drop);
                        }
                    }
                }
            }
        });
        Ok(PhoneStream { ring, alive, target, buffering: AtomicBool::new(true), sock })
    }

    fn target(&self) -> usize {
        self.target.load(Ordering::Relaxed)
    }

    /// Takes up to `n` samples, skipping one in 200 while the queue runs long (0.5 % faster, inaudible), so the
    /// phone's and the laptop's clocks can't drift the delay upwards. After running dry it waits until the queue
    /// is back up to the target, so a bursty link gives one short gap instead of a gap between every burst.
    fn take(&self, n: usize, out: &mut Vec<f32>) {
        let mut buffering = self.buffering.load(Ordering::Relaxed);
        play(&mut self.ring.lock().unwrap(), &mut buffering, self.target(), n, out);
        self.buffering.store(buffering, Ordering::Relaxed);
    }

    fn queued(&self) -> usize {
        self.ring.lock().unwrap().len()
    }
}

impl Drop for PhoneStream {
    fn drop(&mut self) {
        let _ = self.sock.shutdown(Shutdown::Both);
    }
}

fn run(rx: Receiver<Route>, status: Arc<MicStatus>, events: Sender<String>) {
    wasapi::com_init();
    let mut want = Route::Off;
    let mut cable: Option<Render> = None;
    let mut phone: Option<PhoneStream> = None;
    let mut laptop: Option<Capture> = None;
    let mut retry_phone_at = Instant::now();
    let mut retry_laptop_at = Instant::now();
    let mut started = false;
    let mut block: Vec<f32> = Vec::with_capacity(RATE as usize / 10);
    let mut spare: Vec<f32> = Vec::new();

    let mut last_tick = Instant::now();
    // After a fault or a wake from sleep the audio devices come and go for a moment: reopen only once 1 s has
    // passed without another fault.
    let mut rebuild_at = Instant::now();
    loop {
        // A gap this long means the laptop slept (or the process was frozen): the old handles are stale.
        if last_tick.elapsed() > Duration::from_secs(5) {
            let _ = events.send("mic: resumed after a pause; reopening the audio devices".into());
            cable = None;
            laptop = None;
            phone = None;
            started = false;
            rebuild_at = Instant::now() + Duration::from_secs(1);
        }
        last_tick = Instant::now();
        if cable.as_ref().is_some_and(|c| !c.healthy()) {
            let _ = events.send("mic: the cable device went away; reopening it".into());
            cable = None;
            laptop = None;
            rebuild_at = Instant::now() + Duration::from_secs(1);
        }
        // Newest request wins.
        while let Ok(r) = rx.try_recv() {
            want = r;
        }
        if want == Route::Off {
            if cable.is_some() || phone.is_some() || laptop.is_some() {
                cable = None;
                phone = None; // closing the socket stops the phone's mic (and its privacy dot)
                laptop = None;
                status.live.store(0, Ordering::Relaxed);
                status.level.store(0, Ordering::Relaxed);
            }
            match rx.recv() {
                Ok(r) => want = r,
                Err(_) => return,
            }
            last_tick = Instant::now(); // waiting while Off is not a pause
            continue;
        }

        if cable.is_none() {
            if Instant::now() < rebuild_at {
                thread::sleep(Duration::from_millis(100));
                continue;
            }
            match wasapi::find(eRender, |n| n.contains("CABLE Input")).and_then(|(d, _)| Render::open(&d)) {
                Ok(r) => {
                    cable = Some(r);
                    status.no_cable.store(false, Ordering::Relaxed);
                }
                Err(e) => {
                    status.no_cable.store(true, Ordering::Relaxed);
                    let _ = events.send(format!("mic: CABLE Input not available ({e})"));
                    thread::sleep(Duration::from_secs(2));
                    continue;
                }
            }
        }

        // Phone wanted: (re)connect, falling back to the laptop mic meanwhile.
        if want == Route::Phone {
            if phone.as_ref().is_some_and(|p| !p.alive.load(Ordering::SeqCst)) {
                phone = None;
                retry_phone_at = Instant::now() + PHONE_RETRY;
                status.phone_failed.store(true, Ordering::Relaxed);
                let _ = events.send("mic: phone stream dropped; laptop mic until it's back".into());
            }
            if phone.is_none() && Instant::now() >= retry_phone_at {
                match PhoneStream::connect() {
                    Ok(p) => {
                        phone = Some(p);
                        started = false;
                    }
                    Err(_) => {
                        retry_phone_at = Instant::now() + Duration::from_secs(1);
                        status.phone_failed.store(true, Ordering::Relaxed);
                    }
                }
            }
        } else {
            phone = None;
        }
        // The phone counts as live once audio actually arrives; until then the laptop mic covers.
        let phone_live = phone.as_ref().is_some_and(|p| p.alive.load(Ordering::SeqCst) && (started || p.queued() >= p.target()));
        if phone_live {
            started = true;
            status.phone_failed.store(false, Ordering::Relaxed);
        }
        let use_laptop = want == Route::Laptop || !phone_live;
        if use_laptop && laptop.is_none() && Instant::now() >= retry_laptop_at {
            // The laptop's own mic: Windows' default one unless that is the cable itself, else any that isn't.
            laptop =
                wasapi::find_default_first(eCapture, |n| !n.contains("CABLE")).and_then(|(d, _)| Capture::open(&d)).ok();
            if laptop.is_none() {
                retry_laptop_at = Instant::now() + LAPTOP_RETRY;
            }
        } else if !use_laptop && laptop.is_some() {
            laptop = None;
        }

        // Fill the cable up to the target delay.
        let cable_ref = cable.as_ref().unwrap();
        let queued = cable_ref.queued() as usize;
        let need = TARGET.saturating_sub(queued).min(cable_ref.free() as usize);
        block.clear();
        if use_laptop {
            // A headset unplugged mid-call fails every read from then on: drop it and look for a mic again.
            if let Some(Err(e)) = laptop.as_ref().map(|c| c.read(&mut spare)) {
                let _ = events.send(format!("mic: the laptop mic stopped ({e}); looking for one again"));
                laptop = None;
                spare.clear();
                retry_laptop_at = Instant::now() + LAPTOP_RETRY;
            }
            if laptop.is_some() {
                if spare.len() > TARGET * 5 {
                    let drop = spare.len() - TARGET;
                    spare.drain(..drop);
                }
                let n = need.min(spare.len());
                block.extend(spare.drain(..n));
            }
            status.live.store(2, Ordering::Relaxed);
        } else if let Some(p) = phone.as_ref() {
            p.take(need, &mut block);
            spare.clear();
            status.live.store(1, Ordering::Relaxed);
        }
        if !block.is_empty() {
            let rms = (block.iter().map(|s| s * s).sum::<f32>() / block.len() as f32).sqrt();
            let db = (20.0 * rms.max(1e-5).log10()).max(-100.0);
            status.level.store(((db * 10.0) + 1000.0) as u32, Ordering::Relaxed);
            let _ = cable_ref.write(&block);
        } else if queued < (RATE / 1000 * 5) as usize {
            // Nothing arrived in time: a few ms of silence beats a click.
            let _ = cable_ref.write(&vec![0.0; (RATE / 1000 * 10) as usize]);
        }
        let source_q = phone.as_ref().map_or(spare.len(), |p| p.queued());
        status.delay_ms.store(((queued + source_q) as u32 * 1000) / RATE, Ordering::Relaxed);
        thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_a_session_key_the_phone_mic_is_never_dialled() {
        // No phone server was started here, so there is no key, and the mic port isn't even connected to.
        let e = PhoneStream::connect().err().expect("connected without a session key");
        assert_eq!(e.kind(), std::io::ErrorKind::NotConnected);
    }

    /// Plays `secs` of a link that delivers `burst_ms` of audio every `burst_ms` (offset by a repeating
    /// jitter pattern), pulling 5 ms every 5 ms like the cable loop. Returns the ms of audio that came up short
    /// after the first second.
    fn short_ms(burst_ms: u64, jitter_ms: &[u64], adaptive: bool, secs: u64) -> usize {
        let per_ms = RATE as usize / 1000;
        let start = Instant::now();
        let mut jitter = Jitter::default();
        let mut target = TARGET;
        let (mut q, mut buffering, mut short) = (VecDeque::new(), true, 0);
        let mut next_burst = 0u64;
        let mut k = 0;
        for ms in (0..secs * 1000).step_by(5) {
            while next_burst <= ms {
                let t = jitter.arrived(start + Duration::from_millis(next_burst), burst_ms as usize * per_ms);
                if adaptive {
                    target = t;
                }
                q.extend(std::iter::repeat(0.1f32).take(burst_ms as usize * per_ms));
                next_burst += burst_ms + jitter_ms[k % jitter_ms.len()];
                next_burst -= jitter_ms[(k + jitter_ms.len() - 1) % jitter_ms.len()];
                k += 1;
            }
            let mut out = Vec::new();
            play(&mut q, &mut buffering, target, 5 * per_ms, &mut out);
            if ms >= 1000 {
                short += 5 * per_ms - out.len();
            }
        }
        short / per_ms
    }

    /// Replays a real arrival log ("ms since first chunk,bytes", one recv per line) through the playout logic,
    /// pulling 5 ms every 5 ms. Returns (ms of audio short after the first second, final target in ms).
    fn replay(log: &str, adaptive: bool) -> (usize, usize) {
        let per_ms = RATE as usize / 1000;
        let arrivals: Vec<(u64, usize)> = log
            .lines()
            .filter_map(|l| l.split_once(','))
            .map(|(t, b)| (t.trim().parse().unwrap(), b.trim().parse().unwrap()))
            .collect();
        let start = Instant::now();
        let (mut jitter, mut q, mut buffering, mut short) = (Jitter::default(), VecDeque::new(), true, 0);
        let mut target = TARGET;
        let mut next = arrivals.iter().peekable();
        let end = arrivals.last().unwrap().0;
        for ms in (0..end).step_by(5) {
            while let Some(&&(t, bytes)) = next.peek().filter(|(t, _)| *t <= ms) {
                next.next();
                let t = jitter.arrived(start + Duration::from_millis(t), bytes / 2);
                if adaptive {
                    target = t;
                }
                q.extend(std::iter::repeat(0.1f32).take(bytes / 2));
            }
            let mut out = Vec::new();
            // The old code: start at 40 ms, then play whatever is there.
            play(&mut q, &mut buffering, if adaptive { target } else if ms == 0 { TARGET } else { 0 }, 5 * per_ms, &mut out);
            if ms >= 1000 {
                short += 5 * per_ms - out.len();
            }
        }
        (short / per_ms, target / per_ms)
    }

    #[test]
    fn real_poco_screen_off_arrivals_play_without_gaps() {
        // Measured 17 Oct 2026: POCO X6 Pro, HyperOS 2, screen off, 19 s: 46 gaps over 100 ms, the longest 201 ms.
        let log = include_str!("fixtures/poco-screen-off-mic-arrivals.csv");
        let (short, target) = replay(log, true);
        let (old, _) = replay(log, false);
        eprintln!("screen off: adaptive {short} ms short (queue {target} ms); fixed 40 ms queue {old} ms short");
        assert!(short <= 10, "{short} ms short");
        assert!(old > short);
        assert!(target <= 240);
    }

    #[test]
    fn real_poco_screen_on_arrivals_stay_at_40_ms() {
        let log = include_str!("fixtures/poco-screen-on-mic-arrivals.csv");
        let (short, target) = replay(log, true);
        eprintln!("screen on: {short} ms short, queue {target} ms");
        assert_eq!(short, 0);
        assert!(target <= 80, "{target}");
    }

    #[test]
    fn a_smooth_link_keeps_the_40_ms_queue() {
        let mut j = Jitter::default();
        let t0 = Instant::now();
        let mut target = 0;
        for i in 0..500 {
            target = j.arrived(t0 + Duration::from_millis(i * 10), 480);
        }
        assert_eq!(target, TARGET);
    }

    #[test]
    fn steady_bursts_cost_nothing_and_a_late_one_grows_the_queue_for_5_s() {
        let per_ms = RATE as usize / 1000;
        let mut j = Jitter::default();
        let t0 = Instant::now();
        // 190 ms bursts, each carrying 190 ms of audio: no extra queue needed.
        let mut target = 0;
        for i in 0..20 {
            target = j.arrived(t0 + Duration::from_millis(i * 190), 190 * per_ms);
        }
        assert_eq!(target, TARGET);
        // One burst 100 ms late: the queue grows by that much at once...
        let grown = j.arrived(t0 + Duration::from_millis(20 * 190 + 100), 190 * per_ms);
        assert_eq!(grown, TARGET + 100 * per_ms);
        // ...and shrinks back once it is 5 s old.
        let mut t = 21 * 190 + 100;
        while t < 21 * 190 + 100 + 5200 {
            target = j.arrived(t0 + Duration::from_millis(t), 190 * per_ms);
            t += 190;
        }
        assert_eq!(target, TARGET);
        // A 2 s stall is capped.
        assert_eq!(j.arrived(t0 + Duration::from_millis(t + 2000), 190 * per_ms), MAX_TARGET);
    }

    #[test]
    fn synthetic_late_bursts_are_mostly_covered() {
        // The POCO X6 Pro with its screen off: ~180 ms bursts, arriving up to 40 ms early or late.
        let pattern = [0, 30, 10, 40, 0, 20];
        // The first late burst in a while still costs a few ms; after that the queue covers it.
        let new = short_ms(180, &pattern, true, 20);
        // The fixed 40 ms queue it replaces came up short whenever a burst arrived late.
        let old = short_ms(180, &pattern, false, 20);
        eprintln!("synthetic bursts: adaptive {new} ms short, fixed 40 ms queue {old} ms short, in 19 s");
        assert!(new * 2 < old, "{new} vs {old}");
    }
}
