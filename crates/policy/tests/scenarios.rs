//! Table-driven scenario tests: each case is a list of (time, event, exact actions expected), plus a
//! final check on the engine's log, memory and state.

use tpl_policy::*;

const TIKTOK: &str = "com.zhiliaoapp.musically";
const SPOTIFY: &str = "com.spotify.music";
const CHROME: &str = "Chrome";
const DISCORD: &str = "Discord.exe";
const GAME: &str = "eldenring.exe";

#[derive(Clone)]
struct Step {
    at: u64,
    ev: Event,
    want: Vec<Action>,
}

fn st(at: u64, ev: Event, want: Vec<Action>) -> Step {
    Step { at, ev, want }
}

struct Case {
    name: &'static str,
    memory: Memory,
    steps: Vec<Step>,
    check: fn(&Engine),
}

fn case(name: &'static str, steps: Vec<Step>, check: fn(&Engine)) -> Case {
    Case { name, memory: Memory::default(), steps, check }
}

fn run(cases: Vec<Case>) {
    for c in cases {
        let mut e = Engine::new(Config::default(), c.memory);
        for (i, s) in c.steps.into_iter().enumerate() {
            let got = e.handle(s.at, s.ev.clone());
            assert_eq!(got, s.want, "case '{}', step {} at {} ms: {:?}", c.name, i, s.at, s.ev);
        }
        (c.check)(&e);
    }
}

// ---- event and action shorthands ---------------------------------------------------------------

fn link(mic_link: bool, a2dp_available: bool) -> Event {
    Event::PhoneLink { mic_link, a2dp_available }
}
fn laptop(app: &str, playing: bool) -> Event {
    Event::LaptopMedia { playing, app_id: app.into() }
}
fn sound(app: &str, active: bool) -> Event {
    Event::LaptopSound { active, app_id: app.into() }
}
fn phone(pkg: &str, playing: bool) -> Event {
    Event::PhoneMedia { playing, package: pkg.into() }
}
fn mic(app: &str, active: bool) -> Event {
    Event::MicInUse { active, app_id: app.into() }
}
fn fg(exe: &str, is_game: bool) -> Event {
    Event::Foreground { exe: exe.into(), is_game }
}
fn wrong(decision_id: u64) -> Event {
    Event::ThatsWrong { decision_id }
}
fn use_mic(route: MicRoute) -> Action {
    Action::UseMic { route }
}
fn toast_keep() -> Action {
    Action::Toast { text: TOAST_HANDED_BACK.into(), offers_keep: true }
}
use Action::{CloseA2dp, DuckPhone, OpenA2dp, PhonePause, PhonePlay, UnduckPhone};
use Event::{HandBack, Keep, Tick};

/// Link up, laptop idle, TikTok starts at 1 s and takes the speaker (decision 1).
fn on_speaker() -> Vec<Step> {
    vec![st(0, link(true, true), vec![]), st(1_000, phone(TIKTOK, true), vec![OpenA2dp])]
}

/// From `on_speaker`, Chrome plays from 10 s and hands back at 13 s (decision 2).
fn handed_back() -> Vec<Step> {
    [
        on_speaker(),
        vec![
            st(10_000, laptop(CHROME, true), vec![DuckPhone]),
            st(12_000, Tick, vec![]),
            st(13_000, Tick, vec![CloseA2dp, UnduckPhone, PhonePlay, toast_keep()]),
        ],
    ]
    .concat()
}

fn last(e: &Engine) -> &Decision {
    e.decisions().last().expect("a decision")
}

fn rule(e: &Engine, s: Situation) -> Option<(Placement, Learned)> {
    e.memory().rule_for(&s).map(|r| (r.placement, r.learned))
}

fn handback_chrome() -> Situation {
    Situation::Handback { laptop_app: CHROME.into() }
}

// ---- rules 1 and 2: microphone and echo guard --------------------------------------------------

