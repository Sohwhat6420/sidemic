//! The Smart auto engine: timestamped events in, actions out. No clocks, threads or I/O.

use std::collections::{BTreeMap, BTreeSet};

use crate::memory::{Learned, Memory};
use crate::names::{friendly_name, human_duration};
use crate::types::{
    Action, AudioMode, Config, Decision, DecisionKind, Event, MicMode, MicRoute, Placement, Situation, State,
};

/// Toast shown after an automatic hand-back (it carries a Keep button).
pub const TOAST_HANDED_BACK: &str = "Phone audio moved back to the phone";
/// Toast shown when Bluetooth audio drops while phone audio was on the Pebble.
pub const TOAST_PEBBLE_LOST: &str = "The Pebble's Bluetooth link dropped, so phone audio is back on the phone";

#[derive(Clone, Debug)]
struct HandbackRecord {
    at_ms: u64,
    laptop_app: String,
    decision_id: u64,
}

/// The policy engine. Feed it events with [`Engine::handle`]; run the returned actions in order.
#[derive(Clone, Debug)]
pub struct Engine {
    cfg: Config,
    mem: Memory,
    log: Vec<Decision>,
    next_decision_id: u64,
    now: u64,

    mic_mode: MicMode,
    audio_mode: AudioMode,
    smart_auto: bool,
    mic_link: bool,
    a2dp_available: bool,

    mic_users: BTreeSet<String>,
    mic_route: MicRoute,
    paused_by_us: bool,

    /// Playing laptop media sessions: app id -> when it started playing (continuously).
    laptop_media: BTreeMap<String, u64>,
    last_media_stop: Option<u64>,
    laptop_sounds: BTreeSet<String>,
    /// Laptop media sessions that must not hand back (Keep, a learned rule, or already playing when the
    /// phone took the Pebble). Cleared per app when its session stops.
    kept_sessions: BTreeSet<String>,

    phone_playing: bool,
    phone_package: Option<String>,
    own_resume_until: Option<u64>,

    phone_audio: Placement,
    ducked: bool,
    duck_release_at: Option<u64>,

    /// Foreground exe (lower case) and whether it is a game.
    foreground: Option<(String, bool)>,
    games_asked: BTreeSet<String>,
    waiting_game: Option<String>,

    last_handback: Option<HandbackRecord>,
}

