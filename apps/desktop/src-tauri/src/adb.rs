//! The wireless adb link: finds the phone, keeps the phone server running on it, and forwards its two sockets
//! (the mic and the control channel) to localhost. Nothing is installed on the phone: the server is a jar run
//! by `app_process` as the shell user, the same way scrcpy works.

use std::io::{BufRead, BufReader, Read};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU8, Ordering};

use sha2::{Digest, Sha256};
use tpl_voice::{NsKind, Params, Strength};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
pub const MIC_PORT: u16 = 27183;
pub const CTL_PORT: u16 = 27184;
const REMOTE_DIR: &str = "/data/local/tmp/tpl";
const JAR: &str = "tpl-server.jar";
const VOICE_LIB: &str = "libtpl_voice_jni.so";

/// What the phone looks like from adb's side, for the UI's hints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeviceState {
    /// Paired and reachable.
    Ready(String),
    /// Reachable but the phone hasn't said "allow" for this network or laptop yet.
    Unauthorised(String),
    /// Nothing over Wi-Fi: usually after a phone reboot, until the Wireless debugging tile is tapped.
    Missing,
}

/// One row of `adb devices`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listed {
    pub serial: String,
    pub state: String,
    /// From `adb devices -l` (`model:SM_S928B`), spaces given back.
    pub model: Option<String>,
}

impl Listed {
    /// Wireless debugging (mDNS, `adb-<serialno>-<id>._adb-tls-connect._tcp`) or `adb connect ip:port`. USB is
    /// ignored: the app is wireless only.
    fn network(&self) -> bool {
        (self.serial.contains("._adb-tls-connect.") || self.serial.contains(':')) && !self.emulator()
    }

    /// An Android emulator (`emulator-5554`, or one reached through a loopback address) is never a phone to use.
    fn emulator(&self) -> bool {
        let s = self.serial.to_lowercase();
        let m = self.model.as_deref().unwrap_or_default().to_lowercase();
        s.starts_with("emulator-")
            || s.starts_with("127.")
            || s.starts_with("localhost:")
            || s.starts_with("[::1]")
            || m.starts_with("sdk gphone")
            || m.starts_with("android sdk")
            || m.contains("emulator")
    }
}

/// Parses `adb devices`. Columns are tab-separated, and mDNS serials can contain spaces: when a phone re-advertises
/// itself (HyperOS does after every Wireless debugging restart) adb names the new instance `adb-X-y (2)._adb-...`.
/// `adb devices -l` pads with spaces instead (adb 36, measured 21 Oct 2026), so without a tab the serial is
/// everything before the first state word.
pub fn parse_devices(out: &str) -> Vec<Listed> {
    const STATES: &[&str] =
        &["device", "offline", "unauthorized", "authorizing", "connecting", "recovery", "sideload", "bootloader", "rescue", "unknown"];
    out.lines()
        .filter_map(|line| {
            let (serial, rest) = match line.split_once('\t') {
                Some(split) => split,
                None => {
                    let at = line.match_indices(' ').map(|(i, _)| i).find(|&i| {
                        let next = line[i..].trim_start();
                        STATES.iter().any(|s| next == *s || next.strip_prefix(s).is_some_and(|r| r.starts_with(' ')))
                    })?;
                    (&line[..at], &line[at..])
                }
            };
            let mut words = rest.split_whitespace();
            let state = words.next()?.to_owned();
            let model = words.find_map(|w| w.strip_prefix("model:")).map(|m| m.replace('_', " "));
            Some(Listed { serial: serial.trim().to_owned(), state, model })
        })
        .collect()
}

/// The phone behind a serial, stable across its mDNS instances and ports: the phone's own serial number for
/// Wireless debugging (`adb-X6PROFAKESN00001-abc123 (2)._adb-tls-connect._tcp` → `X6PROFAKESN00001`), or the
/// serial itself for `ip:port`.
///
/// An `ip:port` serial (after `adb connect`, which the reconnect fallback uses) has no serial number in its name; the
/// phone's own is looked up once by `Adb::devices` and remembered here (26 Oct: without it the phone came back from a
/// drop under its address, which matched nothing, and the app called its own phone "another phone").
pub fn phone_id(serial: &str) -> String {
    serial
        .strip_prefix("adb-")
        .and_then(|rest| rest.split_once('-'))
        .map_or_else(|| ADDR_OWNER.lock().unwrap().get(serial).cloned().unwrap_or_else(|| serial.to_owned()), |(id, _)| id.to_owned())
}

