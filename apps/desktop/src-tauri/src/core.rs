//! The app's brain. One thread owns the Smart auto engine and everything it decides about; small helper threads
//! keep the phone link up, open and close Bluetooth audio, and watch the laptop (who has the mic, what is playing,
//! which app is in front). They all talk to the brain through one channel, and the UI only ever reads `snapshot`.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::process::Child;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};
use tpl_policy::{
    friendly_name, Action, AudioMode, Config, DecisionKind, Engine, Event, Memory, MicMode, MicRoute, Placement, Situation,
};
use windows::Win32::Media::Audio::eCapture;

use crate::adb::{Adb, DeviceState};
use crate::mic::{self, MicStatus, Route};
use crate::phone::{PhoneControl, PhoneEvent};
use crate::{log, wasapi, winrt};

/// Everything the brain hears about.
pub enum Msg {
    Phone(PhoneEvent),
    Link(Link),
    A2dp { available: bool, error: Option<String> },
    Laptop(Event),
    Act { action: String, arg: Option<String> },
    MicNote(String),
}

/// How far the phone link has got, for the hints under the phone's name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Link {
    NoAdb,
    Missing,
    Unauthorised,
    Starting,
    Failed(String),
    Ready,
}

/// What the UI holds on to: a way to send actions, and the latest state.
#[derive(Clone)]
pub struct Handle {
    pub tx: Sender<Msg>,
    pub snapshot: Arc<Mutex<Value>>,
}

#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    mic_mode: MicMode,
    audio_mode: AudioMode,
    smart_auto: bool,
    /// Auto-start was switched on at first run (his ruling: start with Windows); after that it's his switch.
    autostart_set: bool,
    /// Set while paused: the settings to put back on Resume. Pausing means laptop mic, phone audio on the
    /// phone, and nothing automatic — Windows as if the app weren't there, but Discord keeps a working mic.
    paused: Option<Paused>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Paused {
    mic_mode: MicMode,
    audio_mode: AudioMode,
    smart_auto: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { mic_mode: MicMode::Auto, audio_mode: AudioMode::Auto, smart_auto: true, autostart_set: false, paused: None }
    }
}

struct Banner {
    text: String,
    buttons: Vec<(String, String, String)>, // label, action, arg
    until: Option<Instant>,
}

struct Core {
    app: AppHandle,
    engine: Engine,
    dir: PathBuf,
    settings: Settings,
    mic_tx: Sender<Route>,
    mic: Arc<MicStatus>,
    mic_note: Option<String>,
    ctl: PhoneControl,
    a2dp_tx: Sender<A2dpCmd>,
    a2dp_error: Option<String>,
    a2dp_available: bool,
    link: Link,
    name: Option<String>,
    battery: Option<u32>,
    charging: bool,
    volume: Option<f64>,
    wallpaper: Option<String>,
    media_title: Option<String>,
    ping_sent: Option<Instant>,
    ping_ms: Option<u32>,
    banner: Option<Banner>,
    speaker: String,
    snapshot: Arc<Mutex<Value>>,
    last_json: String,
    last_now: u64,
    pings: Vec<u32>,
    last_live: String,
}