#[test]
fn rule1_2_microphone_and_echo_guard() {
    run(vec![
        case(
            "Discord opens the mic with phone music playing: pause, phone mic, then resume",
            vec![
                st(0, link(true, false), vec![]),
                st(500, phone(SPOTIFY, true), vec![]),
                st(1_000, mic(DISCORD, true), vec![PhonePause, use_mic(MicRoute::Phone)]),
                st(1_100, phone(SPOTIFY, false), vec![]),
                st(60_000, mic(DISCORD, false), vec![use_mic(MicRoute::None), PhonePlay]),
                // Our own resume echoing back is not "the phone started media".
                st(60_200, phone(SPOTIFY, true), vec![]),
            ],
            |e| {
                assert!(e.decisions().is_empty());
                assert!(!e.state().paused_for_mic);
                assert_eq!(e.state().mic_route, MicRoute::None);
            },
        ),
        case(
            "Link drops mid-call -> laptop mic, comes back -> phone mic",
            vec![
                st(0, link(true, false), vec![]),
                st(1_000, mic(DISCORD, true), vec![use_mic(MicRoute::Phone)]),
                st(2_000, link(false, false), vec![use_mic(MicRoute::Laptop)]),
                st(3_000, link(true, false), vec![use_mic(MicRoute::Phone)]),
                st(4_000, mic(DISCORD, false), vec![use_mic(MicRoute::None)]),
            ],
            |_| {},
        ),
        case(
            "Link down when the call starts -> laptop mic so he is never silent",
            vec![st(0, link(false, false), vec![]), st(1_000, mic(DISCORD, true), vec![use_mic(MicRoute::Laptop)])],
            |_| {},
        ),
        case(
            "Mic mode overrides: Laptop pins the laptop, Phone still falls back when the link is down",
            vec![
                st(0, link(true, false), vec![]),
                st(100, Event::SetMicMode { mode: MicMode::Laptop }, vec![]),
                st(1_000, mic(DISCORD, true), vec![use_mic(MicRoute::Laptop)]),
                st(2_000, Event::SetMicMode { mode: MicMode::Phone }, vec![use_mic(MicRoute::Phone)]),
                st(3_000, link(false, false), vec![use_mic(MicRoute::Laptop)]),
                st(4_000, Event::SetMicMode { mode: MicMode::Auto }, vec![]),
                st(5_000, link(true, false), vec![use_mic(MicRoute::Phone)]),
            ],
            |_| {},
        ),
        case(
            "Two apps share the mic; idle only when the last one lets go",
            vec![
                st(0, link(true, false), vec![]),
                st(1_000, mic(DISCORD, true), vec![use_mic(MicRoute::Phone)]),
                st(2_000, mic(GAME, true), vec![]),
                st(3_000, mic(DISCORD, false), vec![]),
                st(4_000, mic(GAME, false), vec![use_mic(MicRoute::None)]),
            ],
            |_| {},
        ),
        case(
            "Phone not playing when the mic goes live -> nothing to resume afterwards",
            vec![
                st(0, link(true, false), vec![]),
                st(1_000, mic(DISCORD, true), vec![use_mic(MicRoute::Phone)]),
                st(2_000, mic(DISCORD, false), vec![use_mic(MicRoute::None)]),
            ],
            |_| {},
        ),
        case(
            "User restarts phone media during the call -> we did not pause it, so no PhonePlay",
            vec![
                st(0, link(true, false), vec![]),
                st(500, phone(SPOTIFY, true), vec![]),
                st(1_000, mic(DISCORD, true), vec![PhonePause, use_mic(MicRoute::Phone)]),
                st(1_100, phone(SPOTIFY, false), vec![]),
                st(9_000, phone(SPOTIFY, true), vec![]),
                st(20_000, mic(DISCORD, false), vec![use_mic(MicRoute::None)]),
            ],
            |_| {},
        ),
        case(
            "Echo guard cannot reach the phone without the link -> no PhonePause",
            vec![
                st(0, link(false, false), vec![]),
                st(500, phone(SPOTIFY, true), vec![]),
                st(1_000, mic(DISCORD, true), vec![use_mic(MicRoute::Laptop)]),
                st(2_000, mic(DISCORD, false), vec![use_mic(MicRoute::None)]),
            ],
            |_| {},
        ),
    ]);
}

// ---- rule 3: idle laptop -> speaker -------------------------------------------------------------

