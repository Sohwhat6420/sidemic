//! The app's brain. One thread owns the Smart auto engine and everything it decides about; small helper threads
//! keep the phone link up, open and close Bluetooth audio, and watch the laptop (who has the mic, what is playing,
//! which app is in front). They all talk to the brain through one channel, and the UI only ever reads `snapshot`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
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

use crate::adb::{Adb, DeviceState};
use crate::mic::{self, MicStatus, Route};
use crate::phone::{PhoneControl, PhoneEvent};
use crate::{log, wasapi, winrt};

/// Everything the brain hears about.
pub enum Msg {
    Phone(PhoneEvent),
    Link(Link),
    A2dp { available: bool, error: Option<String>, quiet: bool },
    Laptop(Event),
    Act { action: String, arg: Option<String> },
    MicNote(String),
    /// Every phone adb can reach right now as (phone id, model), the phone the user picked (`chosen`) and the one
    /// the link is actually on (`active`: a stand-in while `chosen` is missing and auto-switch is on).
    Roster { ready: Vec<(String, String)>, chosen: Option<String>, active: Option<String> },
    /// Something the user must be told about the phone link (a switch happened, the chosen phone is missing).
    Notice { text: String, buttons: Vec<(String, String, String)> },
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
    /// When the phone he picked isn't reachable, use the other ready phone he used last, say so, and go back when
    /// the picked one returns. Off: it only offers.
    auto_switch: bool,
    /// Extra delay between the speaker recording and the phone audio for echo cancelling, 0–500 ms.
    aec_delay_ms: u32,
    /// While echo cancelling is down: false = the raw phone mic, true = silence.
    aec_fallback_silence: bool,
}

#[derive(Clone, Serialize, Deserialize)]
struct Paused {
    mic_mode: MicMode,
    audio_mode: AudioMode,
    smart_auto: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mic_mode: MicMode::Auto,
            audio_mode: AudioMode::Auto,
            smart_auto: true,
            autostart_set: false,
            paused: None,
            auto_switch: false,
            aec_delay_ms: 0,
            aec_fallback_silence: false,
        }
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
    /// What the phone said about itself in its hello.
    phone: PhoneInfo,
    /// Something the phone refused, explained for its brand (see `refusal_hint`).
    phone_warning: Option<String>,
    /// The phone list as the link thread last reported it.
    roster: Roster,
    /// Each phone's own name (from its hello), by phone id, remembered in `phone-names.json` so the menu can show
    /// phones that aren't connected right now.
    known: BTreeMap<String, String>,
    /// The chosen phone a "not reachable" notice was last shown for, so it appears once per outage.
    notified_missing: Option<String>,
    last_menu: String,
}

struct PhoneRow {
    id: String,
    label: String,
    ready: bool,
    /// The link is on this phone (or, while connecting, going to be).
    active: bool,
    /// The one he picked, which the link returns to when it is reachable.
    chosen: bool,
}

#[derive(Default)]
struct Roster {
    ready: Vec<(String, String)>,
    chosen: Option<String>,
    active: Option<String>,
}

#[derive(Default)]
struct PhoneInfo {
    model: String,
    manufacturer: String,
    bt_name: String,
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
        let data_dir = data_dir.clone();
        thread::Builder::new()
            .name("tpl-adb".into())
            .spawn(move || adb_thread(tx, ctl, ptx, phone_dir, data_dir))
            .unwrap();
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