pub fn start(app: AppHandle, data_dir: PathBuf, phone_dir: PathBuf) -> Handle {
    let _ = fs::create_dir_all(&data_dir);
    let (tx, rx) = mpsc::channel::<Msg>();
    let snapshot = Arc::new(Mutex::new(json!({})));

    // The mic switch, with its notes forwarded to the brain.
    let (mic_tx, mic_rx) = mpsc::channel::<Route>();
    let (note_tx, note_rx) = mpsc::channel::<String>();
    let mic_status = Arc::new(MicStatus::default());
    mic::spawn(mic_rx, mic_status.clone(), note_tx);
    forward(note_rx, tx.clone(), Msg::MicNote);

    // The phone link.
    let ctl = PhoneControl::default();
    let (ptx, prx) = mpsc::channel::<PhoneEvent>();
    forward(prx, tx.clone(), Msg::Phone);
    {
        let (tx, ctl) = (tx.clone(), ctl.clone());
        thread::Builder::new().name("tpl-adb".into()).spawn(move || adb_thread(tx, ctl, ptx, phone_dir)).unwrap();
    }

    // Bluetooth audio from the phone.
    let (a2dp_tx, a2dp_rx) = mpsc::channel::<A2dpCmd>();
    {
        let tx = tx.clone();
        thread::Builder::new().name("tpl-a2dp".into()).spawn(move || a2dp_thread(a2dp_rx, tx)).unwrap();
    }

    // The laptop watchers.
    {
        let tx = tx.clone();
        thread::Builder::new().name("tpl-laptop".into()).spawn(move || laptop_thread(tx)).unwrap();
    }

    let settings: Settings =
        fs::read_to_string(data_dir.join("settings.json")).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    let memory = fs::read_to_string(data_dir.join("memory.json"))
        .ok()
        .and_then(|s| Memory::from_json(&s).ok())
        .unwrap_or_default();

    let core = Core {
        app,
        engine: Engine::new(Config::default(), memory),
        dir: data_dir,
        settings,
        mic_tx,
        mic: mic_status,
        mic_note: None,
        ctl,
        a2dp_tx,
        a2dp_error: None,
        a2dp_available: false,
        link: Link::Starting,
        name: None,
        battery: None,
        charging: false,
        volume: None,
        wallpaper: None,
        media_title: None,
        ping_sent: None,
        ping_ms: None,
        banner: None,
        speaker: String::new(),
        snapshot: snapshot.clone(),
        last_json: String::new(),
        last_now: 0,
        pings: Vec::new(),
        last_live: "off".into(),
    };
    thread::Builder::new().name("tpl-core".into()).spawn(move || core.run(rx)).unwrap();
    Handle { tx, snapshot }
}

fn forward<T: Send + 'static>(rx: Receiver<T>, tx: Sender<Msg>, wrap: fn(T) -> Msg) {
    thread::spawn(move || {
        for v in rx {
            if tx.send(wrap(v)).is_err() {
                return;
            }
        }
    });
}

fn epoch_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

