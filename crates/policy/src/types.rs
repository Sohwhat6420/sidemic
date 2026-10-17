//! Inputs, outputs and the small value types they share.

use serde::{Deserialize, Serialize};

/// Thresholds for the automatic rules. Every timing the engine uses lives here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// The laptop counts as quiet once no media session has played for this long (rule 3).
    pub quiet_before_take_ms: u64,
    /// Laptop media that keeps playing this long while the phone holds the speaker takes it back (rule 4).
    pub sustained_media_ms: u64,
    /// How long after a hand-back the Keep button still works (rule 4).
    pub keep_window_ms: u64,
    /// Silence needed after a short laptop sound before the phone is unducked (rule 5).
    pub duck_release_ms: u64,
    /// After we send PhonePlay, a "phone started playing" report inside this window is our own resume
    /// echoing back, not the user starting something new, so it does not trigger the take rules.
    pub own_resume_echo_ms: u64,
    /// How many decisions the log keeps.
    pub decision_log_len: usize,
    /// Keep presses for the same laptop app before the engine learns "don't hand back for that app".
    pub keeps_to_learn: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            quiet_before_take_ms: 2_000,
            sustained_media_ms: 3_000,
            keep_window_ms: 10_000,
            duck_release_ms: 600,
            own_resume_echo_ms: 5_000,
            decision_log_len: 50,
            keeps_to_learn: 2,
        }
    }
}

/// Microphone preference. `Auto` and `Phone` both use the phone mic while a laptop app wants a mic and
/// the link is up, falling back to the laptop mic when it is down; `Laptop` always uses the laptop mic.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MicMode {
    #[default]
    Auto,
    Phone,
    Laptop,
}

/// Where phone media should play. `Phone` = it stays on the phone; `Laptop` = always on the speaker.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioMode {
    #[default]
    Auto,
    Phone,
    Laptop,
}

/// Which microphone the laptop should be using. `None` means idle: nobody is using a mic.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MicRoute {
    Phone,
    Laptop,
    #[default]
    None,
}

/// Where the phone's media audio is going: its own speaker, or the speaker over Bluetooth A2DP.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Placement {
    #[default]
    Phone,
    /// The laptop's default speaker. Memory saved before the rename says "pebble".
    #[serde(alias = "pebble")]
    Speaker,
}

impl Placement {
    pub fn opposite(self) -> Self {
        match self {
            Placement::Phone => Placement::Speaker,
            Placement::Speaker => Placement::Phone,
        }
    }
}

/// Everything the app reports to the engine. Each event is handed over with its timestamp
/// (`Engine::handle(now_ms, event)`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// A Windows media session (GSMTC) started or stopped. The phone's own Bluetooth session is
    /// filtered out by the app before it gets here.
    LaptopMedia { playing: bool, app_id: String },
    /// A non-media audio session on the speaker became active or inactive (a Discord ping, a notification).
    LaptopSound { active: bool, app_id: String },
    /// The phone's media state, with the package that owns it.
    PhoneMedia { playing: bool, package: String },
    /// A laptop app opened or closed a microphone.
    MicInUse { active: bool, app_id: String },
    /// The foreground app changed. `is_game` covers full-screen D3D apps and exes on the game list.
    Foreground { exe: String, is_game: bool },
    /// Whether the phone is reachable for mic and control (Wi-Fi/adb), and whether Bluetooth audio can open.
    PhoneLink { mic_link: bool, a2dp_available: bool },
    /// Nothing happened; lets timers fire.
    Tick,
    SetMicMode { mode: MicMode },
    SetAudioMode { mode: AudioMode },
    SetSmartAuto { on: bool },
    /// Send phone audio back to the phone now.
    HandBack,
    /// Cancel a pending or just-made hand-back and keep phone audio on the speaker.
    Keep,
    /// The user says a logged decision was wrong: undo it now and remember the opposite.
    ThatsWrong { decision_id: u64 },
    /// The user's answer to `Action::AskAboutGame`.
    AnswerGame { exe: String, keep_on_phone: bool },
}

/// What the app should do. Actions are returned in the order they should run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    UseMic { route: MicRoute },
    OpenA2dp,
    CloseA2dp,
    PhonePlay,
    PhonePause,
    DuckPhone,
    UnduckPhone,
    AskAboutGame { exe: String },
    Toast { text: String, offers_keep: bool },
}

/// What kind of automatic decision was made.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    /// Phone audio was moved onto the speaker.
    #[serde(alias = "moved_to_pebble")]
    MovedToSpeaker,
    /// Phone media started but was left on the phone.
    KeptOnPhone,
    /// Phone audio was moved off the speaker back to the phone.
    MovedToPhone,
    /// Laptop media would have taken the speaker back, but phone audio was left there.
    #[serde(alias = "kept_on_pebble")]
    KeptOnSpeaker,
}

/// The situation a decision was made in. A `ThatsWrong` stores a rule under this key, so the next time
/// the same situation comes up the opposite happens.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Situation {
    /// This phone app started media while phone audio was on the phone.
    #[serde(alias = "take_pebble")]
    TakeSpeaker { phone_package: String },
    /// This laptop app played long enough to take the speaker back.
    Handback { laptop_app: String },
    /// Phone media started while this game (exe, lower case) was in the foreground.
    Game { exe: String },
}

/// One entry in the decision log the UI shows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    pub id: u64,
    pub at_ms: u64,
    pub kind: DecisionKind,
    /// What happened, e.g. "Moved TikTok to the speaker".
    pub text: String,
    /// Why, e.g. "The laptop had been quiet for 2 minutes".
    pub reason: String,
    /// Where phone audio ended up because of this decision.
    pub placement: Placement,
    /// The situation key a `ThatsWrong` would learn against; `None` when it cannot be flipped.
    pub situation: Option<Situation>,
    /// Set once the user has undone it (Keep or `ThatsWrong`), so it cannot be flipped twice.
    pub corrected: bool,
}

/// A small snapshot for the UI.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub mic_route: MicRoute,
    pub mic_mode: MicMode,
    pub audio_mode: AudioMode,
    pub smart_auto: bool,
    pub mic_link: bool,
    pub a2dp_available: bool,
    /// Where phone media audio is going.
    pub phone_audio: Placement,
    pub phone_playing: bool,
    pub phone_package: Option<String>,
    /// Laptop apps with a playing media session.
    pub laptop_media: Vec<String>,
    /// Laptop apps with an active short sound.
    pub laptop_sounds: Vec<String>,
    /// Laptop apps that have the mic open.
    pub mic_users: Vec<String>,
    pub phone_ducked: bool,
    /// We paused phone media because a mic went live, and will resume it when the mic is released.
    pub paused_for_mic: bool,
    /// A game the engine has asked about and is waiting on.
    pub waiting_on_game: Option<String>,
    /// Until when (ms) the Keep button on the last hand-back still works.
    pub keep_offer_until: Option<u64>,
}