/// `ip:port` serial → the phone's own serial number (`ro.serialno`).
static ADDR_OWNER: std::sync::Mutex<std::collections::BTreeMap<String, String>> = std::sync::Mutex::new(std::collections::BTreeMap::new());

/// Which phone to use, out of everything adb lists.
///
/// - `want` is the phone used last time (a `phone_id`). While it is remembered, no other phone is ever taken
///   over by itself: with two phones on the same Wi-Fi, the one you use stays yours until you pick the other.
/// - `current` is the serial the server is running on; it wins among one phone's duplicate mDNS entries so the
///   server isn't restarted for nothing.
///
/// Returns the state and the other phones that are ready (`phone_id`, model), for "Use this phone".
pub fn choose(list: &[Listed], want: Option<&str>, current: Option<&str>) -> (DeviceState, Vec<(String, String)>) {
    let net: Vec<&Listed> = list.iter().filter(|l| l.network()).collect();
    let target = want.map(str::to_owned).or_else(|| {
        // Nobody chosen yet: the first ready phone, else the first one asking to be allowed.
        net.iter().find(|l| l.state == "device").or_else(|| net.first()).map(|l| phone_id(&l.serial))
    });
    let mut others: Vec<(String, String)> = net
        .iter()
        .filter(|l| l.state == "device")
        .map(|l| {
            let id = phone_id(&l.serial);
            let model = l.model.clone().unwrap_or_else(|| id.clone());
            (id, model)
        })
        .filter(|(id, _)| Some(id) != target.as_ref())
        .collect();
    others.sort();
    others.dedup();
    let Some(target) = target else { return (DeviceState::Missing, others) };
    let mine: Vec<&&Listed> = net.iter().filter(|l| phone_id(&l.serial) == target).collect();
    let ready = mine
        .iter()
        .find(|l| l.state == "device" && Some(l.serial.as_str()) == current)
        .or_else(|| mine.iter().find(|l| l.state == "device"));
    let state = if let Some(l) = ready {
        DeviceState::Ready(l.serial.clone())
    } else if let Some(l) = mine.iter().find(|l| l.state == "unauthorized") {
        DeviceState::Unauthorised(l.serial.clone())
    } else {
        DeviceState::Missing
    };
    (state, others)
}

/// Every phone that is ready right now (`phone_id`, model), once each, for the tray's Phones menu. Emulators and
/// USB rows are left out, as in `choose`.
pub fn ready_phones(list: &[Listed]) -> Vec<(String, String)> {
    let mut ready: Vec<(String, String)> = list
        .iter()
        .filter(|l| l.network() && l.state == "device")
        .map(|l| {
            let id = phone_id(&l.serial);
            let model = l.model.clone().unwrap_or_else(|| id.clone());
            (id, model)
        })
        .collect();
    ready.sort();
    // One phone can be listed twice (a re-advertised mDNS instance); keep the entry that knows its model.
    ready.dedup_by(|b, a| {
        let same = a.0 == b.0;
        if same && a.1 == a.0 {
            a.1 = std::mem::take(&mut b.1);
        }
        same
    });
    ready
}

/// Which ready phone to stand in for a missing `chosen` one: the one used most recently (`used` holds each phone's
/// last-used time), else the first by id. Never `chosen` itself.
pub fn stand_in(ready: &[(String, String)], chosen: &str, used: &std::collections::BTreeMap<String, u64>) -> Option<String> {
    ready
        .iter()
        .filter(|(id, _)| id != chosen)
        .max_by_key(|(id, _)| (used.get(id).copied().unwrap_or(0), std::cmp::Reverse(id.clone())))
        .map(|(id, _)| id.clone())
}