impl Core {
    fn run(mut self, rx: Receiver<Msg>) {
        wasapi::com_init();
        // Dev builds never register themselves to start at sign-in; only the installed app does.
        if !self.settings.autostart_set && !cfg!(debug_assertions) {
            self.settings.autostart_set = true;
            crate::set_autostart(&self.app, true);
            self.save_settings();
        }
        let (m, a, s) = (self.settings.mic_mode, self.settings.audio_mode, self.settings.smart_auto);
        self.feed(Event::SetMicMode { mode: m });
        self.feed(Event::SetAudioMode { mode: a });
        self.feed(Event::SetSmartAuto { on: s });

        let mut next_ping = Instant::now();
        let mut next_speaker = Instant::now();
        let mut next_stats = Instant::now() + Duration::from_secs(900);
        loop {
            if Instant::now() >= next_stats {
                next_stats = Instant::now() + Duration::from_secs(900);
                if !self.pings.is_empty() {
                    let max = self.pings.iter().max().copied().unwrap_or(0);
                    let avg = self.pings.iter().sum::<u32>() / self.pings.len() as u32;
                    log::line(format!(
                        "stats (15 min): ping avg {avg} ms, max {max} ms over {} pings; battery {:?}%",
                        self.pings.len(),
                        self.battery
                    ));
                    self.pings.clear();
                }
            }
            // Wake for the next message, the engine's next timer, or a 250 ms UI refresh, whichever is first.
            let mut wait = Duration::from_millis(250);
            if let Some(d) = self.engine.next_deadline() {
                wait = wait.min(Duration::from_millis(d.saturating_sub(epoch_ms()).max(1)));
            }
            match rx.recv_timeout(wait) {
                Ok(msg) => self.on_msg(msg),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            if self.engine.next_deadline().is_some_and(|d| d <= epoch_ms()) {
                self.feed(Event::Tick);
            }
            if Instant::now() >= next_ping && self.ctl.connected() {
                self.ping_sent = Some(Instant::now());
                self.ctl.ping();
                next_ping = Instant::now() + Duration::from_secs(5);
            }
            if Instant::now() >= next_speaker {
                self.speaker = wasapi::default_render().map(|(_, n)| n).unwrap_or_default();
                next_speaker = Instant::now() + Duration::from_secs(5);
            }
            if self.banner.as_ref().and_then(|b| b.until).is_some_and(|t| Instant::now() >= t) {
                self.banner = None;
            }
            self.publish();
        }
    }

    fn on_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Phone(ev) => match ev {
                PhoneEvent::Hello { name, model } => {
                    log::line(format!("phone said hello: {name} ({model})"));
                    self.name = Some(if name.is_empty() { model } else { name });
                    self.link_changed();
                }
                PhoneEvent::Battery { level, charging } => {
                    self.battery = Some(level);
                    self.charging = charging;
                }
                PhoneEvent::Volume(v) => self.volume = Some(v),
                PhoneEvent::Wallpaper(b64) => self.wallpaper = Some(format!("data:image/jpeg;base64,{b64}")),
                PhoneEvent::Media { playing, package, title } => {
                    self.media_title = (!title.is_empty()).then_some(title);
                    self.feed(Event::PhoneMedia { playing, package });
                }
                PhoneEvent::Pong => {
                    if let Some(t) = self.ping_sent.take() {
                        let ms = t.elapsed().as_millis() as u32;
                        self.ping_ms = Some(ms);
                        self.pings.push(ms);
                        if ms > 200 {
                            log::line(format!("slow ping: {ms} ms"));
                        }
                    }
                }
                PhoneEvent::Gone => {
                    log::line("phone control channel closed");
                    self.ping_ms = None;
                    self.link_changed();
                }
            },
            Msg::Link(l) => {
                if l != self.link {
                    log::line(format!("phone link: {l:?}"));
                    self.link = l;
                    self.link_changed();
                }
            }
            Msg::A2dp { available, error } => {
                if let Some(e) = &error {
                    log::line(format!("bluetooth audio error: {e}"));
                }
                if available != self.a2dp_available {
                    log::line(format!("bluetooth audio available: {available}"));
                }
                if error.is_some() {
                    self.a2dp_error = error;
                }
                if available != self.a2dp_available {
                    self.a2dp_available = available;
                    self.link_changed();
                }
            }
            Msg::Laptop(ev) => self.feed(ev),
            Msg::MicNote(n) => {
                log::line(&n);
                self.mic_note = Some(n);
            }
            Msg::Act { action, arg } => self.act(&action, arg.as_deref()),
        }
    }

    fn link_changed(&mut self) {
        let (mic_link, a2dp_available) = (self.ctl.connected(), self.a2dp_available);
        self.feed(Event::PhoneLink { mic_link, a2dp_available });
    }

    /// Hands an event to the engine and runs what it decides.
    fn feed(&mut self, ev: Event) {
        let now = epoch_ms().max(self.last_now);
        self.last_now = now;
        let memory_before = self.engine.memory().to_json();
        if !matches!(ev, Event::Tick) {
            log::line(format!("event {ev:?}"));
        }
        let last_decision = self.engine.decisions().last().map(|d| d.id);
        for a in self.engine.handle(now, ev) {
            log::line(format!("  -> {a:?}"));
            self.run_action(a);
        }
        for d in self.engine.decisions().iter().filter(|d| last_decision.is_none_or(|l| d.id > l)) {
            log::line(format!("decision #{}: {} ({})", d.id, d.text, d.reason));
        }
        let memory_after = self.engine.memory().to_json();
        if memory_after != memory_before {
            let _ = fs::write(self.dir.join("memory.json"), memory_after);
        }
    }

    fn run_action(&mut self, a: Action) {
        match a {
            Action::UseMic { route } => {
                let _ = self.mic_tx.send(match route {
                    MicRoute::Phone => Route::Phone,
                    MicRoute::Laptop => Route::Laptop,
                    MicRoute::None => Route::Off,
                });
            }
            Action::OpenA2dp => {
                self.a2dp_error = None;
                let hint = self.name.clone().unwrap_or_else(|| "Galaxy".into());
                let _ = self.a2dp_tx.send(A2dpCmd::Open(hint));
            }
            Action::CloseA2dp => {
                let _ = self.a2dp_tx.send(A2dpCmd::Close);
            }
            Action::PhonePlay => self.ctl.play(),
            Action::PhonePause => self.ctl.pause(),
            Action::DuckPhone => self.ctl.duck(),
            Action::UnduckPhone => self.ctl.unduck(),
            Action::AskAboutGame { exe } => {
                let game = friendly_name(&exe);
                self.banner = Some(Banner {
                    text: format!("Phone media started while {game} is open. Where should it play during {game}?"),
                    buttons: vec![
                        ("On the phone".into(), "answer_game".into(), format!("{exe}|phone")),
                        ("On the Pebble".into(), "answer_game".into(), format!("{exe}|pebble")),
                    ],
                    until: None,
                });
            }
            Action::Toast { text, offers_keep } => {
                let keep = self.engine.config().keep_window_ms;
                self.banner = Some(Banner {
                    text,
                    buttons: if offers_keep {
                        vec![("Keep on the Pebble".into(), "keep".into(), String::new())]
                    } else {
                        vec![("OK".into(), "dismiss".into(), String::new())]
                    },
                    until: Some(Instant::now() + Duration::from_millis(keep)),
                });
                // Never pops anything up: the notice waits in the flyout (and the tray tooltip) until he looks.
            }
        }
    }

    fn act(&mut self, action: &str, arg: Option<&str>) {
        if action != "volume" {
            log::line(format!("ui: {action} {}", arg.unwrap_or("")));
        }
        let st = self.engine.state();
        // Changing the mic, audio or Smart auto by hand while paused means he has taken over: no longer paused.
        if self.settings.paused.is_some()
            && matches!(action, "toggle_mic" | "mic_mode" | "audio_mode" | "toggle_audio" | "toggle_auto" | "hand_back")
        {
            self.settings.paused = None;
            self.save_settings();
        }
        match action {
            "mark" => {
                let mut snap = self.snapshot.lock().unwrap().clone();
                if let Some(o) = snap.as_object_mut() {
                    o.remove("wallpaper");
                    o.remove("decisions");
                }
                log::line("=== MARK: problem flagged from the tray around here ===");
                log::line(format!("state at mark: {snap}"));
                log::line(format!("engine at mark: {st:?}"));
            }
            "toggle_pause" => {
                if let Some(p) = self.settings.paused.take() {
                    log::line("resumed: settings put back");
                    self.set_mic_mode(p.mic_mode);
                    self.set_audio_mode(p.audio_mode);
                    self.settings.smart_auto = p.smart_auto;
                    self.save_settings();
                    self.feed(Event::SetSmartAuto { on: p.smart_auto });
                } else {
                    log::line("paused: laptop mic, phone audio on the phone, Smart auto off");
                    self.settings.paused =
                        Some(Paused { mic_mode: st.mic_mode, audio_mode: st.audio_mode, smart_auto: st.smart_auto });
                    self.banner = None;
                    self.settings.smart_auto = false;
                    self.feed(Event::SetSmartAuto { on: false });
                    self.set_mic_mode(MicMode::Laptop);
                    self.set_audio_mode(AudioMode::Phone);
                }
            }
            "toggle_mic" => {
                let mode = if st.mic_mode == MicMode::Laptop { MicMode::Auto } else { MicMode::Laptop };
                self.set_mic_mode(mode);
            }
            "mic_mode" => self.set_mic_mode(parse_mode(arg).map_or(MicMode::Auto, |m| match m {
                AudioMode::Auto => MicMode::Auto,
                AudioMode::Phone => MicMode::Phone,
                AudioMode::Laptop => MicMode::Laptop,
            })),
            "audio_mode" => self.set_audio_mode(parse_mode(arg).unwrap_or(AudioMode::Auto)),
            "toggle_audio" => {
                if st.phone_audio == Placement::Pebble {
                    if st.audio_mode == AudioMode::Laptop {
                        self.set_audio_mode(AudioMode::Auto);
                    }
                    self.feed(Event::HandBack);
                } else {
                    self.set_audio_mode(AudioMode::Laptop);
                }
            }
            "toggle_auto" => {
                self.settings.smart_auto = !st.smart_auto;
                self.save_settings();
                self.feed(Event::SetSmartAuto { on: !st.smart_auto });
            }
            "hand_back" => self.feed(Event::HandBack),
            "keep" => {
                self.banner = None;
                self.feed(Event::Keep);
            }
            "dismiss" => self.banner = None,
            "thats_wrong" => {
                if let Some(id) = arg.and_then(|a| a.parse().ok()) {
                    self.feed(Event::ThatsWrong { decision_id: id });
                }
            }
            "answer_game" => {
                if let Some((exe, where_)) = arg.and_then(|a| a.rsplit_once('|')) {
                    self.banner = None;
                    self.feed(Event::AnswerGame { exe: exe.to_owned(), keep_on_phone: where_ == "phone" });
                }
            }
            "forget_rule" => {
                if let Some(id) = arg.and_then(|a| a.parse().ok()) {
                    self.engine.memory_mut().delete_rule(id);
                    let _ = fs::write(self.dir.join("memory.json"), self.engine.memory().to_json());
                }
            }
            "toggle_autostart" => {
                let on = crate::autostart_enabled(&self.app);
                crate::set_autostart(&self.app, !on);
            }
            "volume" => {
                if let Some(v) = arg.and_then(|a| a.parse::<f64>().ok()) {
                    self.volume = Some(v);
                    self.ctl.volume(v.clamp(0.0, 1.0));
                }
            }
            _ => log::line(format!("unknown action {action}")),
        }
        self.publish();
    }

    fn set_mic_mode(&mut self, mode: MicMode) {
        self.settings.mic_mode = mode;
        self.save_settings();
        self.feed(Event::SetMicMode { mode });
    }

    fn set_audio_mode(&mut self, mode: AudioMode) {
        self.settings.audio_mode = mode;
        self.save_settings();
        self.feed(Event::SetAudioMode { mode });
    }

    fn save_settings(&self) {
        if let Ok(s) = serde_json::to_string_pretty(&self.settings) {
            let _ = fs::write(self.dir.join("settings.json"), s);
        }
    }

    /// Builds the UI's state and sends it if anything changed.
    fn publish(&mut self) {
        let st = self.engine.state();
        let connected = self.ctl.connected();
        let (hint, hint_long) = match &self.link {
            Link::NoAdb => ("adb isn't installed".to_owned(), "TRUE Phone Link uses adb to reach the phone. Install scrcpy (it includes adb) with: winget install Genymobile.scrcpy".to_owned()),
            Link::Missing => ("Tap Wireless debugging on your phone".to_owned(), "The phone isn't reachable over Wi-Fi. After a restart or a Wi-Fi change, the phone turns Wireless debugging off: swipe down and tap its tile in Quick settings. Both devices must be on the same Wi-Fi.".to_owned()),
            Link::Unauthorised => ("Allow this laptop on your phone".to_owned(), "The phone is asking whether to allow wireless debugging from this laptop on this network. Tap Allow, and tick Always allow on this network.".to_owned()),
            Link::Failed(e) => ("Couldn't start on the phone".to_owned(), format!("Starting TRUE Phone Link on the phone failed: {e}. It retries by itself.")),
            Link::Starting | Link::Ready if !connected => ("Connecting…".to_owned(), "Starting TRUE Phone Link on the phone. This takes a few seconds.".to_owned()),
            _ => (String::new(), String::new()),
        };

        let live = match self.mic.live.load(Ordering::Relaxed) {
            1 => "phone",
            2 => "laptop",
            _ => "off",
        };
        let level_db = self.mic.level.load(Ordering::Relaxed) as f64 / 10.0 - 100.0;
        if live != self.last_live {
            log::line(format!(
                "mic now: {live} (mic delay {} ms incl. 32 ms on the phone)",
                self.mic.delay_ms.load(Ordering::Relaxed) + 32
            ));
            self.last_live = live.to_owned();
        }
        let mic_why = if live == "laptop" && st.mic_mode != MicMode::Laptop {
            if !connected { "The phone isn't connected".to_owned() } else { "Waiting for the phone's mic".to_owned() }
        } else {
            String::new()
        };

        let speaker = speaker_name(&self.speaker);
        let speaker_short = if speaker.to_lowercase().contains("pebble") { "Pebble".to_owned() } else { speaker.clone() };
        let playing = st.phone_playing.then(|| {
            st.phone_package.as_deref().map(friendly_name).unwrap_or_else(|| self.media_title.clone().unwrap_or_default())
        });
        let last = self.engine.decisions().last();
        let audio_why = match (&self.a2dp_error, last) {
            (Some(e), _) => e.clone(),
            (None, Some(d)) => d.reason.clone(),
            _ => String::new(),
        };

        let decisions: Vec<Value> = self
            .engine
            .decisions()
            .iter()
            .map(|d| {
                let icon = match (&d.situation, d.kind) {
                    (Some(Situation::Game { .. }), _) => "game",
                    (_, DecisionKind::MovedToPebble | DecisionKind::KeptOnPebble) => "speaker",
                    _ => "phone",
                };
                let undoable = !d.corrected && d.situation.is_some();
                json!({"id": d.id, "at": d.at_ms, "icon": icon, "text": d.text, "reason": d.reason, "corrected": d.corrected, "undoable": undoable})
            })
            .collect();
        let rules: Vec<Value> = self
            .engine
            .memory()
            .rules()
            .iter()
            .map(|r| {
                let icon = if matches!(r.situation, Situation::Game { .. }) { "game" } else { "flow" };
                json!({"key": r.id.to_string(), "icon": icon, "text": r.describe()})
            })
            .collect();

        let state = json!({
            "connected": connected,
            "link_kind": "wifi",
            "link_text": "Wi-Fi, wireless debugging",
            "hint": hint,
            "hint_long": hint_long,
            "device": {"name": self.name},
            "battery": if connected { self.battery } else { None },
            "charging": self.charging,
            "volume": self.volume,
            "wallpaper": self.wallpaper,
            "ping_ms": self.ping_ms,
            "version": env!("CARGO_PKG_VERSION"),
            "autostart": crate::autostart_enabled(&self.app),
            "smart_auto": st.smart_auto,
            "paused": self.settings.paused.is_some(),
            "mic": {
                "mode": st.mic_mode, "live": live, "app": st.mic_users.first().map(|a| friendly_name(a)),
                "level_db": level_db, "delay_ms": self.mic.delay_ms.load(Ordering::Relaxed) + 32, "why": mic_why,
            },
            "audio": {
                "mode": st.audio_mode, "on_speaker": st.phone_audio == Placement::Pebble, "available": st.a2dp_available,
                "playing": playing, "why": audio_why, "speaker": speaker, "speaker_short": speaker_short,
            },
            "banner": self.banner.as_ref().map(|b| json!({
                "text": b.text,
                "buttons": b.buttons.iter().map(|(l, a, g)| json!({"label": l, "action": a, "arg": g})).collect::<Vec<_>>(),
            })),
            "decisions": decisions,
            "rules": rules,
        });
        let text = state.to_string();
        if text != self.last_json {
            self.last_json = text;
            let paused = self.settings.paused.is_some();
            let tip = tooltip(connected, &hint, live, &playing, st.phone_audio);
            crate::set_tray_tooltip(&self.app, &if paused { format!("{tip}\nPaused") } else { tip });
            crate::set_pause_label(&self.app, paused);
            *self.snapshot.lock().unwrap() = state.clone();
            let _ = self.app.emit("state", state);
        }
    }
}

