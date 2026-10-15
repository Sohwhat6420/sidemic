//! What the engine has learned. The app persists this as JSON and shows it on the Rules page.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::names::friendly_name;
use crate::types::{Placement, Situation};

/// How a rule was learned.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Learned {
    /// The user pressed "That's wrong" on a decision.
    ThatsWrong,
    /// The user pressed Keep after hand-backs for the same laptop app enough times.
    KeptRepeatedly,
    /// The user answered the game question.
    GameAnswer,
}

/// One learned rule: in this situation, phone audio goes here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub id: u64,
    pub situation: Situation,
    pub placement: Placement,
    pub learned: Learned,
    pub at_ms: u64,
}

impl Rule {
    /// A one-line description for the Rules page.
    pub fn describe(&self) -> String {
        match (&self.situation, self.placement) {
            (Situation::TakePebble { phone_package }, Placement::Phone) => {
                format!("{} stays on the phone", friendly_name(phone_package))
            }
            (Situation::TakePebble { phone_package }, Placement::Pebble) => format!(
                "{} moves to the Pebble, even when the laptop is playing",
                friendly_name(phone_package)
            ),
            (Situation::Handback { laptop_app }, Placement::Pebble) => format!(
                "Phone audio stays on the Pebble when {} plays",
                friendly_name(laptop_app)
            ),
            (Situation::Handback { laptop_app }, Placement::Phone) => format!(
                "Phone audio goes back to the phone when {} plays",
                friendly_name(laptop_app)
            ),
            (Situation::Game { exe }, Placement::Pebble) => {
                format!("Phone audio may use the Pebble while {exe} is open")
            }
            (Situation::Game { exe }, Placement::Phone) => {
                format!("Phone audio stays on the phone while {exe} is open")
            }
        }
    }
}

/// Everything learned, serialisable as JSON.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Memory {
    rules: Vec<Rule>,
    /// Keep presses so far per laptop app id, before a rule is learned.
    keep_counts: BTreeMap<String, u32>,
    next_rule_id: u64,
}

impl Memory {
    pub fn new() -> Self {
        Self::default()
    }

    /// Load from JSON written by [`Memory::to_json`].
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        let mut m: Memory = serde_json::from_str(json)?;
        // Repair the id counter if the file was edited by hand.
        if let Some(max) = m.rules.iter().map(|r| r.id).max() {
            if m.next_rule_id <= max {
                m.next_rule_id = max + 1;
            }
        }
        Ok(m)
    }

    /// Save as pretty JSON.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("Memory always serialises")
    }

    /// Every learned rule, oldest first.
    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    pub fn rule_for(&self, situation: &Situation) -> Option<&Rule> {
        self.rules.iter().find(|r| &r.situation == situation)
    }

    /// Store a rule, replacing any rule for the same situation. Returns the rule's id.
    pub fn set_rule(&mut self, situation: Situation, placement: Placement, learned: Learned, at_ms: u64) -> u64 {
        self.rules.retain(|r| r.situation != situation);
        if self.next_rule_id == 0 {
            self.next_rule_id = 1;
        }
        let id = self.next_rule_id;
        self.next_rule_id += 1;
        self.rules.push(Rule { id, situation, placement, learned, at_ms });
        id
    }

    /// Delete a rule by id. Returns whether one was deleted.
    pub fn delete_rule(&mut self, id: u64) -> bool {
        let before = self.rules.len();
        self.rules.retain(|r| r.id != id);
        self.rules.len() != before
    }

    /// Forget every rule and every Keep count.
    pub fn clear(&mut self) {
        self.rules.clear();
        self.keep_counts.clear();
    }

    /// Keep presses recorded so far for a laptop app (reset once a rule is learned).
    pub fn keep_count(&self, laptop_app: &str) -> u32 {
        self.keep_counts.get(laptop_app).copied().unwrap_or(0)
    }

    /// Count one Keep for `laptop_app`; once it reaches `threshold`, learn "don't hand back for that app".
    /// Returns true when a rule was learned.
    pub(crate) fn count_keep(&mut self, laptop_app: &str, threshold: u32, at_ms: u64) -> bool {
        let n = self.keep_counts.entry(laptop_app.to_string()).or_insert(0);
        *n += 1;
        if *n >= threshold {
            self.keep_counts.remove(laptop_app);
            self.set_rule(
                Situation::Handback { laptop_app: laptop_app.to_string() },
                Placement::Pebble,
                Learned::KeptRepeatedly,
                at_ms,
            );
            true
        } else {
            false
        }
    }

    pub(crate) fn reset_keep_count(&mut self, laptop_app: &str) {
        self.keep_counts.remove(laptop_app);
    }
}
