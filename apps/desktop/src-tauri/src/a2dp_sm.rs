//! The decisions behind the Bluetooth audio thread (`core::a2dp_thread`): when the phone counts as "available",
//! how long to wait after a failed open or a drop, and when a live link must be closed because the phone in use
//! changed. It holds no link, clock or Windows type, so the timing rules can be tested with a made-up clock.

use std::time::{Duration, Instant};

/// Seconds to wait before saying "available" again after a failed open or a drop: 5, 10, ... 120.
const BACKOFF_MIN: u64 = 5;
const BACKOFF_MAX: u64 = 120;
/// A link must stay up this long before its earlier failures are forgotten: one that opens and drops again at once
/// would otherwise be retried (and toasted) every `BACKOFF_MIN` seconds.
const LIVE_FOR_RESET: Duration = Duration::from_secs(60);
/// After a deliberate close for a phone switch, look again this soon.
const SWITCH_RESCAN: Duration = Duration::from_secs(1);

#[derive(Debug)]
pub struct Machine {
    /// Whether the phone in use is paired and reachable (what the engine is told).
    pub available: bool,
    backoff: u64,
    next_scan: Instant,
    /// When the current link opened, until it has been up for `LIVE_FOR_RESET`.
    live_since: Option<Instant>,
    /// The phone the open link was opened for.
    owner: Option<String>,
    /// The phone in use now (the roster's active id).
    phone: Option<String>,
    hint_empty: bool,
    switched: bool,
}

/// How an `A2dpCmd::Open` ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Open {
    Failed,
    /// A new link is up.
    Opened,
    /// A link was already open (a repeated Open): nothing about it changed.
    AlreadyOpen,
}

/// What `tick` found.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Tick {
    /// The link dropped: close it.
    pub dropped: bool,
    /// The link was closed on purpose because the phone changed: tell the engine without the "dropped" toast.
    pub switched: bool,
}

impl Machine {
    pub fn new(now: Instant) -> Machine {
        Machine { available: false, backoff: 0, next_scan: now, live_since: None, owner: None, phone: None, hint_empty: true, switched: false }
    }

    /// The phone to look for was (re)announced. `changed` is whether the names differ from the last announcement,
    /// `link_open` whether a link is open, `open_matches` whether the open link's source still matches the names.
    /// Returns true when the open link must be closed: it belongs to another phone.
    pub fn on_hint(&mut self, now: Instant, phone: Option<String>, hint_empty: bool, changed: bool, link_open: bool, open_matches: bool) -> bool {
        let close = link_open
            && match (&self.owner, &phone) {
                (Some(owner), Some(new)) => owner != new, // the roster's phone changed
                (Some(_), None) => false,                 // adb lost the phone for a moment: the link carries on
                (None, _) => !hint_empty && !open_matches, // opened without knowing the phone: go by the names
            };
        self.phone = phone;
        self.hint_empty = hint_empty;
        // A link opened before the roster said which phone this is: the first announcement that names the phone and
        // matches the open source says whose link it is, so a later switch is judged by id too.
        if link_open && !close && self.owner.is_none() && self.phone.is_some() && open_matches {
            self.owner = self.phone.clone();
        }
        if close {
            self.switched = true;
            self.owner = None;
            self.live_since = None;
        }
        if changed {
            self.backoff = 0;
            self.next_scan = now; // look again now
        }
        close
    }

    /// `A2dpCmd::Open` finished.
    pub fn on_open(&mut self, now: Instant, result: Open) {
        match result {
            Open::Opened => {
                // Earlier failures are only forgotten once the link has stayed up (see `tick`).
                self.owner = self.phone.clone();
                self.live_since = Some(now);
            }
            // A repeated Open must not re-stamp the owner (it could adopt the old phone's link under a new id) or
            // push back the 60 s reset.
            Open::AlreadyOpen => {}
            Open::Failed => {
                self.available = false; // phone audio stays on the phone
                self.fail(now);
            }
        }
    }