fn parse_mode(arg: Option<&str>) -> Option<AudioMode> {
    match arg? {
        "auto" => Some(AudioMode::Auto),
        "phone" => Some(AudioMode::Phone),
        "laptop" => Some(AudioMode::Laptop),
        _ => None,
    }
}

/// "Speakers (Creative Pebble V3)" → "Creative Pebble V3".
fn speaker_name(endpoint: &str) -> String {
    match (endpoint.find('('), endpoint.rfind(')')) {
        (Some(a), Some(b)) if b > a => endpoint[a + 1..b].trim().to_owned(),
        _ => endpoint.to_owned(),
    }
}

fn tooltip(connected: bool, hint: &str, live: &str, playing: &Option<String>, audio: Placement) -> String {
    let mut t = String::from("TRUE Phone Link");
    if !connected {
        t += &format!("\n{hint}");
        return t;
    }
    if live != "off" {
        t += &format!("\nMic: {live}");
    }
    if let Some(p) = playing {
        t += &format!("\n{p} on the {}", if audio == Placement::Pebble { "speaker" } else { "phone" });
    }
    t
}

// ---------------------------------------------------------------------------------------------------------
// The phone link: keep the server running on the phone and the control channel connected.

static ADB: OnceLock<Adb> = OnceLock::new();
static SERIAL: Mutex<Option<String>> = Mutex::new(None);