#[test]
fn rule3_idle_laptop_takes_the_speaker() {
    run(vec![
        case("Laptop idle + TikTok starts -> OpenA2dp", on_speaker(), |e| {
            let d = last(e);
            assert_eq!(d.kind, DecisionKind::MovedToSpeaker);
            assert_eq!(d.text, "Moved TikTok to the speaker");
            assert_eq!(d.reason, "The laptop was quiet");
            assert_eq!(e.state().phone_audio, Placement::Speaker);
        }),
        case(
            "Reason names how long the laptop had been quiet",
            vec![
                st(0, link(true, true), vec![]),
                st(1_000, laptop(CHROME, true), vec![]),
                st(5_000, laptop(CHROME, false), vec![]),
                st(125_000, phone(TIKTOK, true), vec![OpenA2dp]),
            ],
            |e| assert_eq!(last(e).reason, "The laptop had been quiet for 2 minutes"),
        ),
        case(
            "Laptop stopped 1 s ago is not quiet yet -> stays on the phone",
            vec![
                st(0, link(true, true), vec![]),
                st(1_000, laptop(CHROME, true), vec![]),
                st(5_000, laptop(CHROME, false), vec![]),
                st(6_000, phone(TIKTOK, true), vec![]),
            ],
            |e| {
                assert_eq!(last(e).kind, DecisionKind::KeptOnPhone);
                assert_eq!(last(e).reason, "The laptop was playing a moment ago");
            },
        ),
        case(
            "Exactly the quiet threshold counts as quiet",
            vec![
                st(0, link(true, true), vec![]),
                st(1_000, laptop(CHROME, true), vec![]),
                st(5_000, laptop(CHROME, false), vec![]),
                st(7_000, phone(TIKTOK, true), vec![OpenA2dp]),
            ],
            |_| {},
        ),
        case(
            "No Bluetooth audio available -> nothing, and nothing logged",
            vec![st(0, link(true, false), vec![]), st(1_000, phone(TIKTOK, true), vec![])],
            |e| assert!(e.decisions().is_empty()),
        ),
        case(
            "Short laptop sounds do not stop the take",
            vec![
                st(0, link(true, true), vec![]),
                st(500, sound(DISCORD, true), vec![]),
                st(1_000, phone(TIKTOK, true), vec![OpenA2dp, DuckPhone]),
            ],
            |_| {},
        ),
    ]);
}

// ---- rule 6: laptop media already playing ------------------------------------------------------

#[test]
fn rule6_laptop_media_playing_keeps_phone_on_phone() {
    run(vec![
        case(
            "YouTube in Chrome playing, TikTok starts -> TikTok stays on the phone",
            vec![
                st(0, link(true, true), vec![]),
                st(1_000, laptop(CHROME, true), vec![]),
                st(2_000, phone(TIKTOK, true), vec![]),
            ],
            |e| {
                let d = last(e);
                assert_eq!(d.kind, DecisionKind::KeptOnPhone);
                assert_eq!(d.text, "Kept TikTok on the phone");
                assert_eq!(d.reason, "Chrome was playing on the laptop");
                assert_eq!(d.situation, Some(Situation::TakeSpeaker { phone_package: TIKTOK.into() }));
            },
        ),
        case(
            "Unknown ids are shown raw",
            vec![
                st(0, link(true, true), vec![]),
                st(1_000, laptop("foobar2000.exe", true), vec![]),
                st(2_000, phone("org.example.radio", true), vec![]),
            ],
            |e| {
                assert_eq!(last(e).text, "Kept org.example.radio on the phone");
                assert_eq!(last(e).reason, "foobar2000.exe was playing on the laptop");
            },
        ),
    ]);
}

// ---- rule 4: sustained laptop media hands back -------------------------------------------------

