//! The wireless adb link: finds the phone, keeps the phone server running on it, and forwards its two sockets
//! (the mic and the control channel) to localhost. Nothing is installed on the phone: the server is a jar run
//! by `app_process` as the shell user, the same way scrcpy works.

use std::io::{BufRead, BufReader, Read};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use sha2::{Digest, Sha256};

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
        self.serial.contains("._adb-tls-connect.") || self.serial.contains(':')
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
pub fn phone_id(serial: &str) -> String {
    serial
        .strip_prefix("adb-")
        .and_then(|rest| rest.split_once('-'))
        .map_or_else(|| serial.to_owned(), |(id, _)| id.to_owned())
}

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
        self.run(&["devices", "-l"]).map(|out| parse_devices(&out)).unwrap_or_default()
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
        let script = format!(
            "CLASSPATH={REMOTE_DIR}/{JAR} app_process / com.truephonelink.server.Main mode=serve \
             source=voice-recognition engine=native denoise=dfn atten=35 lib={REMOTE_DIR}/{VOICE_LIB}"
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

#[cfg(test)]
mod tests {
    use super::*;

    // Measured 17 Oct 2026: a Galaxy S24 Ultra (One UI 8) and a POCO X6 Pro (HyperOS 2) on one laptop. The POCO
    // is listed twice: it re-advertised itself on a new port after Wireless debugging restarted.
    const TWO_PHONES: &str = "List of devices attached\n\
        adb-S24FAKESN01-Xy12Zq._adb-tls-connect._tcp\tdevice product:e3qxxx model:SM_S928B device:e3q transport_id:15\n\
        adb-X6PROFAKESN00001-abc123 (2)._adb-tls-connect._tcp\tdevice product:duchamp_global model:2311DRK48G transport_id:22\n\
        adb-X6PROFAKESN00001-abc123._adb-tls-connect._tcp\tdevice\n\n";

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