    /// The engine closed the link (hand-back): not a failure.
    pub fn on_close(&mut self) {
        self.live_since = None;
        self.owner = None;
        self.backoff = 0;
    }

    /// The user asked for something that needs the speaker link (a mode click): don't make them wait out a stale
    /// backoff.
    pub fn on_retry(&mut self, now: Instant) {
        self.backoff = 0;
        self.next_scan = now;
    }

    /// One look at the world. `link_open`/`link_live` describe the link; `rescan` is how long until the next look;
    /// `scan` says whether the phone is paired (only called when a look is due and no link is open).
    pub fn tick(&mut self, now: Instant, link_open: bool, link_live: bool, rescan: Duration, scan: impl FnOnce() -> bool) -> Tick {
        if self.switched {
            self.switched = false;
            self.available = false;
            self.next_scan = now + SWITCH_RESCAN;
            return Tick { switched: true, ..Tick::default() };
        }
        if link_open && !link_live {
            self.available = false;
            self.live_since = None;
            self.owner = None;
            self.fail(now);
            return Tick { dropped: true, ..Tick::default() };
        }
        if link_open {
            if self.live_since.is_some_and(|t| now.saturating_duration_since(t) >= LIVE_FOR_RESET) {
                self.backoff = 0;
                self.live_since = None;
            }
        } else {
            self.live_since = None;
        }
        if now >= self.next_scan {
            self.available = link_open || scan(); // a live link is available whatever the hint says now
            self.next_scan = now + rescan;
        }
        Tick::default()
    }

    fn fail(&mut self, now: Instant) {
        self.backoff = (self.backoff * 2).clamp(BACKOFF_MIN, BACKOFF_MAX);
        self.next_scan = now + Duration::from_secs(self.backoff);
    }

    #[cfg(test)]
    fn backoff(&self) -> u64 {
        self.backoff
    }