#[test]
fn rule4_sustained_laptop_media_hands_back() {
    run(vec![
        case(
            "YouTube plays 3 s while on the speaker -> CloseA2dp, PhonePlay, Toast with Keep",
            [
                handed_back(),
                vec![
                    // Android pauses on disconnect, then our PhonePlay echoes back: neither is a new start.
                    st(13_100, phone(TIKTOK, false), vec![]),
                    st(13_200, phone(TIKTOK, true), vec![]),
                ],
            ]
            .concat(),
            |e| {
                let d = last(e);
                assert_eq!(d.id, 2);
                assert_eq!(d.kind, DecisionKind::MovedToPhone);
                assert_eq!(d.text, "Moved TikTok back to the phone");
                assert_eq!(d.reason, "Chrome had been playing for 3 seconds");
                assert_eq!(e.state().phone_audio, Placement::Phone);
                assert_eq!(e.state().keep_offer_until, Some(23_000));
            },
        ),
        case(
            "A late event still fires the hand-back first, at the deadline",
            [
                on_speaker(),
                vec![
                    st(10_000, laptop(CHROME, true), vec![DuckPhone]),
                    st(15_000, laptop(CHROME, false), vec![CloseA2dp, UnduckPhone, PhonePlay, toast_keep()]),
                ],
            ]
            .concat(),
            |e| assert_eq!(last(e).kind, DecisionKind::MovedToPhone),
        ),
        case(
            "Keep inside the window reopens and does not hand back again",
            [
                handed_back(),
                vec![st(15_000, Keep, vec![OpenA2dp]), st(30_000, Tick, vec![])],
            ]
            .concat(),
            |e| {
                assert_eq!(e.state().phone_audio, Placement::Speaker);
                assert_eq!(e.memory().keep_count(CHROME), 1);
                assert!(e.memory().rules().is_empty());
                assert!(e.decisions()[1].corrected);
            },
        ),
        case(
            "Keep after the window does nothing",
            [handed_back(), vec![st(23_001, Keep, vec![])]].concat(),
            |e| assert_eq!(e.memory().keep_count(CHROME), 0),
        ),
        case(
            "Keep while a hand-back is still pending cancels it",
            [
                on_speaker(),
                vec![
                    st(10_000, laptop(CHROME, true), vec![DuckPhone]),
                    st(11_000, Keep, vec![]),
                    st(11_600, Tick, vec![UnduckPhone]),
                    st(20_000, Tick, vec![]),
                ],
            ]
            .concat(),
            |e| assert_eq!(e.memory().keep_count(CHROME), 1),
        ),
        case(
            "Manual HandBack works any time",
            [on_speaker(), vec![st(2_000, HandBack, vec![CloseA2dp, PhonePlay])]].concat(),
            |e| assert_eq!(e.decisions().len(), 1),
        ),
        case(
            "next_deadline points at the hand-back",
            [on_speaker(), vec![st(10_000, laptop(CHROME, true), vec![DuckPhone])]].concat(),
            |e| assert_eq!(e.next_deadline(), Some(13_000)),
        ),
    ]);
}

// ---- rule 5: short sounds duck -----------------------------------------------------------------

#[test]
fn rule5_short_sounds_duck_and_never_hand_back() {
    run(vec![
        case(
            "YouTube plays 1 s -> only a duck, released after 600 ms of silence",
            [
                on_speaker(),
                vec![
                    st(10_000, laptop(CHROME, true), vec![DuckPhone]),
                    st(11_000, laptop(CHROME, false), vec![]),
                    st(11_599, Tick, vec![]),
                    st(11_600, Tick, vec![UnduckPhone]),
                    st(20_000, Tick, vec![]),
                ],
            ]
            .concat(),
            |e| {
                assert_eq!(e.state().phone_audio, Placement::Speaker);
                assert_eq!(e.decisions().len(), 1);
            },
        ),
        case(
            "Discord ping -> duck, a second ping cancels the release, unduck after silence",
            [
                on_speaker(),
                vec![
                    st(10_000, sound(DISCORD, true), vec![DuckPhone]),
                    st(10_200, sound(DISCORD, false), vec![]),
                    st(10_500, sound(DISCORD, true), vec![]),
                    st(10_700, sound(DISCORD, false), vec![]),
                    st(11_200, Tick, vec![]),
                    st(11_300, Tick, vec![UnduckPhone]),
                ],
            ]
            .concat(),
            |_| {},
        ),
        case(
            "A long Discord sound never hands back",
            [
                on_speaker(),
                vec![st(10_000, sound(DISCORD, true), vec![DuckPhone]), st(60_000, Tick, vec![])],
            ]
            .concat(),
            |e| assert_eq!(e.state().phone_audio, Placement::Speaker),
        ),
        case(
            "Pings while phone audio is on the phone do nothing",
            vec![
                st(0, link(true, false), vec![]),
                st(1_000, phone(TIKTOK, true), vec![]),
                st(2_000, sound(DISCORD, true), vec![]),
                st(2_100, sound(DISCORD, false), vec![]),
                st(5_000, Tick, vec![]),
            ],
            |_| {},
        ),
    ]);
}

// ---- rule 7: games -----------------------------------------------------------------------------