    let known: BTreeMap<String, String> =
        fs::read_to_string(data_dir.join("phone-names.json")).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    AUTO_SWITCH.store(settings.auto_switch, Ordering::Relaxed);
    crate::echo::DELAY_MS.store(settings.aec_delay_ms.min(crate::echo::MAX_DELAY_MS), Ordering::Relaxed);
    crate::echo::FALLBACK_SILENCE.store(settings.aec_fallback_silence, Ordering::Relaxed);

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
        phone: PhoneInfo::default(),
        phone_warning: None,
        roster: Roster::default(),
        known,
        notified_missing: None,
        last_menu: String::new(),
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
                self.engine.set_speaker_name(&speaker_name(&self.speaker));
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
                PhoneEvent::Hello { name, model, manufacturer, android, bt_name } => {
                    log::line(format!(
                        "phone said hello: {name} ({manufacturer} {model}, Android {android}, Bluetooth name {bt_name:?})"
                    ));
                    self.name = Some(if name.is_empty() { model.clone() } else { name });
                    if let (Some(id), Some(n)) = (self.roster.active.clone(), self.name.clone()) {
                        if self.known.get(&id) != Some(&n) {
                            self.known.insert(id, n);
                            if let Ok(s) = serde_json::to_string_pretty(&self.known) {
                                let _ = fs::write(self.dir.join("phone-names.json"), s);
                            }
                        }
                    }
                    self.phone = PhoneInfo { model, manufacturer, bt_name };
                    self.phone_warning = None;
                    self.link_changed();
                }
                PhoneEvent::Refused { what, error } => {
                    log::line(format!("phone refused {what}: {error}"));
                    self.phone_warning = Some(refusal_hint(&self.phone.manufacturer, &what));
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
            Msg::A2dp { available, error, quiet } => {
                let changed = available != self.a2dp_available;
                if let Some(e) = &error {
                    log::line(format!("bluetooth audio error: {e}"));
                }
                if changed {
                    log::line(format!("bluetooth audio available: {available}"));
                }
                if error.is_some() {
                    self.a2dp_error = error;
                }
                for step in a2dp_report(quiet, changed) {
                    match step {
                        A2dpReport::SpeakerClosed => self.feed(Event::SpeakerLinkClosed),
                        A2dpReport::LinkChanged => {
                            self.a2dp_available = available;
                            self.link_changed();
                        }
                    }
                }
            }
            Msg::Laptop(ev) => self.feed(ev),
            Msg::MicNote(n) => {
                log::line(&n);
                self.mic_note = Some(n);
            }
            Msg::Act { action, arg } => self.act(&action, arg.as_deref()),
            Msg::Roster { ready, chosen, active } => {
                let moved = active != self.roster.active && self.roster.active.is_some();
                self.roster = Roster { ready, chosen, active };
                if moved {
                    // The link moved to another phone: forget what the last one said about itself.
                    self.name = None;
                    self.wallpaper = None;
                    self.phone = PhoneInfo::default();
                    self.phone_warning = None;
                    self.send_a2dp_hint(); // carries the new roster id
                }
                self.check_chosen_missing();
            }
            Msg::Notice { text, buttons } => {
                self.banner = Some(Banner { text, buttons, until: Some(Instant::now() + Duration::from_secs(120)) });
            }
        }
    }

    /// The name to show for a phone: what it called itself, else the model adb saw, else its id.
    fn phone_label(&self, id: &str) -> String {
        self.known
            .get(id)
            .cloned()
            .or_else(|| self.roster.ready.iter().find(|(i, _)| i == id).map(|(_, m)| m.clone()))
            .unwrap_or_else(|| id.to_owned())
    }

    /// Never a silent substitute: when the phone he picked can't be reached and another one can, say so once and
    /// offer it (with auto-switch on, the link thread has already moved and sends its own notice).
    fn check_chosen_missing(&mut self) {
        let Some(chosen) = self.roster.chosen.clone() else { return };
        let chosen_ready = self.roster.ready.iter().any(|(id, _)| *id == chosen);
        if chosen_ready {
            self.notified_missing = None;
            return;
        }
        let other = self.roster.ready.iter().find(|(id, _)| *id != chosen).map(|(id, _)| id.clone());
        if self.settings.auto_switch || self.notified_missing.as_ref() == Some(&chosen) {
            return;
        }
        if let Some(other) = other {
            self.notified_missing = Some(chosen.clone());
            let (c, o) = (self.phone_label(&chosen), self.phone_label(&other));
            log::line(format!("chosen phone {c} isn't reachable; {o} is ready (offered, not switched)"));
            self.banner = Some(Banner {
                text: format!("{c} isn't reachable. {o} is ready."),
                buttons: vec![(format!("Use {o}"), "use_phone".into(), other), ("Not now".into(), "dismiss".into(), String::new())],
                until: Some(Instant::now() + Duration::from_secs(120)),
            });
        }
    }

    /// Which paired Bluetooth source is this phone (see `hint_for`).
    fn a2dp_hint(&self) -> winrt::Hint {
        hint_for(&self.phone.bt_name, self.name.as_deref().unwrap_or(""), &self.phone.model)
    }

    fn send_a2dp_hint(&self) {
        let _ = self.a2dp_tx.send(A2dpCmd::Hint(self.a2dp_hint(), self.roster.active.clone()));
    }

    fn link_changed(&mut self) {
        self.send_a2dp_hint();
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
                self.send_a2dp_hint();
                let _ = self.a2dp_tx.send(A2dpCmd::Open);
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
                        (format!("On the {}", self.engine.speaker_name()), "answer_game".into(), format!("{exe}|speaker")),
                    ],
                    until: None,
                });
            }
            Action::Toast { text, offers_keep } => {
                let keep = self.engine.config().keep_window_ms;
                self.banner = Some(Banner {
                    text,
                    buttons: if offers_keep {
                        vec![(format!("Keep on the {}", self.engine.speaker_name()), "keep".into(), String::new())]
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
        if wants_speaker(action, arg, st.phone_audio) {
            let _ = self.a2dp_tx.send(A2dpCmd::Retry); // don't make him wait out an old backoff
        }
        match action {
            "use_phone" => {
                if let Some(id) = arg {
                    *PICK.lock().unwrap() = Some(id.to_owned());
                    self.banner = None;
                    // A tray tick flips by itself when clicked; redraw the menu from the truth.
                    self.last_json.clear();
                    self.last_menu.clear();
                }
            }
            "aec_delay" => {
                // "+50" / "-50" step it; a plain number sets it.
                let cur = self.settings.aec_delay_ms as i64;
                let next = match arg {
                    Some(a) if a.starts_with('+') || a.starts_with('-') => a.parse::<i64>().ok().map(|d| cur + d),
                    Some(a) => a.parse::<i64>().ok(),
                    None => None,
                };
                if let Some(n) = next {
                    let n = n.clamp(0, crate::echo::MAX_DELAY_MS as i64) as u32;
                    self.settings.aec_delay_ms = n;
                    crate::echo::DELAY_MS.store(n, Ordering::Relaxed);
                    self.save_settings();
                    log::line(format!("echo cancelling extra delay: {n} ms"));
                }
            }
            "aec_fallback" => {
                let silence = arg == Some("silence");
                self.settings.aec_fallback_silence = silence;
                crate::echo::FALLBACK_SILENCE.store(silence, Ordering::Relaxed);
                self.save_settings();
                log::line(format!("when echo cancelling is down: {}", if silence { "silence" } else { "the raw phone mic" }));
            }
            "toggle_auto_switch" => {
                self.settings.auto_switch = !self.settings.auto_switch;
                AUTO_SWITCH.store(self.settings.auto_switch, Ordering::Relaxed);
                self.save_settings();
                log::line(format!("auto-switch to the last-used ready phone: {}", self.settings.auto_switch));
                self.notified_missing = None;
                self.check_chosen_missing();
                self.last_json.clear();
                self.last_menu.clear();
            }
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
                if st.phone_audio == Placement::Speaker {
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

    /// Every phone worth listing: those ready now, the one he picked (even when it is missing), and the one the
    /// link is on, in that order of the roster.
    fn phone_rows(&self) -> Vec<PhoneRow> {
        let mut ids: Vec<String> = self.roster.ready.iter().map(|(id, _)| id.clone()).collect();
        for extra in [&self.roster.chosen, &self.roster.active].into_iter().flatten() {
            if !ids.contains(extra) {
                ids.push(extra.clone());
            }
        }
        ids.into_iter()
            .map(|id| PhoneRow {
                label: self.phone_label(&id),
                ready: self.roster.ready.iter().any(|(i, _)| *i == id),
                active: self.roster.active.as_ref() == Some(&id),
                chosen: self.roster.chosen.as_ref() == Some(&id),
                id,
            })
            .collect()
    }

    /// Builds the UI's state and sends it if anything changed.
    fn publish(&mut self) {
        let st = self.engine.state();
        let connected = self.ctl.connected();
        let (hint, hint_long) = match &self.link {
            Link::NoAdb => ("adb isn't installed".to_owned(), "Sidemic uses adb to reach the phone. Install scrcpy (it includes adb) with: winget install Genymobile.scrcpy".to_owned()),
            Link::Missing => ("Tap Wireless debugging on your phone".to_owned(), "The phone isn't reachable over Wi-Fi. After a restart or a Wi-Fi change, the phone turns Wireless debugging off: swipe down and tap its tile in Quick settings. Both devices must be on the same Wi-Fi.".to_owned()),
            Link::Unauthorised => ("Allow this laptop on your phone".to_owned(), "The phone is asking whether to allow wireless debugging from this laptop on this network. Tap Allow, and tick Always allow on this network.".to_owned()),
            Link::Failed(e) => ("Couldn't start on the phone".to_owned(), format!("Starting Sidemic on the phone failed: {e}. It retries by itself.")),
            Link::Starting | Link::Ready if !connected => ("Connecting…".to_owned(), "Starting Sidemic on the phone. This takes a few seconds.".to_owned()),
            _ => (String::new(), String::new()),
        };

        let phones = self.phone_rows();
        let other_phones: Vec<&PhoneRow> = phones.iter().filter(|p| p.ready && !p.active).collect();
        // The phone he picked isn't here but another is: say which, in the hint and the tray tooltip.
        let hint = match (&self.roster.chosen, other_phones.first()) {
            (Some(c), Some(o)) if !connected && !phones.iter().any(|p| p.id == *c && p.ready) => {
                format!("{} isn't reachable; {} is ready", self.phone_label(c), o.label)
            }
            _ => hint,
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
                    (_, DecisionKind::MovedToSpeaker | DecisionKind::KeptOnSpeaker) => "speaker",
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
                json!({"key": r.id.to_string(), "icon": icon, "text": r.describe(self.engine.speaker_name())})
            })
            .collect();

        let state = json!({
            "connected": connected,
            "link_kind": "wifi",
            "link_text": "Wi-Fi, wireless debugging",
            "hint": hint,
            "hint_long": hint_long,
            "phone_warning": self.phone_warning,
            "other_phones": other_phones.iter().map(|p| json!({"id": p.id, "model": p.label})).collect::<Vec<_>>(),
            "phones": phones.iter().map(|p| json!({"id": p.id, "label": p.label, "ready": p.ready, "active": p.active, "chosen": p.chosen})).collect::<Vec<_>>(),
            "auto_switch": self.settings.auto_switch,
            "aec_delay_ms": self.settings.aec_delay_ms,
            "aec_fallback_silence": self.settings.aec_fallback_silence,
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
                "mode": st.audio_mode, "on_speaker": st.phone_audio == Placement::Speaker, "available": st.a2dp_available,
                "playing": playing, "why": audio_why, "speaker": speaker, "speaker_short": speaker,
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
            let menu = json!([&phones.iter().map(|p| (&p.id, &p.label, p.ready, p.active)).collect::<Vec<_>>(), self.settings.auto_switch]).to_string();
            if menu != self.last_menu {
                self.last_menu = menu;
                crate::set_phones_menu(
                    &self.app,
                    phones.iter().map(|p| crate::PhoneMenuRow { id: p.id.clone(), label: p.label.clone(), ready: p.ready, active: p.active }).collect(),
                    self.settings.auto_switch,
                );
            }
            *self.snapshot.lock().unwrap() = state.clone();
            let _ = self.app.emit("state", state);
        }
    }
}

/// Whether a click asks for the speaker, so a stale Bluetooth backoff should be cut short: the Laptop and Auto
/// modes do (Auto may take it), Phone does not, and the toggle does when it is not on the speaker (its other branch
/// hands the audio back).
fn wants_speaker(action: &str, arg: Option<&str>, phone_audio: Placement) -> bool {
    match action {
        "audio_mode" => parse_mode(arg) != Some(AudioMode::Phone),
        "toggle_audio" => phone_audio != Placement::Speaker,
        _ => false,
    }
}

/// What the brain does with a report from the Bluetooth thread, in order. `quiet`: the thread closed the link on
/// purpose for a phone switch, so phone audio goes back to the phone without the "dropped" toast, and that comes
/// BEFORE the availability change (which would otherwise raise the toast). `changed`: availability differs from what
/// the engine last heard.
#[derive(Debug, PartialEq, Eq)]
enum A2dpReport {
    SpeakerClosed,
    LinkChanged,
}

fn a2dp_report(quiet: bool, changed: bool) -> Vec<A2dpReport> {
    let mut steps = Vec::new();
    if quiet {
        steps.push(A2dpReport::SpeakerClosed);
    }
    if changed {
        steps.push(A2dpReport::LinkChanged);
    }
    steps
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

/// What to say when the phone refuses something the shell user is normally allowed to do. Some brands put it
/// behind a security switch of their own in Developer options.
fn refusal_hint(manufacturer: &str, what: &str) -> String {
    let m = manufacturer.to_lowercase();
    let fix = if ["xiaomi", "redmi", "poco"].iter().any(|b| m.contains(b)) {
        "On Xiaomi, Redmi and POCO phones, turn on Developer options → USB debugging (Security settings). \
         The phone may ask for a SIM card and a Mi account the first time."
    } else if ["oppo", "realme", "oneplus"].iter().any(|b| m.contains(b)) {
        "On OPPO, realme and OnePlus phones, turn on Developer options → Disable permission monitoring."
    } else {
        "Look in Developer options for a security switch that limits USB debugging, and turn it on."
    };
    format!("The phone refused {what}. {fix}")
}

fn tooltip(connected: bool, hint: &str, live: &str, playing: &Option<String>, audio: Placement) -> String {
    let mut t = String::from("Sidemic");
    if !connected {
        t += &format!("\n{hint}");
        return t;
    }
    if live != "off" {
        t += &format!("\nMic: {live}");
    }
    if let Some(p) = playing {
        t += &format!("\n{p} on the {}", if audio == Placement::Speaker { "speaker" } else { "phone" });
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

/// The phone this laptop uses, kept in `phone.json` so that a second phone on the same Wi-Fi is never taken over by
/// itself, plus the address that last worked, for reconnecting when mDNS goes stale.
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Remembered {
    phone: Option<String>,
    addr: Option<String>,
    /// When each phone was last in use (unix seconds), for auto-switch's "the one he used last".
    used: BTreeMap<String, u64>,
}

/// "Use this phone" from the tray or the Connection page; the adb thread picks it up.
static PICK: Mutex<Option<String>> = Mutex::new(None);

/// The auto-switch setting, mirrored here for the adb thread.
static AUTO_SWITCH: AtomicBool = AtomicBool::new(false);

/// How long the chosen phone must be missing before auto-switch moves to another (adb's mDNS blips for seconds).
const SWITCH_AFTER: Duration = Duration::from_secs(15);
/// How often the phone list is refreshed while the phone is connected (each refresh is one `adb devices`).
const ROSTER_EVERY: Duration = Duration::from_secs(8);

fn adb_thread(tx: Sender<Msg>, ctl: PhoneControl, ptx: Sender<PhoneEvent>, phone_dir: PathBuf, data_dir: PathBuf) {
    let Some(adb) = Adb::find(None) else {
        let _ = tx.send(Msg::Link(Link::NoAdb));
        return;
    };
    adb.start_daemon();
    let adb = ADB.get_or_init(|| adb);
    let file = data_dir.join("phone.json");
    let mut mine: Remembered = fs::read_to_string(&file).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    let save = |m: &Remembered| {
        if let Ok(s) = serde_json::to_string_pretty(m) {
            let _ = fs::write(&file, s);
        }
    };
    if let Ok(p) = std::env::var("TPL_PHONE") {
        mine.phone = Some(p);
    }
    let mut server: Option<(String, Child)> = None;
    let mut fails = 0;
    let mut next_reconnect = Instant::now();
    // While the phone he picked (`mine.phone`) is missing and auto-switch is on, the link runs on this one instead.
    let mut stand_in: Option<String> = None;
    let mut missing_since: Option<Instant> = None;
    let mut last_roster = (Vec::new(), None, None);
    let mut next_roster = Instant::now();
    let mut next_used_save = Instant::now();
    // Stops the running server and drops the control channel, so the next pass starts on the phone now wanted.
    let drop_link = |server: &mut Option<(String, Child)>| {
        if let Some((s, mut c)) = server.take() {
            adb.stop_server(&s);
            let _ = c.kill();
        }
        ctl.disconnect();
    };
    loop {
        if let Some(p) = PICK.lock().unwrap().take() {
            if stand_in.as_deref() == Some(p.as_str()) {
                // He picked the phone the link is already standing in on: it becomes his phone, no restart.
                log::line(format!("phone picked: {p} (already in use)"));
                mine.phone = Some(p);
                mine.addr = None;
                stand_in = None;
                save(&mine);
            } else if mine.phone.as_deref() != Some(p.as_str()) || stand_in.is_some() {
                log::line(format!("phone picked: {p}"));
                mine.phone = Some(p);
                mine.addr = None;
                stand_in = None;
                missing_since = None;
                save(&mine);
                drop_link(&mut server);
            }
        }
        let listed = if ctl.connected() && Instant::now() < next_roster {
            thread::sleep(Duration::from_secs(2));
            continue;
        } else {
            next_roster = Instant::now() + ROSTER_EVERY;
            adb.devices()
        };
        let ready = crate::adb::ready_phones(&listed);
        let unix = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());

        // Auto-switch: the picked phone can't be reached, so use the one he used last; go back when it returns.
        let chosen_ready = mine.phone.as_ref().is_some_and(|c| ready.iter().any(|(id, _)| id == c));
        if let Some(chosen) = mine.phone.clone() {
            if chosen_ready {
                missing_since = None;
                if let Some(s) = stand_in.take() {
                    log::line(format!("auto-switch: {chosen} is back, returning to it from {s}"));
                    let _ = tx.send(Msg::Notice {
                        text: "Your phone is back, so Sidemic has switched to it.".into(),
                        buttons: vec![("OK".into(), "dismiss".into(), String::new())],
                    });
                    drop_link(&mut server);
                }
            } else if AUTO_SWITCH.load(Ordering::Relaxed) {
                let since = *missing_since.get_or_insert_with(Instant::now);
                let current_ok = stand_in.as_ref().is_some_and(|s| ready.iter().any(|(id, _)| id == s));
                if !current_ok && since.elapsed() >= SWITCH_AFTER {
                    let next = crate::adb::stand_in(&ready, &chosen, &mine.used);
                    if next != stand_in {
                        match &next {
                            Some(n) => {
                                log::line(format!("auto-switch: {chosen} isn't reachable, using {n}"));
                                let _ = tx.send(Msg::Notice {
                                    text: "Your phone isn't reachable, so Sidemic is using the other ready phone. It goes back when yours returns.".into(),
                                    buttons: vec![("OK".into(), "dismiss".into(), String::new())],
                                });
                            }
                            None => log::line(format!("auto-switch: {chosen} isn't reachable and no other phone is ready")),
                        }
                        stand_in = next;
                        drop_link(&mut server);
                    }
                }
            } else {
                missing_since = None;
                if stand_in.take().is_some() {
                    log::line("auto-switch turned off: back to the phone he picked");
                    drop_link(&mut server);
                }
            }
        }

        let want = stand_in.clone().or_else(|| mine.phone.clone());
        let current = server.as_ref().map(|(s, _)| s.as_str());
        let (state, _) = crate::adb::choose(&listed, want.as_deref(), current);
        let active = match &state {
            DeviceState::Ready(s) => Some(crate::adb::phone_id(s)),
            _ => None,
        };
        let roster = (ready.clone(), mine.phone.clone().or_else(|| active.clone()), active.clone());
        if roster != last_roster {
            let _ = tx.send(Msg::Roster { ready, chosen: roster.1.clone(), active: roster.2.clone() });
            last_roster = roster;
        }
        if ctl.connected() {
            // Remember when this phone was last in use, at most once a minute.
            if let Some(id) = &active {
                if Instant::now() >= next_used_save {
                    next_used_save = Instant::now() + Duration::from_secs(60);
                    mine.used.insert(id.clone(), unix);
                    save(&mine);
                }
            }
            continue;
        }
        match state {
            DeviceState::Ready(serial) => {
                let _ = tx.send(Msg::Link(Link::Starting));
                let id = crate::adb::phone_id(&serial);
                if stand_in.is_none() && mine.phone.as_deref() != Some(id.as_str()) {
                    log::line(format!("using phone {id} ({serial})"));
                    mine.phone = Some(id.clone());
                    save(&mine);
                }
                if stand_in.is_none() {
                    if let Some(addr) = adb.address(&serial) {
                        if mine.addr.as_deref() != Some(addr.as_str()) {
                            log::line(format!("phone listens on {addr}"));
                            mine.addr = Some(addr);
                            save(&mine);
                        }
                    }
                }
                mine.used.insert(id, unix);
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
                // adb's mDNS auto-connect can hold on to a stale instance (HyperOS re-advertises on a new port
                // and keeps the old one), so every 15 s try the advertised addresses and the last one that worked.
                if let Some(phone) = mine.phone.as_deref().filter(|_| Instant::now() >= next_reconnect) {
                    next_reconnect = Instant::now() + Duration::from_secs(15);
                    let ok = adb.reconnect(phone, mine.addr.as_deref());
                    if !ok.is_empty() {
                        log::line(format!("reconnected to the phone directly at {}", ok.join(", ")));
                    }
                }
            }
        }
        thread::sleep(Duration::from_secs(2));
    }
}

// ---------------------------------------------------------------------------------------------------------
// Bluetooth audio: open and close the phone's A2DP link, and notice when it drops.

/// The names a phone is looked for under among the paired Bluetooth sources. Windows lists the phone under its
/// Bluetooth name, which can differ from the name in Settings (on HyperOS it does by default), so a known Bluetooth
/// name is the only thing to match. Without one (Bluetooth off when the phone said hello, or an older phone server),
/// the name and model it gave are tried. There is no "the one paired source will do" fallback: it opened the wrong
/// phone on 27 Oct 2026. With no phone introduced at all there is nobody to play to, so nothing matches.
fn hint_for(bt_name: &str, name: &str, model: &str) -> winrt::Hint {
    if !bt_name.is_empty() {
        return winrt::Hint { names: vec![bt_name.to_string()] };
    }
    let mut names: Vec<String> = Vec::new();
    for n in [name, model] {
        if !n.is_empty() && !names.iter().any(|x| x == n) {
            names.push(n.to_string());
        }
    }
    winrt::Hint { names }
}

enum A2dpCmd {
    /// Which phone to look for (its names, and the roster id of the phone in use); "available" means that phone
    /// is paired, not that some phone is.
    Hint(winrt::Hint, Option<String>),
    Open,
    Close,
    /// Look for the phone now, whatever wait a failed open or a drop has left.
    Retry,
}

fn a2dp_thread(rx: Receiver<A2dpCmd>, tx: Sender<Msg>) {
    wasapi::com_init();
    let mut link = winrt::A2dp::default();
    let bt_watch = winrt::BtWatch::start().map_err(|e| log::line(format!("bluetooth device watcher didn't start: {e}"))).ok();
    // Let the watcher report the devices that are already there before the first check.
    thread::sleep(Duration::from_millis(500));
    let mut hint = winrt::Hint::default();
    // The decisions (what is available, how long to wait after a failure, when to close for a switch) are in
    // a2dp_sm, where they are tested; this loop only carries them out.
    let mut sm = crate::a2dp_sm::Machine::new(Instant::now());
    let rescan = Duration::from_secs(if bt_watch.is_some() { 2 } else { 60 });
    loop {
        let before = sm.available;
        let mut error = None;
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(A2dpCmd::Hint(h, phone)) => {
                let changed = h != hint;
                if changed {
                    log::line(format!("bluetooth audio looks for {:?}", h.names));
                }
                let open_matches = link.is_open() && h.matches(link.open_name());
                if sm.on_hint(Instant::now(), phone, h.names.is_empty(), changed, link.is_open(), open_matches) {
                    log::line(format!("bluetooth audio closed: it was {}, not the phone now in use", link.open_name()));
                    link.close();
                }
                hint = h;
            }
            Ok(A2dpCmd::Open) => {
                let was_open = link.is_open();
                let r = link.open(&hint);
                log::line(format!("bluetooth audio open ({:?}): {r:?}", hint.names));
                sm.on_open(
                    Instant::now(),
                    match (&r, was_open) {
                        (Err(_), _) => crate::a2dp_sm::Open::Failed,
                        (Ok(_), true) => crate::a2dp_sm::Open::AlreadyOpen,
                        (Ok(_), false) => crate::a2dp_sm::Open::Opened,
                    },
                );
                error = r.err();
            }
            Ok(A2dpCmd::Close) => {
                log::line("bluetooth audio closed");
                link.close();
                sm.on_close();
            }
            Ok(A2dpCmd::Retry) => sm.on_retry(Instant::now()),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        let (open, live) = (link.is_open(), link.is_open() && link.is_live());
        let t = sm.tick(Instant::now(), open, live, rescan, || match &bt_watch {
            Some(w) => w.has(&hint),
            // No watcher (it failed to start): fall back to a full search once a minute.
            None => winrt::A2dp::devices().is_ok_and(|d| winrt::pick_device(d, &hint).is_ok()),
        });
        if t.dropped {
            log::line(format!("bluetooth audio dropped, state {:?}", link.state()));
            link.close();
        }
        if sm.available != before || error.is_some() || t.switched {
            // `switched`: the engine is told the speaker link is gone without the "dropped" toast.
            let _ = tx.send(Msg::A2dp { available: sm.available, error, quiet: t.switched });
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
    let mut watch = wasapi::Watch::default();
    // The media check gets its own thread: Windows' media service has been seen to stop answering for good
    // (16 Oct), and that must only freeze media detection, never the mic and sound watchers.
    let media_now: Arc<Mutex<(Vec<String>, Instant)>> = Arc::new(Mutex::new((Vec::new(), Instant::now())));
    {
        let media_now = media_now.clone();
        thread::Builder::new()
            .name("tpl-media".into())
            .spawn(move || {
                wasapi::com_init();
                let mut media_watch = winrt::MediaWatch::default();
                loop {
                    let playing = media_watch.playing();
                    *media_now.lock().unwrap() = (playing, Instant::now());
                    thread::sleep(Duration::from_secs(1));
                }
            })
            .unwrap();
    }
    let mut media_stuck_logged = false;
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
        let now_mic = names(watch.mic_users(), &[]);
        diff(&mut mic_users, now_mic, &tx, |app_id, active| Event::MicInUse { active, app_id });

        let now_sounds = names(watch.sounding(), NOT_SOUNDS);
        diff(&mut sounds, now_sounds, &tx, |app_id, active| Event::LaptopSound { active, app_id });

        if n % 4 == 0 {
            let (playing, at) = media_now.lock().unwrap().clone();
            let stuck = at.elapsed() > Duration::from_secs(30);
            if stuck != media_stuck_logged {
                media_stuck_logged = stuck;
                log::line(if stuck { "Windows' media service isn't answering; laptop media can't be seen until it does" } else { "Windows' media service is answering again" });
            }
            let now_media: BTreeSet<String> =
                playing.into_iter().filter(|app| !app.to_lowercase().contains("bluetooth")).collect();
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

#[cfg(test)]
mod tests {
    use super::{hint_for, refusal_hint};

    #[test]
    fn a_known_bluetooth_name_is_the_only_thing_looked_for() {
        let h = hint_for("POCO X6 Pro 5G", "Ethan's POCO", "23122PCD1G");
        assert_eq!(h.names, vec!["POCO X6 Pro 5G".to_string()]);
    }

    #[test]
    fn without_a_bluetooth_name_the_name_and_model_are_tried_and_no_lone_device_stands_in() {
        // HyperOS sends bt:"" when Bluetooth is off: the S24 (the only paired source) must still not match
        let h = hint_for("", "POCO X6 Pro", "23122PCD1G");
        assert_eq!(h.names, vec!["POCO X6 Pro".to_string(), "23122PCD1G".to_string()]);
        assert_eq!(h.pick(&["Ethan's Galaxy S24 Ultra"]), None);
        assert_eq!(h.pick(&["Ethan's Galaxy S24 Ultra", "POCO X6 Pro 5G"]), Some(1));
    }

    #[test]
    fn before_any_hello_nothing_matches() {
        let h = hint_for("", "", "");
        assert!(h.names.is_empty());
        assert_eq!(h.pick(&["Ethan's Galaxy S24 Ultra"]), None);
        assert_eq!(hint_for("", "Same", "Same").names.len(), 1);
    }

    #[test]
    fn a_hint_matches_the_source_it_names_and_not_another() {
        // Only the name match. Whether an open link is kept or closed is decided in a2dp_sm (tested there).
        let poco = hint_for("POCO X6 Pro 5G", "", "");
        assert!(poco.matches("POCO X6 Pro 5G"));
        assert!(!poco.matches("Ethan's Galaxy S24 Ultra"));
    }

    use super::{a2dp_report, parse_mode, wants_speaker, A2dpReport};
    use tpl_policy::{Action, AudioMode, Config, Engine, Event, Memory, Placement, TOAST_BLUETOOTH_LOST};

    #[test]
    fn only_a_click_that_wants_the_speaker_cuts_the_backoff_short() {
        for (action, arg, on, want) in [
            ("audio_mode", Some("laptop"), Placement::Phone, true),
            ("audio_mode", Some("auto"), Placement::Phone, true),
            ("audio_mode", None, Placement::Phone, true), // unparsed = Auto
            ("audio_mode", Some("phone"), Placement::Speaker, false),
            ("toggle_audio", None, Placement::Phone, true), // would open the speaker
            ("toggle_audio", None, Placement::Speaker, false), // hands back
            ("volume", Some("0.5"), Placement::Phone, false),
            ("hand_back", None, Placement::Speaker, false),
        ] {
            assert_eq!(wants_speaker(action, arg, on), want, "{action} {arg:?} {on:?}");
        }
        assert_eq!(parse_mode(Some("phone")), Some(AudioMode::Phone));
    }

    /// Runs the brain's reaction to a Bluetooth report through the real engine, with phone audio on the speaker.
    fn toasts_after(quiet: bool) -> (Vec<String>, Placement) {
        let mut e = Engine::new(Config::default(), Memory::default());
        e.handle(0, Event::PhoneLink { mic_link: true, a2dp_available: true });
        let opened = e.handle(1_000, Event::PhoneMedia { playing: true, package: "com.zhiliaoapp.musically".into() });
        assert!(opened.contains(&Action::OpenA2dp), "setup: phone media should take the speaker: {opened:?}");
        let mut toasts = Vec::new();
        for step in a2dp_report(quiet, true) {
            let ev = match step {
                A2dpReport::SpeakerClosed => Event::SpeakerLinkClosed,
                A2dpReport::LinkChanged => Event::PhoneLink { mic_link: true, a2dp_available: false },
            };
            for a in e.handle(3_000, ev) {
                if let Action::Toast { text, .. } = a {
                    toasts.push(text);
                }
            }
        }
        (toasts, e.state().phone_audio)
    }

    #[test]
    fn a_phone_switch_goes_back_to_the_phone_quietly_and_a_real_drop_still_says_so() {
        let (toasts, audio) = toasts_after(true);
        assert!(toasts.is_empty(), "a deliberate switch must not toast: {toasts:?}");
        assert_eq!(audio, Placement::Phone);
        // the same report from a real drop (not quiet) is the old behaviour: back on the phone, with the toast
        let (toasts, audio) = toasts_after(false);
        assert_eq!(toasts, vec![TOAST_BLUETOOTH_LOST.to_string()]);
        assert_eq!(audio, Placement::Phone);
    }

    #[test]
    fn the_report_order_puts_the_quiet_close_before_the_availability_change() {
        assert_eq!(a2dp_report(true, true), vec![A2dpReport::SpeakerClosed, A2dpReport::LinkChanged]);
        assert_eq!(a2dp_report(true, false), vec![A2dpReport::SpeakerClosed]);
        assert_eq!(a2dp_report(false, true), vec![A2dpReport::LinkChanged]);
        assert!(a2dp_report(false, false).is_empty());
    }

    #[test]
    fn refusals_name_the_brands_switch() {
        assert!(refusal_hint("Xiaomi", "media control").contains("USB debugging (Security settings)"));
        assert!(refusal_hint("OnePlus", "media control").contains("Disable permission monitoring"));
        assert!(refusal_hint("samsung", "media control").starts_with("The phone refused media control."));
    }
}