/// Stops the phone server (called when the app quits), so the phone's mic and privacy dot are released.
pub fn shutdown() {
    if let (Some(adb), Some(serial)) = (ADB.get(), SERIAL.lock().unwrap().clone()) {
        adb.stop_server(&serial);
    }
}

fn adb_thread(tx: Sender<Msg>, ctl: PhoneControl, ptx: Sender<PhoneEvent>, phone_dir: PathBuf) {
    let Some(adb) = Adb::find(None) else {
        let _ = tx.send(Msg::Link(Link::NoAdb));
        return;
    };
    adb.start_daemon();
    let adb = ADB.get_or_init(|| adb);
    let mut server: Option<(String, Child)> = None;
    let mut fails = 0;
    loop {
        if ctl.connected() {
            thread::sleep(Duration::from_secs(2));
            continue;
        }
        match adb.device() {
            DeviceState::Ready(serial) => {
                let _ = tx.send(Msg::Link(Link::Starting));
                let stale = match server.as_mut() {
                    Some((s, c)) => *s != serial || c.try_wait().ok().flatten().is_some(),
                    None => true,
                };
                if stale || fails >= 3 {
                    if let Some((_, mut c)) = server.take() {
                        let _ = c.kill();
                    }
                    let started = adb.ensure_files(&serial, &phone_dir).and_then(|_| adb.start_server(&serial));
                    match started {
                        Ok(c) => {
                            *SERIAL.lock().unwrap() = Some(serial.clone());
                            server = Some((serial, c));
                            fails = 0;
                            thread::sleep(Duration::from_millis(1500));
                        }
                        Err(e) => {
                            let _ = tx.send(Msg::Link(Link::Failed(e)));
                            thread::sleep(Duration::from_secs(5));
                            continue;
                        }
                    }
                }
                if ctl.connect(ptx.clone()) {
                    fails = 0;
                    let _ = tx.send(Msg::Link(Link::Ready));
                } else {
                    fails += 1;
                }
            }
            DeviceState::Unauthorised(_) => {
                let _ = tx.send(Msg::Link(Link::Unauthorised));
            }
            DeviceState::Missing => {
                if let Some((_, mut c)) = server.take() {
                    let _ = c.kill();
                }
                let _ = tx.send(Msg::Link(Link::Missing));
            }
        }
        thread::sleep(Duration::from_secs(2));
    }
}