#[test]
fn rule7_game_prompt_asked_once_and_remembered() {
    let mut speaker_for_game = Memory::default();
    speaker_for_game.set_rule(Situation::Game { exe: GAME.into() }, Placement::Speaker, Learned::GameAnswer, 0);
    let mut phone_for_game = Memory::default();
    phone_for_game.set_rule(Situation::Game { exe: GAME.into() }, Placement::Phone, Learned::GameAnswer, 0);

    run(vec![
        case(
            "Asked once, kept on the phone while waiting, answer applied now and remembered",
            vec![
                st(0, link(true, true), vec![]),
                st(100, fg("EldenRing.exe", true), vec![]),
                st(1_000, phone(TIKTOK, true), vec![Action::AskAboutGame { exe: GAME.into() }]),
                st(2_000, phone(TIKTOK, false), vec![]),
                st(3_000, phone(TIKTOK, true), vec![]),
                st(4_000, Event::AnswerGame { exe: "EldenRing.exe".into(), keep_on_phone: false }, vec![OpenA2dp]),
                st(5_000, HandBack, vec![CloseA2dp, PhonePlay]),
                st(6_000, phone(TIKTOK, false), vec![]),
                // Outside the resume-echo window, so this is a real start; the remembered answer applies.
                st(20_000, phone(TIKTOK, true), vec![OpenA2dp]),
            ],
            |e| {
                assert_eq!(rule(e, Situation::Game { exe: GAME.into() }), Some((Placement::Speaker, Learned::GameAnswer)));
                assert_eq!(e.state().waiting_on_game, None);
                assert_eq!(last(e).reason, "You chose the speaker while eldenring.exe is open");
                let asks = e.decisions().iter().filter(|d| d.reason.starts_with("Asked")).count();
                assert_eq!(asks, 1);
            },
        ),
        Case {
            name: "Remembered 'keep on phone' -> stays on the phone, no question",
            memory: phone_for_game,
            steps: vec![
                st(0, link(true, true), vec![]),
                st(100, fg(GAME, true), vec![]),
                st(1_000, phone(TIKTOK, true), vec![]),
            ],
            check: |e| {
                assert_eq!(last(e).kind, DecisionKind::KeptOnPhone);
                assert_eq!(last(e).reason, "You chose the phone while eldenring.exe is open");
            },
        },
        Case {
            name: "In a game sharing the speaker, the game's own sounds do not duck the phone",
            memory: speaker_for_game,
            steps: vec![
                st(0, link(true, true), vec![]),
                st(100, fg(GAME, true), vec![]),
                st(1_000, phone(TIKTOK, true), vec![OpenA2dp]),
                st(2_000, sound("EldenRing.exe", true), vec![]),
                st(3_000, sound(DISCORD, true), vec![DuckPhone]),
            ],
            check: |_| {},
        },
        case(
            "Not a game -> the normal rules",
            vec![
                st(0, link(true, true), vec![]),
                st(100, fg("notepad.exe", false), vec![]),
                st(1_000, phone(TIKTOK, true), vec![OpenA2dp]),
            ],
            |_| {},
        ),
    ]);
}

// ---- rule 8: learning --------------------------------------------------------------------------