/// Parses `adb mdns services` into (instance's phone_id, "ip:port") for Wireless debugging connect services.
pub fn parse_mdns(out: &str) -> Vec<(String, String)> {
    out.lines()
        .filter_map(|line| {
            let mut cols = line.split('\t');
            let (instance, service, addr) = (cols.next()?, cols.next()?, cols.next()?);
            service.starts_with("_adb-tls-connect").then(|| (phone_id(instance.trim()), addr.trim().to_owned()))
        })
        .collect()
}

/// TCP ports in LISTEN state from `/proc/net/tcp` + `/proc/net/tcp6` (column 2 is `addr:PORT` in hex, column 4
/// the state, `0A` = listen).
pub fn parse_listening(proc_net_tcp: &str) -> Vec<u16> {
    proc_net_tcp
        .lines()
        .filter_map(|line| {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.get(3) != Some(&"0A") {
                return None;
            }
            u16::from_str_radix(cols.get(1)?.rsplit(':').next()?, 16).ok()
        })
        .collect()
}

pub struct Adb {
    exe: PathBuf,
}

impl Adb {
    /// adb from (in order) TPL_ADB, the app's bundled copy, scrcpy's winget install, then PATH.
    pub fn find(bundled: Option<&Path>) -> Option<Adb> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Ok(p) = std::env::var("TPL_ADB") {
            candidates.push(p.into());
        }
        if let Some(b) = bundled {
            candidates.push(b.join("adb.exe"));
        }
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            let winget = Path::new(&local).join("Microsoft/WinGet/Packages");
            if let Ok(dirs) = std::fs::read_dir(&winget) {
                for d in dirs.flatten() {
                    if d.file_name().to_string_lossy().starts_with("Genymobile.scrcpy") {
                        if let Ok(inner) = std::fs::read_dir(d.path()) {
                            for i in inner.flatten() {
                                candidates.push(i.path().join("adb.exe"));
                            }
                        }
                    }
                }
            }
        }
        candidates.push("adb.exe".into());
        candidates
            .into_iter()
            .find(|p| p.is_file() || p.as_os_str() == "adb.exe")
            .map(|exe| Adb { exe })
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new(&self.exe);
        c.creation_flags(CREATE_NO_WINDOW).stdin(Stdio::null());
        c
    }

    /// Runs adb and returns stdout, or stderr as the error.
    pub fn run(&self, args: &[&str]) -> Result<String, String> {
        let out = self.cmd().args(args).output().map_err(|e| format!("adb: {e}"))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_owned())
        }
    }

    pub fn start_daemon(&self) {
        let _ = self.run(&["start-server"]);
    }

    /// Everything adb lists. adb connects to paired phones by itself through mDNS.
    pub fn devices(&self) -> Vec<Listed> {
        let list = self.run(&["devices", "-l"]).map(|out| parse_devices(&out)).unwrap_or_default();
        for l in &list {
            let by_address = l.state == "device" && l.network() && !l.serial.starts_with("adb-");
            if by_address && !ADDR_OWNER.lock().unwrap().contains_key(&l.serial) {
                if let Ok(out) = self.shell(&l.serial, "getprop ro.serialno") {
                    let id = out.trim();
                    if !id.is_empty() && !id.contains(char::is_whitespace) {
                        ADDR_OWNER.lock().unwrap().insert(l.serial.clone(), id.to_owned());
                    }
                }
            }
        }
        list
    }

    /// The address the phone's Wireless debugging is listening on right now ("ip:port"), to reconnect to it
    /// directly if mDNS goes wrong. A phone can advertise stale ports (HyperOS keeps the old instance after a
    /// restart of Wireless debugging), so it is the advertised address whose port the phone is really listening on.
    pub fn address(&self, serial: &str) -> Option<String> {
        if !serial.contains("._adb-tls-connect.") {
            return serial.contains(':').then(|| serial.to_owned());
        }
        let id = phone_id(serial);
        let advertised: Vec<String> =
            parse_mdns(&self.run(&["mdns", "services"]).ok()?).into_iter().filter(|(p, _)| *p == id).map(|(_, a)| a).collect();
        let listening = parse_listening(&self.shell(serial, "cat /proc/net/tcp6 /proc/net/tcp").ok()?);
        advertised.into_iter().find(|a| a.rsplit(':').next().and_then(|p| p.parse().ok()).is_some_and(|p| listening.contains(&p)))
    }

    /// When the chosen phone is missing: drops its dead (offline) entries and connects straight to every address
    /// it is advertising plus the last one that worked. adb's own mDNS auto-connect can stick to a stale instance.
    /// Returns the addresses that connected.
    pub fn reconnect(&self, phone: &str, last: Option<&str>) -> Vec<String> {
        for l in self.devices() {
            if l.state == "offline" && phone_id(&l.serial) == phone {
                let _ = self.run(&["disconnect", &l.serial]);
            }
        }
        let mut addrs: Vec<String> = self
            .run(&["mdns", "services"])
            .map(|out| parse_mdns(&out).into_iter().filter(|(p, _)| p == phone).map(|(_, a)| a).collect())
            .unwrap_or_default();
        addrs.extend(last.map(str::to_owned));
        addrs.sort();
        addrs.dedup();
        addrs
            .into_iter()
            .filter(|a| self.run(&["connect", a]).is_ok_and(|out| out.contains("connected to")))
            .collect()
    }

    pub fn shell(&self, serial: &str, script: &str) -> Result<String, String> {
        self.run(&["-s", serial, "shell", script])
    }

    /// Pushes the server jar and the voice library when the phone's copies differ (checked by SHA-256).
    pub fn ensure_files(&self, serial: &str, dir: &Path) -> Result<(), String> {
        self.shell(serial, &format!("mkdir -p {REMOTE_DIR}"))?;
        for name in [JAR, VOICE_LIB] {
            let local = dir.join(name);
            let bytes = std::fs::read(&local).map_err(|e| format!("{}: {e}", local.display()))?;
            let want = hex(&Sha256::digest(&bytes));
            let have = self.shell(serial, &format!("sha256sum {REMOTE_DIR}/{name} 2>/dev/null")).unwrap_or_default();
            if !have.starts_with(&want) {
                self.run(&["-s", serial, "push", &local.to_string_lossy(), &format!("{REMOTE_DIR}/{name}")])?;
            }
        }
        Ok(())
    }

    /// Starts the phone server (stopping any older copy first) and forwards its sockets.
    /// The returned child is the `adb shell` that hosts it; its stdout carries the server's log.
    pub fn start_server(&self, serial: &str) -> Result<Child, String> {
        let _ = self.shell(serial, "pkill -f com.truephonelink.server");
        let args = server_args(strength(), |k| std::env::var(k).ok());
        let script = format!(
            "CLASSPATH={REMOTE_DIR}/{JAR} app_process / com.truephonelink.server.Main mode=serve {args} lib={REMOTE_DIR}/{VOICE_LIB}"
        );
        let mut child = self
            .cmd()
            .args(["-s", serial, "shell", &script])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("adb shell: {e}"))?;
        // The server's log goes to the app's log. It must be read either way: a full pipe would block adb, and
        // with it the server's next log line and the mic thread behind it.
        if let Some(out) = child.stdout.take() {
            pipe_to_log(out);
        }
        if let Some(err) = child.stderr.take() {
            pipe_to_log(err);
        }
        self.run(&["-s", serial, "forward", &format!("tcp:{MIC_PORT}"), "localabstract:tpl_mic"])?;
        self.run(&["-s", serial, "forward", &format!("tcp:{CTL_PORT}"), "localabstract:tpl_ctl"])?;
        Ok(child)
    }

    pub fn stop_server(&self, serial: &str) {
        let _ = self.shell(serial, "pkill -f com.truephonelink.server");
        let _ = self.run(&["-s", serial, "forward", "--remove", &format!("tcp:{MIC_PORT}")]);
        let _ = self.run(&["-s", serial, "forward", "--remove", &format!("tcp:{CTL_PORT}")]);
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn pipe_to_log(r: impl Read + Send + 'static) {
    let _ = std::thread::Builder::new().name("tpl-phone-log".into()).spawn(move || {
        for line in BufReader::new(r).lines().map_while(Result::ok) {
            crate::log::line(format!("phone: {}", line.trim_end()));
        }
    });
}

/// The phone-server argument for `TPL_MAXGAIN` (" maxgain=12"), or nothing. Only a plain number from 0 to 24 dB is
/// passed on; anything else is ignored rather than put on a shell command line.
fn maxgain_arg(v: Option<&str>) -> String {
    match v.and_then(|v| v.trim().parse::<f32>().ok()) {
        Some(g) if g.is_finite() && (0.0..=24.0).contains(&g) => format!(" maxgain={g}"),
        _ => String::new(),
    }
}

/// The "Voice cleaning" setting, mirrored here for the phone server's command line (an index into `Strength::ALL`).
static STRENGTH: AtomicU8 = AtomicU8::new(2);

pub fn set_strength(s: Strength) {
    STRENGTH.store(Strength::ALL.iter().position(|&k| k == s).unwrap_or(2) as u8, Ordering::Relaxed);
}

pub fn strength() -> Strength {
    Strength::ALL.get(STRENGTH.load(Ordering::Relaxed) as usize).copied().unwrap_or_default()
}

/// What the phone's voice chain adds to the mic delay: DeepFilterNet's frame and look-ahead (30 ms) plus the
/// limiter's look-ahead (2 ms), logged by the phone as "latency 31.9 ms"; the limiter alone without it.
pub fn phone_latency_ms(s: Strength) -> u32 {
    if s.ns() == NsKind::None {
        2
    } else {
        32
    }
}

/// Sources the phone server accepts; `TPL_SOURCE` must be one of them.
const SOURCES: [&str; 6] = ["voice-recognition", "mic", "camcorder", "voice-communication", "unprocessed", "voice-performance"];

/// The phone server's voice arguments for a strength, plus the A/B overrides read through `env`. Only known words
/// and plain numbers reach the shell command line.
///
/// - `TPL_DENOISE=none|rnnoise|dfn` replaces the suppressor (does the phone's DeepFilterNet defeat the echo
///   canceller?).
/// - `TPL_DSP=0` bypasses the phone's whole voice chain: the check for a time-varying gain in front of the laptop's
///   echo canceller.
/// - `TPL_MAXGAIN=<dB>` caps the speech leveller (default 18): "does the leveller in front of the echo canceller
///   spend its gain" (investigations/2026-10-29-echo-sim.md).
/// - `TPL_SOURCE=<android source>` picks another capture source. On a POCO X6 Pro, voice-recognition and
///   unprocessed carry a ~25 dB hole from 7.8 to 11.3 kHz and voice-communication stops at 7.8 kHz, while mic and
///   camcorder are full band (room tone, 11 Nov 2026); the default stays voice-recognition until a spoken A/B on
///   that phone says otherwise.
fn server_args(strength: Strength, env: impl Fn(&str) -> Option<String>) -> String {
    let ns = match strength.ns() {
        NsKind::None => "none",
        NsKind::Rnnoise => "rnnoise",
        NsKind::Dfn => "dfn",
    };
    let denoise = env("TPL_DENOISE").filter(|d| ["none", "rnnoise", "dfn"].contains(&d.as_str())).unwrap_or_else(|| ns.into());
    let dsp = if env("TPL_DSP").as_deref() == Some("0") { "0" } else { "1" };
    let maxgain = match maxgain_arg(env("TPL_MAXGAIN").as_deref()) {
        m if m.is_empty() => format!(" maxgain={}", Params::default().max_gain_db),
        m => m,
    };
    let source = env("TPL_SOURCE").filter(|s| SOURCES.contains(&s.as_str())).unwrap_or_else(|| SOURCES[0].into());
    format!(
        "source={source} engine=native denoise={denoise} atten={} expand=0 dsp={dsp}{maxgain}",
        strength.atten_db()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn each_voice_cleaning_strength_reaches_the_phone_command_line() {
        assert_eq!(
            server_args(Strength::Medium, no_env),
            "source=voice-recognition engine=native denoise=dfn atten=18 expand=0 dsp=1 maxgain=18"
        );
        assert!(server_args(Strength::Light, no_env).contains(" denoise=dfn atten=12 "));
        assert!(server_args(Strength::Strong, no_env).contains(" denoise=dfn atten=35 "));
        assert!(server_args(Strength::Off, no_env).contains(" denoise=none "));
    }

    #[test]
    fn the_ab_overrides_pass_only_known_words_and_plain_numbers() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
        };
        let a = server_args(Strength::Medium, env(&[("TPL_DENOISE", "rnnoise"), ("TPL_DSP", "0"), ("TPL_MAXGAIN", "12"), ("TPL_SOURCE", "mic")]));
        assert_eq!(a, "source=mic engine=native denoise=rnnoise atten=18 expand=0 dsp=0 maxgain=12");
        let b = server_args(Strength::Medium, env(&[("TPL_DENOISE", "dfn; reboot"), ("TPL_SOURCE", "mic lib=/x"), ("TPL_MAXGAIN", "$(id)")]));
        assert_eq!(b, server_args(Strength::Medium, no_env));
    }

    #[test]
    fn the_strength_mirror_round_trips_and_sets_the_phone_delay() {
        for s in Strength::ALL {
            set_strength(s);
            assert_eq!(strength(), s);
        }
        set_strength(Strength::default());
        assert_eq!(phone_latency_ms(Strength::Off), 2);
        assert_eq!(phone_latency_ms(Strength::Medium), 32);
    }

    #[test]
    fn the_leveller_cap_is_passed_on_only_as_a_plain_number_of_0_to_24_db() {
        assert_eq!(maxgain_arg(Some("12")), " maxgain=12");
        assert_eq!(maxgain_arg(Some(" 0 ")), " maxgain=0");
        assert_eq!(maxgain_arg(Some("7.5")), " maxgain=7.5");
        assert_eq!(maxgain_arg(None), "");
        for bad in ["", "25", "-1", "NaN", "inf", "12; reboot", "$(id)", "12 lib=/x"] {
            assert_eq!(maxgain_arg(Some(bad)), "", "{bad:?}");
        }
    }

    // Measured 17 Oct 2026: a Galaxy S24 Ultra (One UI 8) and a POCO X6 Pro (HyperOS 2) on one laptop. The POCO
    // is listed twice: it re-advertised itself on a new port after Wireless debugging restarted.
    const TWO_PHONES: &str = "List of devices attached\n\
        adb-S24FAKESN01-Xy12Zq._adb-tls-connect._tcp\tdevice product:e3qxxx model:SM_S928B device:e3q transport_id:15\n\
        adb-X6PROFAKESN00001-abc123 (2)._adb-tls-connect._tcp\tdevice product:duchamp_global model:2311DRK48G transport_id:22\n\
        adb-X6PROFAKESN00001-abc123._adb-tls-connect._tcp\tdevice\n\n";

    #[test]
    fn a_phone_reconnected_by_address_is_still_the_same_phone() {
        // 26 Oct live test: after `adb connect 192.0.2.78:34363` the phone is listed by address only.
        ADDR_OWNER.lock().unwrap().insert("192.0.2.78:34363".into(), "X6PROFAKESN00001".into());
        let list = parse_devices("List of devices attached\n192.0.2.78:34363\tdevice product:duchamp_global model:2311DRK48G transport_id:3\n");
        assert_eq!(phone_id("192.0.2.78:34363"), "X6PROFAKESN00001");
        assert_eq!(ready_phones(&list), vec![("X6PROFAKESN00001".to_string(), "2311DRK48G".to_string())]);
        let (state, others) = choose(&list, Some("X6PROFAKESN00001"), None);
        assert_eq!(state, DeviceState::Ready("192.0.2.78:34363".into()));
        assert!(others.is_empty(), "its own address must not be offered as another phone");
    }

    #[test]
    fn serials_with_spaces_parse() {
        let l = parse_devices(TWO_PHONES);
        assert_eq!(l.len(), 3);
        assert_eq!(l[1].serial, "adb-X6PROFAKESN00001-abc123 (2)._adb-tls-connect._tcp");
        assert_eq!(l[1].state, "device");
        assert_eq!(l[0].model.as_deref(), Some("SM S928B"));
        assert_eq!(l[2].model, None);
        assert_eq!(phone_id(&l[1].serial), "X6PROFAKESN00001");
        assert_eq!(phone_id("192.0.2.78:5555"), "192.0.2.78:5555");
    }

    #[test]
    fn the_long_listing_pads_with_spaces() {
        // `adb devices -l` from scrcpy 4.1's adb, 21 Oct 2026: no tabs at all.
        let l = parse_devices(
            "List of devices attached\r\n\
             adb-X6PROFAKESN00001-abc123._adb-tls-connect._tcp device product:duchamp_global model:2311DRK48G device:duchamp transport_id:73\r\n\
             adb-X6PROFAKESN00001-abc123 (2)._adb-tls-connect._tcp offline transport_id:74\r\n\
             192.0.2.78:5555     unauthorized transport_id:75\r\n\r\n",
        );
        assert_eq!(l.len(), 3);
        assert_eq!(l[0].serial, "adb-X6PROFAKESN00001-abc123._adb-tls-connect._tcp");
        assert_eq!(l[0].state, "device");
        assert_eq!(l[0].model.as_deref(), Some("2311DRK48G"));
        assert_eq!(l[1].serial, "adb-X6PROFAKESN00001-abc123 (2)._adb-tls-connect._tcp");
        assert_eq!(l[1].state, "offline");
        assert_eq!(l[2].serial, "192.0.2.78:5555");
        assert_eq!(l[2].state, "unauthorized");
        assert!(matches!(choose(&l, None, None).0, DeviceState::Ready(ref s) if s == &l[0].serial));
    }

    #[test]
    fn the_remembered_phone_wins_and_the_other_is_offered() {
        let l = parse_devices(TWO_PHONES);
        let (s, others) = choose(&l, Some("X6PROFAKESN00001"), None);
        assert!(matches!(s, DeviceState::Ready(ref x) if phone_id(x) == "X6PROFAKESN00001"));
        assert_eq!(others, vec![("S24FAKESN01".to_owned(), "SM S928B".to_owned())]);
        let (s, _) = choose(&l, Some("S24FAKESN01"), None);
        assert_eq!(s, DeviceState::Ready("adb-S24FAKESN01-Xy12Zq._adb-tls-connect._tcp".into()));
    }

    #[test]
    fn first_run_takes_the_first_ready_phone() {
        let (s, others) = choose(&parse_devices(TWO_PHONES), None, None);
        assert_eq!(s, DeviceState::Ready("adb-S24FAKESN01-Xy12Zq._adb-tls-connect._tcp".into()));
        assert_eq!(others[0].0, "X6PROFAKESN00001");
    }

    #[test]
    fn a_missing_phone_is_not_replaced_by_another() {
        let l = parse_devices("List of devices attached\nadb-X6PROFAKESN00001-abc123._adb-tls-connect._tcp\tdevice\n");
        let (s, others) = choose(&l, Some("S24FAKESN01"), None);
        assert_eq!(s, DeviceState::Missing);
        assert_eq!(others[0].0, "X6PROFAKESN00001");
    }

    #[test]
    fn duplicates_keep_the_running_server_and_skip_dead_entries() {
        let l = parse_devices(TWO_PHONES);
        let cur = "adb-X6PROFAKESN00001-abc123._adb-tls-connect._tcp";
        assert_eq!(choose(&l, Some("X6PROFAKESN00001"), Some(cur)).0, DeviceState::Ready(cur.into()));
        let stale = parse_devices(
            "List of devices attached\n\
             adb-X6PROFAKESN00001-abc123._adb-tls-connect._tcp\toffline\n\
             adb-X6PROFAKESN00001-abc123 (2)._adb-tls-connect._tcp\tdevice\n",
        );
        assert_eq!(
            choose(&stale, Some("X6PROFAKESN00001"), Some(cur)).0,
            DeviceState::Ready("adb-X6PROFAKESN00001-abc123 (2)._adb-tls-connect._tcp".into())
        );
    }

    #[test]
    fn emulators_are_never_a_phone() {
        let l = parse_devices(
            "List of devices attached\n\
             emulator-5554\tdevice product:sdk_gphone64_x86_64 model:sdk_gphone64_x86_64 device:emu64xa transport_id:3\n\
             127.0.0.1:5555\tdevice model:Pixel_7\n\
             adb-X6PROFAKESN00001-abc123._adb-tls-connect._tcp\tdevice model:2311DRK48G\n",
        );
        assert_eq!(l.len(), 3);
        let (s, others) = choose(&l, None, None);
        assert_eq!(s, DeviceState::Ready("adb-X6PROFAKESN00001-abc123._adb-tls-connect._tcp".into()));
        assert!(others.is_empty());
        // Even when only an emulator is attached, or one was remembered, it is not offered or used.
        let only = parse_devices("List of devices attached\nemulator-5554\tdevice model:sdk_gphone64_x86_64\n");
        assert_eq!(choose(&only, None, None), (DeviceState::Missing, vec![]));
        assert_eq!(choose(&only, Some("emulator-5554"), None).0, DeviceState::Missing);
    }

    #[test]
    fn ready_phones_lists_each_once_with_its_model() {
        let l = parse_devices(TWO_PHONES);
        assert_eq!(
            ready_phones(&l),
            vec![("S24FAKESN01".to_owned(), "SM S928B".to_owned()), ("X6PROFAKESN00001".to_owned(), "2311DRK48G".to_owned())]
        );
        // The model-less duplicate listed first must not win.
        let l = parse_devices(
            "List of devices attached\n\
             adb-X6PROFAKESN00001-a._adb-tls-connect._tcp\tdevice\n\
             adb-X6PROFAKESN00001-a (2)._adb-tls-connect._tcp\tdevice model:2311DRK48G\n\
             adb-S24FAKESN01-b._adb-tls-connect._tcp\toffline model:SM_S928B\n",
        );
        assert_eq!(ready_phones(&l), vec![("X6PROFAKESN00001".to_owned(), "2311DRK48G".to_owned())]);
    }

    #[test]
    fn the_stand_in_is_the_last_used_other_phone() {
        let ready = vec![("A".to_owned(), "a".to_owned()), ("B".to_owned(), "b".to_owned()), ("C".to_owned(), "c".to_owned())];
        let mut used = std::collections::BTreeMap::new();
        assert_eq!(stand_in(&ready, "A", &used).as_deref(), Some("B"));
        used.insert("C".to_owned(), 50);
        used.insert("B".to_owned(), 10);
        used.insert("A".to_owned(), 99);
        assert_eq!(stand_in(&ready, "A", &used).as_deref(), Some("C"));
        assert_eq!(stand_in(&ready[..1], "A", &used), None);
        assert_eq!(stand_in(&[], "A", &used), None);
    }

    #[test]
    fn unauthorised_and_usb_rows() {
        let l = parse_devices("List of devices attached\nS24FAKESN01\tdevice usb:1-1\nadb-X1-a._adb-tls-connect._tcp\tunauthorized\n");
        assert_eq!(choose(&l, None, None).0, DeviceState::Unauthorised("adb-X1-a._adb-tls-connect._tcp".into()));
        assert_eq!(choose(&[], None, None).0, DeviceState::Missing);
    }

    #[test]
    fn stale_mdns_ports_are_told_apart() {
        let mdns = "List of discovered mdns services\n\
            adb-X6PROFAKESN00001-abc123 (2)\t_adb-tls-connect._tcp\t192.0.2.78:36849\n\
            adb-X6PROFAKESN00001-abc123\t_adb-tls-connect._tcp\t192.0.2.78:40725\n\
            adb-S24FAKESN01-Xy12Zq\t_adb-tls-connect._tcp\t192.0.2.246:32777\n\
            adb-S24FAKESN01-Xy12Zq\t_adb-tls-pairing._tcp\t192.0.2.246:41111\n";
        let m = parse_mdns(mdns);
        assert_eq!(m.len(), 3);
        assert_eq!(m[0], ("X6PROFAKESN00001".into(), "192.0.2.78:36849".into()));
        // The POCO's /proc/net/tcp6 at that moment: only 36849 (0x8FF1) was listening on all addresses.
        let tcp = "  sl  local_address                         remote_address                        st\n\
            0: 0000000000000000FFFF00000100007F:49C8 00000000000000000000000000000000:0000 0A 0\n\
            1: 00000000000000000000000000000000:8FF1 00000000000000000000000000000000:0000 0A 0\n\
            2: 0000000000000000FFFF00004E32A8C0:8FF1 0000000000000000FFFF00002E32A8C0:D2A1 01 0\n";
        assert_eq!(parse_listening(tcp), vec![0x49C8, 36849]);
    }
}
