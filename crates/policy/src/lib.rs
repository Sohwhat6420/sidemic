//! Sidemic's "Smart auto" policy.
//!
//! The laptop app makes the phone the PC microphone over Wi-Fi and can route the phone's media audio to
//! the laptop's default speaker over Bluetooth A2DP. This crate decides when. It is pure
//! and deterministic: feed [`Engine::handle`] timestamped [`Event`]s and run the [`Action`]s it returns.
//! There are no clocks, threads or I/O in here; send [`Event::Tick`] (ideally at
//! [`Engine::next_deadline`]) so timers can fire.
//!
//! Automatic decisions go into a log ([`Engine::decisions`]) the UI shows, and anything learned from the
//! user lives in [`Memory`], which the app persists as JSON.

mod engine;
mod memory;
mod names;
mod types;

pub use engine::{Engine, DEFAULT_SPEAKER, TOAST_BLUETOOTH_LOST, TOAST_HANDED_BACK};
pub use memory::{Learned, Memory, Rule};
pub use names::{friendly_name, human_duration};
pub use types::{
    Action, AudioMode, Config, Decision, DecisionKind, Event, MicMode, MicRoute, Placement, Situation, State,
};