#[test]
fn rule8_thats_wrong_flips_and_is_remembered() {
    run(vec![
        case(
            "Wrong 'Moved TikTok to the speaker' -> back to the phone now, and next time it stays",
            [
                on_speaker(),
                vec![
                    st(2_000, wrong(1), vec![CloseA2dp, PhonePlay]),
                    st(2_100, phone(TIKTOK, false), vec![]),
                    st(2_200, phone(TIKTOK, true), vec![]),
                    st(30_000, phone(TIKTOK, false), vec![]),
                    st(40_000, phone(TIKTOK, true), vec![]),
                    // A second "that's wrong" on the same decision does nothing.
                    st(41_000, wrong(1), vec![]),
                ],
            ]
            .concat(),
            |e| {
                let s = Situation::TakeSpeaker { phone_package: TIKTOK.into() };
                assert_eq!(rule(e, s), Some((Placement::Phone, Learned::ThatsWrong)));
                assert_eq!(last(e).text, "Kept TikTok on the phone");
                assert_eq!(last(e).reason, "You asked for TikTok to stay on the phone");
            },
        ),
        case(
            "Wrong 'Kept TikTok on the phone' (YouTube playing) -> speaker now, no hand-back, and next time too",
            vec![
                st(0, link(true, true), vec![]),
                st(1_000, laptop(CHROME, true), vec![]),
                st(2_000, phone(TIKTOK, true), vec![]),
                st(3_000, wrong(1), vec![OpenA2dp]),
                st(30_000, Tick, vec![]),
                st(31_000, HandBack, vec![CloseA2dp, PhonePlay]),
                st(32_000, phone(TIKTOK, false), vec![]),
                st(60_000, phone(TIKTOK, true), vec![OpenA2dp]),
            ],
            |e| {
                let s = Situation::TakeSpeaker { phone_package: TIKTOK.into() };
                assert_eq!(rule(e, s), Some((Placement::Speaker, Learned::ThatsWrong)));
                assert_eq!(last(e).reason, "You asked for TikTok to go to the speaker");
            },
        ),
        case(
            "Wrong hand-back -> reopen now and never hand back for Chrome",
            [
                handed_back(),
                vec![
                    st(14_000, wrong(2), vec![OpenA2dp]),
                    st(20_000, laptop(CHROME, false), vec![]),
                    st(30_000, laptop(CHROME, true), vec![]),
                    st(40_000, Tick, vec![]),
                ],
            ]
            .concat(),
            |e| {
                assert_eq!(rule(e, handback_chrome()), Some((Placement::Speaker, Learned::ThatsWrong)));
                assert_eq!(last(e).kind, DecisionKind::KeptOnSpeaker);
                assert_eq!(e.state().phone_audio, Placement::Speaker);
            },
        ),
        case("Unknown decision id -> nothing", [on_speaker(), vec![st(2_000, wrong(99), vec![])]].concat(), |e| {
            assert!(e.memory().rules().is_empty())
        }),
        case(
            "Keep twice for Chrome -> learns 'don't hand back for Chrome'",
            [
                handed_back(),
                vec![
                    st(14_000, Keep, vec![OpenA2dp]),
                    st(20_000, laptop(CHROME, false), vec![]),
                    st(30_000, laptop(CHROME, true), vec![DuckPhone]),
                    st(33_000, Tick, vec![CloseA2dp, UnduckPhone, PhonePlay, toast_keep()]),
                    st(34_000, Keep, vec![OpenA2dp]),
                    st(40_000, laptop(CHROME, false), vec![]),
                    // Learned: Chrome no longer ducks or hands back.
                    st(50_000, laptop(CHROME, true), vec![]),
                    st(53_000, Tick, vec![]),
                    st(90_000, Tick, vec![]),
                ],
            ]
            .concat(),
            |e| {
                assert_eq!(rule(e, handback_chrome()), Some((Placement::Speaker, Learned::KeptRepeatedly)));
                assert_eq!(e.memory().keep_count(CHROME), 0);
                let d = last(e);
                assert_eq!(d.kind, DecisionKind::KeptOnSpeaker);
                assert_eq!(d.text, "Kept TikTok on the speaker");
                assert_eq!(d.reason, "You asked to keep it there when Chrome plays");
            },
        ),
    ]);
}

// ---- rule 9: Smart auto off --------------------------------------------------------------------

#[test]
fn rule9_smart_auto_off_makes_no_a2dp_decisions() {
    run(vec![
        case(
            "Mic rules still run; no automatic A2DP; manual modes and HandBack still work",
            vec![
                st(0, link(true, true), vec![]),
                st(100, Event::SetSmartAuto { on: false }, vec![]),
                st(1_000, phone(TIKTOK, true), vec![]),
                st(2_000, mic(DISCORD, true), vec![PhonePause, use_mic(MicRoute::Phone)]),
                st(3_000, mic(DISCORD, false), vec![use_mic(MicRoute::None), PhonePlay]),
                st(4_000, Event::SetAudioMode { mode: AudioMode::Laptop }, vec![OpenA2dp]),
                st(5_000, Event::SetAudioMode { mode: AudioMode::Auto }, vec![]),
                st(6_000, laptop(CHROME, true), vec![]),
                st(20_000, Tick, vec![]),
                st(21_000, HandBack, vec![CloseA2dp, PhonePlay]),
            ],
            |e| assert!(e.decisions().is_empty()),
        ),
        case(
            "No game question either",
            vec![
                st(0, link(true, true), vec![]),
                st(100, Event::SetSmartAuto { on: false }, vec![]),
                st(200, fg(GAME, true), vec![]),
                st(1_000, phone(TIKTOK, true), vec![]),
            ],
            |e| assert!(e.decisions().is_empty()),
        ),
    ]);
}

// ---- audio modes and rule 10: connectivity -----------------------------------------------------