    #[cfg(test)]
    fn owner(&self) -> Option<&str> {
        self.owner.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCAN: Duration = Duration::from_secs(2);

    fn s(x: u64) -> Duration {
        Duration::from_secs(x)
    }

    fn id(x: &str) -> Option<String> {
        Some(x.to_string())
    }

    /// A machine for phone "poco" that has announced itself and found it paired.
    fn ready(t0: Instant) -> Machine {
        let mut m = Machine::new(t0);
        m.on_hint(t0, id("poco"), false, true, false, false);
        m.tick(t0, false, false, SCAN, || true);
        assert!(m.available);
        m
    }

    #[test]
    fn a_failed_open_backs_off_5_10_20_up_to_120_seconds() {
        let t0 = Instant::now();
        let mut m = ready(t0);
        let mut now = t0;
        for want in [5, 10, 20, 40, 80, 120, 120] {
            m.on_open(now, Open::Failed);
            assert!(!m.available);
            assert_eq!(m.backoff(), want);
            // no look until the wait is over
            assert!(!m.tick(now + s(want) - Duration::from_millis(1), false, false, SCAN, || panic!("scanned too early")).dropped);
            now += s(want);
            m.tick(now, false, false, SCAN, || true);
            assert!(m.available);
        }
    }

    #[test]
    fn a_link_that_opens_and_drops_again_at_once_keeps_backing_off() {
        let t0 = Instant::now();
        let mut m = ready(t0);
        let mut now = t0;
        let mut waits = Vec::new();
        for _ in 0..4 {
            m.on_open(now, Open::Opened);
            now += s(2); // up for 2 s only
            assert!(!m.tick(now, true, true, SCAN, || true).dropped);
            now += s(1);
            assert!(m.tick(now, true, false, SCAN, || true).dropped);
            waits.push(m.backoff());
            now += s(m.backoff());
            m.tick(now, false, false, SCAN, || true);
            assert!(m.available);
        }
        assert_eq!(waits, vec![5, 10, 20, 40], "a flapping link must not be retried every 5 s");
    }

    #[test]
    fn a_link_that_stayed_up_a_minute_is_forgiven() {
        let t0 = Instant::now();
        let mut m = ready(t0);
        m.on_open(t0, Open::Failed); // 5
        m.on_open(t0, Open::Failed); // 10
        assert_eq!(m.backoff(), 10);
        let mut now = t0 + s(10);
        m.tick(now, false, false, SCAN, || true);
        m.on_open(now, Open::Opened);
        now += s(59);
        m.tick(now, true, true, SCAN, || true);
        assert_eq!(m.backoff(), 10, "59 s is not enough");
        now += s(2);
        m.tick(now, true, true, SCAN, || true);
        assert_eq!(m.backoff(), 0);
        // so a later drop starts again at 5 s
        assert!(m.tick(now + s(1), true, false, SCAN, || true).dropped);
        assert_eq!(m.backoff(), 5);
    }

    #[test]
    fn a_hand_back_is_not_a_failure() {
        let t0 = Instant::now();
        let mut m = ready(t0);
        m.on_open(t0, Open::Failed);
        assert_eq!(m.backoff(), 5);
        m.on_open(t0 + s(5), Open::Opened);
        m.on_close();
        assert_eq!(m.backoff(), 0);
    }

    #[test]
    fn a_mode_click_cuts_a_stale_backoff_short() {
        let t0 = Instant::now();
        let mut m = ready(t0);
        let mut now = t0;
        for _ in 0..6 {
            m.on_open(now, Open::Failed);
        }
        assert_eq!(m.backoff(), 120);
        now += s(3);
        assert!(!m.available);
        m.tick(now, false, false, SCAN, || panic!("still backing off"));
        m.on_retry(now);
        m.tick(now, false, false, SCAN, || true);
        assert!(m.available, "available again on the click, not 2 minutes later");
        assert_eq!(m.backoff(), 0);
    }

    #[test]
    fn a_live_link_is_available_whatever_the_hint_says() {
        let t0 = Instant::now();
        let mut m = ready(t0);
        m.on_open(t0, Open::Opened);
        m.tick(t0 + s(2), true, true, SCAN, || panic!("no scan while a link is open"));
        assert!(m.available);
        // adb lost the phone: an empty hint, same roster id gone
        assert!(!m.on_hint(t0 + s(3), None, true, true, true, false));
        m.tick(t0 + s(4), true, true, SCAN, || false);
        assert!(m.available);
    }

    #[test]
    fn a_new_phone_closes_the_old_phones_link_without_the_dropped_path() {
        let t0 = Instant::now();
        let mut m = ready(t0);
        m.on_open(t0, Open::Opened);
        // roster moves to the S24; its name does not match the open source
        assert!(m.on_hint(t0 + s(5), id("s24"), false, true, true, false));
        let t = m.tick(t0 + s(5), false, false, SCAN, || true);
        assert_eq!(t, Tick { switched: true, dropped: false });
        assert!(!m.available);
        assert_eq!(m.backoff(), 0, "a deliberate switch is not a failure");
        // looked at again a second later
        m.tick(t0 + s(5) + s(1), false, false, SCAN, || true);
        assert!(m.available);
    }

    #[test]
    fn a_re_hello_with_other_names_from_the_same_phone_keeps_its_link() {
        let t0 = Instant::now();
        let mut m = ready(t0);
        m.on_open(t0, Open::Opened);
        // the same phone says hello again with a Bluetooth name that no longer matches the open source
        assert!(!m.on_hint(t0 + s(30), id("poco"), false, true, true, false));
        let t = m.tick(t0 + s(30), true, true, SCAN, || false);
        assert_eq!(t, Tick::default());
        assert!(m.available);
    }

    #[test]
    fn a_link_opened_without_a_roster_id_falls_back_to_the_names() {
        let t0 = Instant::now();
        let mut m = Machine::new(t0);
        m.on_hint(t0, None, false, true, false, false);
        m.on_open(t0, Open::Opened); // owner unknown
        assert!(!m.on_hint(t0, None, true, true, true, false), "an empty hint never closes");
        assert!(!m.on_hint(t0, None, false, true, true, true), "names that match keep it");
        assert!(m.on_hint(t0, None, false, true, true, false), "names that do not match close it");
    }

    #[test]
    fn nothing_is_closed_when_no_link_is_open() {
        let t0 = Instant::now();
        let mut m = ready(t0);
        assert!(!m.on_hint(t0, id("s24"), false, true, false, false));
        assert_eq!(m.tick(t0, false, false, SCAN, || false), Tick::default());
    }

    #[test]
    fn a_new_announcement_looks_again_at_once_and_clears_the_wait() {
        let t0 = Instant::now();
        let mut m = ready(t0);
        m.on_open(t0, Open::Failed);
        assert_eq!(m.backoff(), 5);
        m.on_hint(t0 + s(1), id("poco"), false, true, false, false);
        assert_eq!(m.backoff(), 0);
        m.tick(t0 + s(1), false, false, SCAN, || true);
        assert!(m.available);
        // an unchanged announcement changes nothing
        m.on_open(t0 + s(1), Open::Failed);
        m.on_hint(t0 + s(2), id("poco"), false, false, false, false);
        assert_eq!(m.backoff(), 5);
    }

    #[test]
    fn a_link_opened_before_the_roster_named_the_phone_gets_its_owner_from_the_first_matching_hint() {
        let t0 = Instant::now();
        let mut m = Machine::new(t0);
        m.on_hint(t0, None, false, true, false, false); // names known, no roster id yet
        m.on_open(t0, Open::Opened);
        assert_eq!(m.owner(), None);
        // the roster names the phone and the open source still matches: the link is that phone's now
        assert!(!m.on_hint(t0 + s(1), id("poco"), false, true, true, true));
        assert_eq!(m.owner(), Some("poco"));
        // so a switch is judged by id, even though the new phone's hint has no names yet
        assert!(m.on_hint(t0 + s(5), id("s24"), true, true, true, false));
    }

    #[test]
    fn an_empty_hint_does_not_claim_an_unowned_link() {
        let t0 = Instant::now();
        let mut m = Machine::new(t0);
        m.on_hint(t0, None, false, true, false, false);
        m.on_open(t0, Open::Opened);
        assert!(!m.on_hint(t0 + s(1), id("s24"), true, true, true, false)); // empty names: no claim, no close
        assert_eq!(m.owner(), None);
    }

    #[test]
    fn a_duplicate_open_does_not_hand_an_unowned_link_to_the_phone_in_the_roster() {
        let t0 = Instant::now();
        let mut m = Machine::new(t0);
        m.on_hint(t0, None, false, true, false, false);
        m.on_open(t0, Open::Opened); // opened before the roster named the phone
        assert!(!m.on_hint(t0 + s(1), id("s24"), true, true, true, false)); // no names yet: no claim, no close
        m.on_open(t0 + s(2), Open::AlreadyOpen);
        assert_eq!(m.owner(), None);
        // once s24's hint names an open source that does not match, the old link closes
        assert!(m.on_hint(t0 + s(5), id("s24"), false, true, true, false));
    }

    #[test]
    fn a_repeated_open_changes_nothing_about_the_link_it_finds() {
        let t0 = Instant::now();
        let mut m = ready(t0);
        m.on_open(t0, Open::Failed); // 5
        m.on_open(t0, Open::Failed); // 10
        let mut now = t0 + s(10);
        m.tick(now, false, false, SCAN, || true);
        m.on_open(now, Open::Opened);
        assert_eq!(m.owner(), Some("poco"));
        // a duplicate Open arrives while the link is open: it must not touch the owner
        m.on_open(now + s(50), Open::AlreadyOpen);
        assert_eq!(m.owner(), Some("poco"), "the duplicate must not change whose link it is");
        // and it did not restart the one-minute clock: 61 s after the real open the backoff is forgiven
        now += s(61);
        m.tick(now, true, true, SCAN, || true);
        assert_eq!(m.backoff(), 0);
    }
}