impl Engine {
    pub fn new(config: Config, memory: Memory) -> Self {
        Self {
            cfg: config,
            mem: memory,
            log: Vec::new(),
            next_decision_id: 1,
            now: 0,
            mic_mode: MicMode::Auto,
            audio_mode: AudioMode::Auto,
            smart_auto: true,
            mic_link: false,
            a2dp_available: false,
            mic_users: BTreeSet::new(),
            mic_route: MicRoute::None,
            paused_by_us: false,
            laptop_media: BTreeMap::new(),
            last_media_stop: None,
            laptop_sounds: BTreeSet::new(),
            kept_sessions: BTreeSet::new(),
            phone_playing: false,
            phone_package: None,
            own_resume_until: None,
            phone_audio: Placement::Phone,
            ducked: false,
            duck_release_at: None,
            foreground: None,
            games_asked: BTreeSet::new(),
            waiting_game: None,
            last_handback: None,
        }
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// The decision log, oldest first, at most `Config::decision_log_len` entries.
    pub fn decisions(&self) -> &[Decision] {
        &self.log
    }

    pub fn memory(&self) -> &Memory {
        &self.mem
    }

    /// For the Rules page (delete a rule, clear everything). Save it afterwards with `Memory::to_json`.
    pub fn memory_mut(&mut self) -> &mut Memory {
        &mut self.mem
    }

    pub fn state(&self) -> State {
        State {
            mic_route: self.mic_route,
            mic_mode: self.mic_mode,
            audio_mode: self.audio_mode,
            smart_auto: self.smart_auto,
            mic_link: self.mic_link,
            a2dp_available: self.a2dp_available,
            phone_audio: self.phone_audio,
            phone_playing: self.phone_playing,
            phone_package: self.phone_package.clone(),
            laptop_media: self.laptop_media.keys().cloned().collect(),
            laptop_sounds: self.laptop_sounds.iter().cloned().collect(),
            mic_users: self.mic_users.iter().cloned().collect(),
            phone_ducked: self.ducked,
            paused_for_mic: self.paused_by_us,
            waiting_on_game: self.waiting_game.clone(),
            keep_offer_until: self.last_handback.as_ref().map(|h| h.at_ms + self.cfg.keep_window_ms),
        }
    }

    /// The earliest time (ms) at which a `Tick` would change something, so the app can schedule one.
    pub fn next_deadline(&self) -> Option<u64> {
        let mut next: Option<u64> = self.duck_release_at;
        if self.handback_eligible() {
            for (app, since) in &self.laptop_media {
                if !self.kept_sessions.contains(app) {
                    let due = since + self.cfg.sustained_media_ms;
                    next = Some(next.map_or(due, |n| n.min(due)));
                }
            }
        }
        next
    }

    /// Feed one event that happened at `now_ms`. Returns the actions to run, in order.
    /// Time never runs backwards: an earlier timestamp is treated as the latest one seen.
    pub fn handle(&mut self, now_ms: u64, event: Event) -> Vec<Action> {
        self.now = self.now.max(now_ms);
        let mut out = Vec::new();
        // Timers that fell due before this event fire first.
        self.run_timers(&mut out);
        match event {
            Event::LaptopMedia { playing, app_id } => self.on_laptop_media(playing, app_id),
            Event::LaptopSound { active, app_id } => {
                if active {
                    self.laptop_sounds.insert(app_id);
                } else {
                    self.laptop_sounds.remove(&app_id);
                }
            }
            Event::PhoneMedia { playing, package } => self.on_phone_media(playing, package, &mut out),
            Event::MicInUse { active, app_id } => self.on_mic(active, app_id, &mut out),
            Event::Foreground { exe, is_game } => self.foreground = Some((exe.to_ascii_lowercase(), is_game)),
            Event::PhoneLink { mic_link, a2dp_available } => self.on_link(mic_link, a2dp_available, &mut out),
            Event::Tick => {}
            Event::SetMicMode { mode } => {
                self.mic_mode = mode;
                self.update_mic_route(&mut out);
            }
            Event::SetAudioMode { mode } => self.on_audio_mode(mode, &mut out),
            Event::SetSmartAuto { on } => self.smart_auto = on,
            Event::HandBack => {
                if self.phone_audio == Placement::Pebble {
                    self.hand_back(&mut out);
                }
            }
            Event::Keep => self.on_keep(&mut out),
            Event::ThatsWrong { decision_id } => self.on_thats_wrong(decision_id, &mut out),
            Event::AnswerGame { exe, keep_on_phone } => self.on_answer_game(exe, keep_on_phone, &mut out),
        }
        self.run_timers(&mut out);
        self.update_duck(&mut out);
        out
    }

    // ---- rule 1 and 2: microphone and echo guard -------------------------------------------------

    fn desired_mic(&self) -> MicRoute {
        if self.mic_users.is_empty() {
            return MicRoute::None;
        }
        match self.mic_mode {
            MicMode::Laptop => MicRoute::Laptop,
            MicMode::Auto | MicMode::Phone => {
                if self.mic_link {
                    MicRoute::Phone
                } else {
                    MicRoute::Laptop
                }
            }
        }
    }

    fn update_mic_route(&mut self, out: &mut Vec<Action>) {
        let want = self.desired_mic();
        if want != self.mic_route {
            self.mic_route = want;
            out.push(Action::UseMic { route: want });
        }
    }

    fn on_mic(&mut self, active: bool, app_id: String, out: &mut Vec<Action>) {
        let was_live = !self.mic_users.is_empty();
        if active {
            self.mic_users.insert(app_id);
        } else {
            self.mic_users.remove(&app_id);
        }
        let live = !self.mic_users.is_empty();

        if !was_live && live && self.phone_playing && self.mic_link {
            out.push(Action::PhonePause);
            self.paused_by_us = true;
        }
        self.update_mic_route(out);
        if was_live && !live && self.paused_by_us {
            self.paused_by_us = false;
            if self.mic_link {
                self.resume_phone(out);
            }
        }
    }

    fn resume_phone(&mut self, out: &mut Vec<Action>) {
        out.push(Action::PhonePlay);
        self.own_resume_until = Some(self.now + self.cfg.own_resume_echo_ms);
    }

    // ---- links and modes -------------------------------------------------------------------------

    fn on_link(&mut self, mic_link: bool, a2dp_available: bool, out: &mut Vec<Action>) {
        let had_a2dp = self.a2dp_available;
        self.mic_link = mic_link;
        self.a2dp_available = a2dp_available;

        // Rule 10: Bluetooth audio went away under us. CloseA2dp cannot run, so just drop the state.
        if !a2dp_available && self.phone_audio == Placement::Pebble {
            self.phone_audio = Placement::Phone;
            self.kept_sessions.clear();
            self.last_handback = None;
            self.release_duck_now(out);
            out.push(Action::Toast { text: TOAST_PEBBLE_LOST.to_string(), offers_keep: false });
        }
        if a2dp_available
            && !had_a2dp
            && self.audio_mode == AudioMode::Laptop
            && self.phone_audio == Placement::Phone
        {
            self.open_pebble(out);
        }
        self.update_mic_route(out);
    }

    fn on_audio_mode(&mut self, mode: AudioMode, out: &mut Vec<Action>) {
        self.audio_mode = mode;
        match mode {
            AudioMode::Phone => {
                if self.phone_audio == Placement::Pebble {
                    self.hand_back(out);
                }
            }
            AudioMode::Laptop => {
                if self.phone_audio == Placement::Phone && self.a2dp_available {
                    self.open_pebble(out);
                }
            }
            AudioMode::Auto => {}
        }
    }

    // ---- moving phone audio ----------------------------------------------------------------------

    fn open_pebble(&mut self, out: &mut Vec<Action>) {
        out.push(Action::OpenA2dp);
        self.phone_audio = Placement::Pebble;
        // Laptop media already playing when the phone takes the Pebble was accepted alongside it,
        // so it must not immediately take the Pebble back.
        let playing: Vec<String> = self.laptop_media.keys().cloned().collect();
        self.kept_sessions.extend(playing);
        self.last_handback = None;
    }

    /// Close A2DP and, since Android pauses on Bluetooth disconnect, resume on the phone's own speaker.
    fn hand_back(&mut self, out: &mut Vec<Action>) {
        out.push(Action::CloseA2dp);
        self.phone_audio = Placement::Phone;
        self.kept_sessions.clear();
        self.last_handback = None;
        self.release_duck_now(out);
        if self.phone_playing && self.mic_link && !self.paused_by_us {
            self.resume_phone(out);
        }
    }

    fn release_duck_now(&mut self, out: &mut Vec<Action>) {
        if self.ducked {
            if self.mic_link {
                out.push(Action::UnduckPhone);
            }
            self.ducked = false;
        }
        self.duck_release_at = None;
    }

    fn phone_name(&self) -> String {
        match &self.phone_package {
            Some(p) => friendly_name(p),
            None => "phone audio".to_string(),
        }
    }

    // ---- rules 3, 6, 7: phone media starts -------------------------------------------------------

    fn on_phone_media(&mut self, playing: bool, package: String, out: &mut Vec<Action>) {
        let was_playing = self.phone_playing;
        let same_package = self.phone_package.as_deref() == Some(package.as_str());
        self.phone_playing = playing;
        if !playing {
            if !package.is_empty() {
                self.phone_package = Some(package);
            }
            return;
        }
        self.phone_package = Some(package.clone());
        let started = !was_playing || !same_package;
        if !started {
            return;
        }
        if let Some(until) = self.own_resume_until.take() {
            if self.now <= until {
                // Our own PhonePlay echoing back, not the user starting something.
                return;
            }
        }
        // The user started phone media during a call: they overrode the echo guard.
        self.paused_by_us = false;
        self.consider_take(&package, out);
    }

    fn laptop_quiet(&self) -> bool {
        self.laptop_media.is_empty()
            && self
                .last_media_stop
                .is_none_or(|t| self.now.saturating_sub(t) >= self.cfg.quiet_before_take_ms)
    }

    /// The laptop media session that has been playing longest, if any.
    fn oldest_laptop_media(&self) -> Option<String> {
        self.laptop_media.iter().min_by_key(|(_, since)| **since).map(|(app, _)| app.clone())
    }

    fn consider_take(&mut self, package: &str, out: &mut Vec<Action>) {
        if self.phone_audio == Placement::Pebble || !self.a2dp_available {
            return;
        }
        match self.audio_mode {
            AudioMode::Phone => return,
            AudioMode::Laptop => {
                self.open_pebble(out);
                return;
            }
            AudioMode::Auto => {}
        }
        if !self.smart_auto {
            return;
        }
        let name = friendly_name(package);
        let moved = format!("Moved {name} to the Pebble");
        let kept = format!("Kept {name} on the phone");

        // Rule 7: a game is in the foreground.
        if let Some((exe, true)) = self.foreground.clone() {
            let situation = Situation::Game { exe: exe.clone() };
            if let Some(rule) = self.mem.rule_for(&situation) {
                match rule.placement {
                    Placement::Pebble => {
                        self.open_pebble(out);
                        let reason = format!("You chose the Pebble while {exe} is open");
                        self.decide(DecisionKind::MovedToPebble, moved, reason, Some(situation), Placement::Pebble);
                    }
                    Placement::Phone => {
                        let reason = format!("You chose the phone while {exe} is open");
                        self.decide(DecisionKind::KeptOnPhone, kept, reason, Some(situation), Placement::Phone);
                    }
                }
            } else if self.games_asked.insert(exe.clone()) {
                self.waiting_game = Some(exe.clone());
                out.push(Action::AskAboutGame { exe: exe.clone() });
                let reason = format!("Asked whether phone audio may use the Pebble while {exe} is open");
                self.decide(DecisionKind::KeptOnPhone, kept, reason, Some(situation), Placement::Phone);
            }
            return;
        }

        let situation = Situation::TakePebble { phone_package: package.to_string() };
        // Rule 8: a learned rule for this phone app wins.
        if let Some(rule) = self.mem.rule_for(&situation) {
            match rule.placement {
                Placement::Pebble => {
                    self.open_pebble(out);
                    let reason = format!("You asked for {name} to go to the Pebble");
                    self.decide(DecisionKind::MovedToPebble, moved, reason, Some(situation), Placement::Pebble);
                }
                Placement::Phone => {
                    let reason = format!("You asked for {name} to stay on the phone");
                    self.decide(DecisionKind::KeptOnPhone, kept, reason, Some(situation), Placement::Phone);
                }
            }
            return;
        }

        // Rule 6: real laptop media is playing, so the phone stays on the phone.
        if let Some(app) = self.oldest_laptop_media() {
            let reason = format!("{} was playing on the laptop", friendly_name(&app));
            self.decide(DecisionKind::KeptOnPhone, kept, reason, Some(situation), Placement::Phone);
            return;
        }
        if !self.laptop_quiet() {
            let reason = "The laptop was playing a moment ago".to_string();
            self.decide(DecisionKind::KeptOnPhone, kept, reason, Some(situation), Placement::Phone);
            return;
        }

        // Rule 3: the laptop is quiet, so take the Pebble.
        self.open_pebble(out);
        let reason = match self.last_media_stop {
            Some(t) => format!("The laptop had been quiet for {}", human_duration(self.now - t)),
            None => "The laptop was quiet".to_string(),
        };
        self.decide(DecisionKind::MovedToPebble, moved, reason, Some(situation), Placement::Pebble);
    }

    // ---- rules 4 and 5: laptop media and sounds while the phone holds the Pebble -----------------

    fn on_laptop_media(&mut self, playing: bool, app_id: String) {
        if playing {
            self.laptop_media.entry(app_id).or_insert(self.now);
        } else if self.laptop_media.remove(&app_id).is_some() {
            self.kept_sessions.remove(&app_id);
            if self.laptop_media.is_empty() {
                self.last_media_stop = Some(self.now);
            }
        }
    }

    fn handback_eligible(&self) -> bool {
        self.phone_audio == Placement::Pebble && self.audio_mode == AudioMode::Auto && self.smart_auto
    }

    fn run_timers(&mut self, out: &mut Vec<Action>) {
        // Rule 4: sustained laptop media takes the Pebble back.
        while self.handback_eligible() {
            let due = self
                .laptop_media
                .iter()
                .filter(|(app, since)| {
                    !self.kept_sessions.contains(*app)
                        && self.now.saturating_sub(**since) >= self.cfg.sustained_media_ms
                })
                .min_by_key(|(_, since)| **since)
                .map(|(app, _)| app.clone());
            let Some(app) = due else { break };
            let app_name = friendly_name(&app);
            let phone_name = self.phone_name();
            let situation = Situation::Handback { laptop_app: app.clone() };
            if self.never_hands_back(&app) {
                self.kept_sessions.insert(app);
                let text = format!("Kept {phone_name} on the Pebble");
                let reason = format!("You asked to keep it there when {app_name} plays");
                self.decide(DecisionKind::KeptOnPebble, text, reason, Some(situation), Placement::Pebble);
                continue;
            }
            self.hand_back(out);
            out.push(Action::Toast { text: TOAST_HANDED_BACK.to_string(), offers_keep: true });
            let text = format!("Moved {phone_name} back to the phone");
            let reason = format!(
                "{app_name} had been playing for {}",
                human_duration(self.cfg.sustained_media_ms)
            );
            let id = self.decide(DecisionKind::MovedToPhone, text, reason, Some(situation), Placement::Phone);
            self.last_handback = Some(HandbackRecord { at_ms: self.now, laptop_app: app, decision_id: id });
            break;
        }

        // Rule 5: unduck after enough silence.
        if let Some(at) = self.duck_release_at {
            if self.now >= at {
                self.duck_release_at = None;
                if self.ducked && !self.duck_sources_active() {
                    self.release_duck_now(out);
                }
            }
        }
    }

    /// A short laptop sound, or laptop media that might yet hand back, is playing over phone audio.
    fn duck_sources_active(&self) -> bool {
        let game = self.foreground.as_ref().filter(|(_, g)| *g).map(|(exe, _)| exe.as_str());
        let sound = self.laptop_sounds.iter().any(|s| Some(s.to_ascii_lowercase().as_str()) != game);
        let pending_media = self.handback_eligible()
            && self.laptop_media.keys().any(|a| !self.kept_sessions.contains(a) && !self.never_hands_back(a));
        sound || pending_media
    }

    /// A learned rule says this laptop app leaves phone audio on the Pebble.
    fn never_hands_back(&self, laptop_app: &str) -> bool {
        self.mem
            .rule_for(&Situation::Handback { laptop_app: laptop_app.to_string() })
            .is_some_and(|r| r.placement == Placement::Pebble)
    }

    fn update_duck(&mut self, out: &mut Vec<Action>) {
        if self.duck_sources_active() {
            self.duck_release_at = None;
            if !self.ducked && self.phone_audio == Placement::Pebble && self.phone_playing && self.mic_link {
                out.push(Action::DuckPhone);
                self.ducked = true;
            }
        } else if self.ducked && self.duck_release_at.is_none() {
            self.duck_release_at = Some(self.now + self.cfg.duck_release_ms);
        }
    }

    // ---- user actions and learning (rule 8) ------------------------------------------------------

    fn on_keep(&mut self, out: &mut Vec<Action>) {
        if let Some(h) = self.last_handback.take() {
            if self.now <= h.at_ms + self.cfg.keep_window_ms
                && self.phone_audio == Placement::Phone
                && self.a2dp_available
            {
                self.open_pebble(out);
                self.kept_sessions.insert(h.laptop_app.clone());
                self.mark_corrected(h.decision_id);
                self.mem.count_keep(&h.laptop_app, self.cfg.keeps_to_learn, self.now);
            }
            return;
        }
        // A hand-back still pending: laptop media is playing but has not reached the threshold.
        if self.handback_eligible() {
            let pending: Vec<String> =
                self.laptop_media.keys().filter(|a| !self.kept_sessions.contains(*a)).cloned().collect();
            for app in pending {
                self.mem.count_keep(&app, self.cfg.keeps_to_learn, self.now);
                self.kept_sessions.insert(app);
            }
        }
    }

    fn on_thats_wrong(&mut self, decision_id: u64, out: &mut Vec<Action>) {
        let Some(d) = self.log.iter().find(|d| d.id == decision_id).cloned() else { return };
        if d.corrected {
            return;
        }
        let Some(situation) = d.situation.clone() else { return };
        self.mark_corrected(decision_id);
        let flipped = d.placement.opposite();
        self.mem.set_rule(situation.clone(), flipped, Learned::ThatsWrong, self.now);
        if let Situation::Handback { laptop_app } = &situation {
            self.mem.reset_keep_count(laptop_app);
        }
        if let Situation::Game { exe } = &situation {
            self.games_asked.remove(exe);
            if self.waiting_game.as_deref() == Some(exe.as_str()) {
                self.waiting_game = None;
            }
        }
        let phone_name = self.phone_name();
        let reason = "You said the earlier decision was wrong".to_string();
        match flipped {
            Placement::Pebble => {
                if self.phone_audio == Placement::Phone && self.a2dp_available {
                    self.open_pebble(out);
                    let text = format!("Moved {phone_name} to the Pebble");
                    self.decide(DecisionKind::MovedToPebble, text, reason, None, Placement::Pebble);
                }
            }
            Placement::Phone => {
                if self.phone_audio == Placement::Pebble {
                    self.hand_back(out);
                    let text = format!("Moved {phone_name} back to the phone");
                    self.decide(DecisionKind::MovedToPhone, text, reason, None, Placement::Phone);
                }
            }
        }
    }

    fn on_answer_game(&mut self, exe: String, keep_on_phone: bool, out: &mut Vec<Action>) {
        let exe = exe.to_ascii_lowercase();
        let placement = if keep_on_phone { Placement::Phone } else { Placement::Pebble };
        let situation = Situation::Game { exe: exe.clone() };
        self.mem.set_rule(situation.clone(), placement, Learned::GameAnswer, self.now);
        self.games_asked.remove(&exe);
        if self.waiting_game.as_deref() == Some(exe.as_str()) {
            self.waiting_game = None;
        }
        let in_game = self.foreground.as_ref().is_some_and(|(fg, g)| *g && *fg == exe);
        if !(in_game && self.phone_playing && self.audio_mode == AudioMode::Auto) {
            return;
        }
        let phone_name = self.phone_name();
        match placement {
            Placement::Pebble => {
                if self.phone_audio == Placement::Phone && self.a2dp_available {
                    self.open_pebble(out);
                    let text = format!("Moved {phone_name} to the Pebble");
                    let reason = format!("You chose the Pebble while {exe} is open");
                    self.decide(DecisionKind::MovedToPebble, text, reason, Some(situation), Placement::Pebble);
                }
            }
            Placement::Phone => {
                if self.phone_audio == Placement::Pebble {
                    self.hand_back(out);
                    let text = format!("Moved {phone_name} back to the phone");
                    let reason = format!("You chose the phone while {exe} is open");
                    self.decide(DecisionKind::MovedToPhone, text, reason, Some(situation), Placement::Phone);
                }
            }
        }
    }

    fn mark_corrected(&mut self, decision_id: u64) {
        if let Some(d) = self.log.iter_mut().find(|d| d.id == decision_id) {
            d.corrected = true;
        }
    }

    fn decide(
        &mut self,
        kind: DecisionKind,
        text: String,
        reason: String,
        situation: Option<Situation>,
        placement: Placement,
    ) -> u64 {
        let id = self.next_decision_id;
        self.next_decision_id += 1;
        self.log.push(Decision { id, at_ms: self.now, kind, text, reason, placement, situation, corrected: false });
        let cap = self.cfg.decision_log_len.max(1);
        if self.log.len() > cap {
            let excess = self.log.len() - cap;
            self.log.drain(..excess);
        }
        id
    }
}