#[test]
fn audio_modes_and_rule10_connectivity() {
    run(vec![
        case(
            "Audio mode Phone hands back and blocks takes",
            [
                on_speaker(),
                vec![
                    st(2_000, Event::SetAudioMode { mode: AudioMode::Phone }, vec![CloseA2dp, PhonePlay]),
                    st(3_000, phone(TIKTOK, false), vec![]),
                    st(30_000, phone(TIKTOK, true), vec![]),
                ],
            ]
            .concat(),
            |e| assert_eq!(e.decisions().len(), 1),
        ),
        case(
            "Audio mode Laptop opens when Bluetooth appears and never hands back",
            vec![
                st(0, link(true, false), vec![]),
                st(100, Event::SetAudioMode { mode: AudioMode::Laptop }, vec![]),
                st(1_000, link(true, true), vec![OpenA2dp]),
                st(2_000, phone(TIKTOK, true), vec![]),
                st(3_000, laptop(CHROME, true), vec![]),
                st(30_000, Tick, vec![]),
            ],
            |e| {
                assert!(e.decisions().is_empty());
                assert_eq!(e.state().phone_audio, Placement::Speaker);
            },
        ),
        case(
            "Bluetooth drops while on the speaker -> state dropped, no CloseA2dp",
            [
                on_speaker(),
                vec![
                    st(2_000, sound(DISCORD, true), vec![DuckPhone]),
                    st(
                        3_000,
                        link(true, false),
                        vec![UnduckPhone, Action::Toast { text: TOAST_BLUETOOTH_LOST.into(), offers_keep: false }],
                    ),
                    st(4_000, sound(DISCORD, false), vec![]),
                    st(10_000, Tick, vec![]),
                    st(11_000, HandBack, vec![]),
                ],
            ]
            .concat(),
            |e| {
                assert_eq!(e.state().phone_audio, Placement::Phone);
                assert!(!e.state().phone_ducked);
            },
        ),
        case(
            "The app closes the speaker link on purpose (phone switch) -> back on the phone, no 'dropped' toast",
            [
                on_speaker(),
                vec![
                    st(2_000, sound(DISCORD, true), vec![DuckPhone]),
                    st(3_000, Event::SpeakerLinkClosed, vec![UnduckPhone]),
                    st(3_000, link(true, false), vec![]),
                    st(11_000, HandBack, vec![]),
                ],
            ]
            .concat(),
            |e| {
                assert_eq!(e.state().phone_audio, Placement::Phone);
                assert!(!e.state().phone_ducked);
            },
        ),
        case(
            "Closing the speaker link on purpose while nothing is on it is a no-op",
            vec![st(0, link(true, true), vec![]), st(1_000, Event::SpeakerLinkClosed, vec![])],
            |e| assert_eq!(e.state().phone_audio, Placement::Phone),
        ),
        case(
            "Bluetooth gone after a hand-back -> Keep cannot reopen",
            [handed_back(), vec![st(13_500, link(true, false), vec![]), st(14_000, Keep, vec![])]].concat(),
            |e| assert_eq!(e.memory().keep_count(CHROME), 0),
        ),
    ]);
}

// ---- the log, memory and serialisation ---------------------------------------------------------

#[test]
fn decision_log_keeps_the_last_50() {
    let mut e = Engine::new(Config::default(), Memory::default());
    e.handle(0, link(true, true));
    e.handle(1, laptop(CHROME, true));
    for i in 0..60u64 {
        e.handle(10 + i * 10, phone(TIKTOK, true));
        e.handle(15 + i * 10, phone(TIKTOK, false));
    }
    assert_eq!(e.decisions().len(), 50);
    assert_eq!(e.decisions()[0].id, 11);
    assert_eq!(e.decisions()[49].id, 60);
}

