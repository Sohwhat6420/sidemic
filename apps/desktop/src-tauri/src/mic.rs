//! The microphone switch. Discord and games listen to "CABLE Output"; this thread plays the chosen source into
//! "CABLE Input": the phone's cleaned mic (over the adb-forwarded socket), or the laptop's own mic when the phone
//! can't be reached, so nobody ever hears silence. Choosing the source here means Windows' default mic and the
//! apps' settings never change.

use std::collections::VecDeque;
use std::io::Read;
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use windows::Win32::Media::Audio::{eCapture, eRender};

use crate::adb::MIC_PORT;
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
}

pub fn spawn(rx: Receiver<Route>, status: Arc<MicStatus>, events: Sender<String>) {
    thread::Builder::new()
        .name("tpl-mic".into())
        .spawn(move || run(rx, status, events))
        .expect("mic thread");
}

const TARGET: usize = (RATE / 1000 * 40) as usize; // keep ~40 ms queued
const HIGH: usize = TARGET * 2; // above this, gently speed up
const PANIC: usize = TARGET * 5; // above this (a Wi-Fi burst), jump back to TARGET

struct PhoneStream {
    ring: Arc<Mutex<VecDeque<f32>>>,
    alive: Arc<AtomicBool>,
    sock: TcpStream,
}

impl PhoneStream {
    fn connect() -> std::io::Result<PhoneStream> {
        let sock = TcpStream::connect_timeout(&([127, 0, 0, 1], MIC_PORT).into(), Duration::from_millis(500))?;
        sock.set_nodelay(true)?;
        let ring = Arc::new(Mutex::new(VecDeque::with_capacity(RATE as usize)));
        let alive = Arc::new(AtomicBool::new(true));
        let (r, a, mut s) = (ring.clone(), alive.clone(), sock.try_clone()?);
        thread::spawn(move || {
            let mut buf = [0u8; 960];
            let mut carry: Option<u8> = None;
            loop {
                match s.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut bytes: Vec<u8> = carry.take().into_iter().collect();
                        bytes.extend_from_slice(&buf[..n]);
                        if bytes.len() % 2 == 1 {
                            carry = bytes.pop();
                        }
                        let mut q = r.lock().unwrap();
                        q.extend(bytes.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0));
                        if q.len() > PANIC {
                            let drop = q.len() - TARGET;
                            q.drain(..drop);
                        }
                    }
                }
            }
            a.store(false, Ordering::SeqCst);
        });
        Ok(PhoneStream { ring, alive, sock })
    }

    /// Takes up to `n` samples, skipping one in 200 while the queue runs long (0.5 % faster, inaudible), so the
    /// phone's and the laptop's clocks can't drift the delay upwards.
    fn take(&self, n: usize, out: &mut Vec<f32>) {
        let mut q = self.ring.lock().unwrap();
        let fast = q.len() > HIGH;
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
    let mut started = false;
    let mut block: Vec<f32> = Vec::with_capacity(RATE as usize / 10);
    let mut spare: Vec<f32> = Vec::new();

    loop {
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
            continue;
        }

        if cable.is_none() {
            match wasapi::find(eRender, |n| n.contains("CABLE Input")).and_then(|(d, _)| Render::open(&d)) {
                Ok(r) => cable = Some(r),
                Err(e) => {
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
        let phone_live = phone.as_ref().is_some_and(|p| p.alive.load(Ordering::SeqCst) && (started || p.queued() >= TARGET));
        if phone_live {
            started = true;
            status.phone_failed.store(false, Ordering::Relaxed);
        }
        let use_laptop = want == Route::Laptop || !phone_live;
        if use_laptop && laptop.is_none() {
            // The laptop's own mic: any capture device that isn't the cable itself.
            laptop = wasapi::find(eCapture, |n| !n.contains("CABLE")).and_then(|(d, _)| Capture::open(&d)).ok();
        } else if !use_laptop && laptop.is_some() {
            laptop = None;
        }

        // Fill the cable up to the target delay.
        let cable_ref = cable.as_ref().unwrap();
        let queued = cable_ref.queued() as usize;
        let need = TARGET.saturating_sub(queued).min(cable_ref.free() as usize);
        block.clear();
        if use_laptop {
            if let Some(c) = laptop.as_ref() {
                let _ = c.read(&mut spare);
                if spare.len() > PANIC {
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