// ---------------------------------------------------------------------------------------------------------
// Bluetooth audio: open and close the phone's A2DP link, and notice when it drops.

enum A2dpCmd {
    Open(String),
    Close,
}

fn a2dp_thread(rx: Receiver<A2dpCmd>, tx: Sender<Msg>) {
    wasapi::com_init();
    let mut link = winrt::A2dp::default();
    let mut available = false;
    let mut next_scan = Instant::now();
    loop {
        let mut error = None;
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(A2dpCmd::Open(hint)) => {
                let r = link.open(&hint);
                log::line(format!("bluetooth audio open ({hint}): {r:?}"));
                if let Err(e) = r {
                    error = Some(e);
                    available = false; // tells the engine phone audio is still on the phone
                    next_scan = Instant::now() + Duration::from_secs(5);
                }
            }
            Ok(A2dpCmd::Close) => {
                log::line("bluetooth audio closed");
                link.close();
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        let mut now_available = available;
        if link.is_open() && !link.is_live() {
            log::line(format!("bluetooth audio dropped, state {:?}", link.state()));
            link.close();
            now_available = false;
            next_scan = Instant::now() + Duration::from_secs(5);
        } else if Instant::now() >= next_scan {
            now_available = winrt::A2dp::devices().is_ok_and(|d| !d.is_empty());
            next_scan = Instant::now() + Duration::from_secs(10);
        }
        if now_available != available || error.is_some() {
            available = now_available;
            let _ = tx.send(Msg::A2dp { available, error });
        }
    }
}

// ---------------------------------------------------------------------------------------------------------
// The laptop: who has the mic, what is making sound, what media is playing, and which app is in front.

/// Processes whose sound is never "a laptop sound": the audio engine and service hosts (Bluetooth audio from
/// the phone plays through one of them, and must not duck itself).
const NOT_SOUNDS: &[&str] = &["svchost.exe", "audiodg.exe"];
/// Full-screen apps that aren't games.
const NOT_GAMES: &[&str] = &[
    "chrome.exe", "msedge.exe", "firefox.exe", "brave.exe", "opera.exe", "vlc.exe", "explorer.exe",
    "applicationframehost.exe", "powerpnt.exe", "true-phone-link.exe", "mpc-hc64.exe",
];

fn laptop_thread(tx: Sender<Msg>) {
    wasapi::com_init();
    let me = std::process::id();
    let names = |pids: Vec<u32>, skip: &[&str]| -> BTreeSet<String> {
        pids.into_iter()
            .filter(|&p| p != me)
            .filter_map(wasapi::process_name)
            .filter(|n| !skip.contains(&n.to_lowercase().as_str()))
            .collect()
    };
    let (mut mic_users, mut sounds, mut media) = (BTreeSet::new(), BTreeSet::new(), BTreeSet::new());
    let mut fg: (Option<String>, bool) = (None, false);
    let mut n = 0u64;
    let debug = std::env::var_os("TPL_DEBUG").is_some();
    let mut last_debug = String::new();
    loop {
        if debug && n % 4 == 0 {
            let rows = wasapi::default_render().and_then(|(d, _)| wasapi::session_debug(&d)).unwrap_or_default().join(", ");
            let media = winrt::laptop_media().unwrap_or_default().iter().map(|(a, t)| format!("{a} [{t}]")).collect::<Vec<_>>().join(", ");
            let line = format!("render: {rows} | media: {media}");
            if line != last_debug {
                eprintln!("{line}");
                last_debug = line;
            }
        }
        let now_mic = wasapi::find(eCapture, |n| n.contains("CABLE Output"))
            .and_then(|(d, _)| wasapi::active_sessions(&d))
            .map(|p| names(p, &[]))
            .unwrap_or_default();
        diff(&mut mic_users, now_mic, &tx, |app_id, active| Event::MicInUse { active, app_id });

        let now_sounds = wasapi::default_render()
            .and_then(|(d, _)| wasapi::sounding_sessions(&d))
            .map(|p| names(p, NOT_SOUNDS))
            .unwrap_or_default();
        diff(&mut sounds, now_sounds, &tx, |app_id, active| Event::LaptopSound { active, app_id });

        if n % 4 == 0 {
            let now_media: BTreeSet<String> = winrt::laptop_media()
                .unwrap_or_default()
                .into_iter()
                .map(|(app, _)| app)
                .filter(|app| !app.to_lowercase().contains("bluetooth"))
                .collect();
            diff(&mut media, now_media, &tx, |app_id, playing| Event::LaptopMedia { playing, app_id });

            let (exe, full) = winrt::foreground();
            let is_game = full && exe.as_ref().is_some_and(|e| !NOT_GAMES.contains(&e.to_lowercase().as_str()));
            if exe.is_some() && (exe != fg.0 || is_game != fg.1) {
                fg = (exe.clone(), is_game);
                let _ = tx.send(Msg::Laptop(Event::Foreground { exe: exe.unwrap_or_default(), is_game }));
            }
        }
        n += 1;
        thread::sleep(Duration::from_millis(250));
    }
}

fn diff(old: &mut BTreeSet<String>, new: BTreeSet<String>, tx: &Sender<Msg>, mk: impl Fn(String, bool) -> Event) {
    for a in old.difference(&new) {
        let _ = tx.send(Msg::Laptop(mk(a.clone(), false)));
    }
    for a in new.difference(old) {
        let _ = tx.send(Msg::Laptop(mk(a.clone(), true)));
    }
    *old = new;
}