#[test]
fn memory_json_round_trip_and_rules_page() {
    let mut m = Memory::new();
    let a = m.set_rule(Situation::TakeSpeaker { phone_package: TIKTOK.into() }, Placement::Phone, Learned::ThatsWrong, 5);
    let b = m.set_rule(handback_chrome(), Placement::Speaker, Learned::KeptRepeatedly, 6);
    m.set_rule(Situation::Game { exe: GAME.into() }, Placement::Speaker, Learned::GameAnswer, 7);
    // Replacing a rule for the same situation keeps one rule.
    let a2 = m.set_rule(Situation::TakeSpeaker { phone_package: TIKTOK.into() }, Placement::Speaker, Learned::ThatsWrong, 8);
    assert_ne!(a, a2);
    assert_eq!(m.rules().len(), 3);

    let json = m.to_json();
    let back = Memory::from_json(&json).expect("round trip");
    assert_eq!(back, m);

    let described: Vec<String> = back.rules().iter().map(|r| r.describe(DEFAULT_SPEAKER)).collect();
    assert_eq!(
        described,
        vec![
            "Phone audio stays on the speaker when Chrome plays",
            "Phone audio may use the speaker while eldenring.exe is open",
            "TikTok moves to the speaker, even when the laptop is playing",
        ]
    );

    let mut back = back;
    assert!(back.delete_rule(b));
    assert!(!back.delete_rule(b));
    assert_eq!(back.rules().len(), 2);

    assert_eq!(Memory::from_json("{}").expect("empty object"), Memory::default());
    assert!(Memory::from_json("not json").is_err());

    // A learned engine memory survives a save and reload into a new engine.
    let mut e = Engine::new(Config::default(), Memory::default());
    for s in [on_speaker(), vec![st(2_000, wrong(1), vec![])]].concat() {
        e.handle(s.at, s.ev);
    }
    let saved = e.memory().to_json();
    let mut e2 = Engine::new(Config::default(), Memory::from_json(&saved).unwrap());
    e2.handle(0, link(true, true));
    assert_eq!(e2.handle(1_000, phone(TIKTOK, true)), vec![]);
    assert_eq!(e2.decisions()[0].reason, "You asked for TikTok to stay on the phone");
}

#[test]
fn texts_use_the_speakers_own_name() {
    let mut e = Engine::new(Config::default(), Memory::default());
    assert_eq!(e.speaker_name(), DEFAULT_SPEAKER);
    e.set_speaker_name("  Desk Speakers ");
    assert_eq!(e.speaker_name(), "Desk Speakers");
    for s in on_speaker() {
        e.handle(s.at, s.ev);
    }
    assert_eq!(last(&e).text, "Moved TikTok to the Desk Speakers");
    let rule = Rule {
        id: 1,
        situation: handback_chrome(),
        placement: Placement::Speaker,
        learned: Learned::KeptRepeatedly,
        at_ms: 0,
    };
    assert_eq!(rule.describe(e.speaker_name()), "Phone audio stays on the Desk Speakers when Chrome plays");
    e.set_speaker_name("");
    assert_eq!(e.speaker_name(), DEFAULT_SPEAKER);
}

#[test]
fn memory_saved_with_the_old_pebble_names_still_loads() {
    let old = r#"{
      "rules": [
        {"id": 1, "situation": {"kind": "take_pebble", "phone_package": "com.zhiliaoapp.musically"},
         "placement": "pebble", "learned": "thats_wrong", "at_ms": 5},
        {"id": 2, "situation": {"kind": "handback", "laptop_app": "Chrome"},
         "placement": "phone", "learned": "kept_repeatedly", "at_ms": 6}
      ],
      "keep_counts": {"powershell.exe": 1},
      "next_rule_id": 3
    }"#;
    let m = Memory::from_json(old).expect("old memory loads");
    let take = Situation::TakeSpeaker { phone_package: TIKTOK.into() };
    assert_eq!(m.rule_for(&take).map(|r| r.placement), Some(Placement::Speaker));
    assert_eq!(m.keep_count("powershell.exe"), 1);
    // It is saved back under the new names.
    let json = m.to_json();
    assert!(!json.contains("pebble"), "{json}");
    assert_eq!(serde_json::from_str::<DecisionKind>(r#""moved_to_pebble""#).unwrap(), DecisionKind::MovedToSpeaker);
    assert_eq!(serde_json::from_str::<DecisionKind>(r#""kept_on_pebble""#).unwrap(), DecisionKind::KeptOnSpeaker);
}

#[test]
fn events_actions_and_state_serialise() {
    let events = vec![
        laptop(CHROME, true),
        Event::SetMicMode { mode: MicMode::Phone },
        Event::AnswerGame { exe: GAME.into(), keep_on_phone: true },
        Tick,
    ];
    for ev in events {
        let json = serde_json::to_string(&ev).unwrap();
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), ev);
    }
    assert_eq!(serde_json::to_string(&use_mic(MicRoute::Phone)).unwrap(), r#"{"type":"use_mic","route":"phone"}"#);
    let mut e = Engine::new(Config::default(), Memory::default());
    for s in on_speaker() {
        e.handle(s.at, s.ev);
    }
    let state = e.state();
    let json = serde_json::to_string(&state).unwrap();
    assert_eq!(serde_json::from_str::<State>(&json).unwrap(), state);
    assert_eq!(state.phone_package.as_deref(), Some(TIKTOK));
}
