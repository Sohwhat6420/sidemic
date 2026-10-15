//! The wireless adb link: finds the phone, keeps the phone server running on it, and forwards its two sockets
//! (the mic and the control channel) to localhost. Nothing is installed on the phone: the server is a jar run
//! by `app_process` as the shell user, the same way scrcpy works.

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

    /// The phone over Wi-Fi. adb connects to paired phones by itself through mDNS.
    pub fn device(&self) -> DeviceState {
        let Ok(list) = self.run(&["devices"]) else { return DeviceState::Missing };
        let mut unauthorised = None;
        for line in list.lines().skip(1) {
            let mut parts = line.split_whitespace();
            let (Some(serial), Some(state)) = (parts.next(), parts.next()) else { continue };
            let network = serial.contains("_adb-tls-connect") || serial.contains(':');
            if !network {
                continue;
            }
            match state {
                "device" => return DeviceState::Ready(serial.to_owned()),
                "unauthorized" => unauthorised = Some(serial.to_owned()),
                _ => {}
            }
        }
        unauthorised.map_or(DeviceState::Missing, DeviceState::Unauthorised)
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
        let child = self
            .cmd()
            .args(["-s", serial, "shell", &script])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("adb shell: {e}"))?;
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
