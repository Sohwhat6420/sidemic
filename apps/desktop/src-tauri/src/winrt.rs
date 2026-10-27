//! The Windows Runtime pieces: Bluetooth audio from the phone (the laptop as its speaker), what media the
//! laptop is playing, the user's accent colour, and the foreground app.

use windows::core::{Result, HSTRING};
use windows::Devices::Enumeration::DeviceInformation;
use windows::Foundation::TypedEventHandler;
use windows::Media::Audio::{AudioPlaybackConnection, AudioPlaybackConnectionOpenResultStatus, AudioPlaybackConnectionState};
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSessionManager as MediaManager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as PlaybackStatus,
};
use windows::UI::ViewManagement::{UIColorType, UISettings};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Shell::{SHQueryUserNotificationState, QUNS_BUSY, QUNS_RUNNING_D3D_FULL_SCREEN};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

/// Phone media over Bluetooth A2DP, played on the laptop's default speaker. The laptop opens and
/// closes the link itself, so the phone never needs to be touched.
#[derive(Default)]
pub struct A2dp {
    conn: Option<AudioPlaybackConnection>,
    /// Windows reports Opened only while audio is actually flowing, and Closed both before the first sound and
    /// after a real drop. So Closed only means "dropped" once Opened has been seen.
    seen_opened: bool,
}

impl A2dp {
    /// Bluetooth devices that can play to this laptop, as (id, name).
    pub fn devices() -> Result<Vec<(HSTRING, String)>> {
        let sel = AudioPlaybackConnection::GetDeviceSelector()?;
        let list = DeviceInformation::FindAllAsyncAqsFilter(&sel)?.join()?;
        let mut out = Vec::new();
        for d in list {
            out.push((d.Id()?, d.Name()?.to_string()));
        }
        Ok(out)
    }

    pub fn is_open(&self) -> bool {
        self.conn.is_some()
    }

    /// Opens the link to the device whose name contains `name_hint` (e.g. "Galaxy S24 Ultra").
    pub fn open(&mut self, name_hint: &str) -> std::result::Result<String, String> {
        if self.conn.is_some() {
            return Ok("already open".into());
        }
        let devices = Self::devices().map_err(|e| e.to_string())?;
        let (id, name) = pick_device(devices, name_hint)?;
        let conn = AudioPlaybackConnection::TryCreateFromId(&id).map_err(|e| e.to_string())?;
        conn.StartAsync().and_then(|a| a.join()).map_err(|e| e.to_string())?;
        let result = conn.OpenAsync().and_then(|a| a.join()).map_err(|e| e.to_string())?;
        let status = result.Status().map_err(|e| e.to_string())?;
        if status != AudioPlaybackConnectionOpenResultStatus::Success {
            return Err(format!("Bluetooth audio from {name} didn't open ({status:?}); is the phone in range?"));
        }
        self.conn = Some(conn);
        Ok(name)
    }

    /// False once the link has dropped (it drops when the phone goes out of range or Bluetooth is turned off).
    pub fn is_live(&mut self) -> bool {
        match self.state() {
            Some(AudioPlaybackConnectionState::Opened) => {
                self.seen_opened = true;
                true
            }
            Some(_) => !self.seen_opened,
            None => false,
        }
    }

    pub fn state(&self) -> Option<AudioPlaybackConnectionState> {
        self.conn.as_ref().and_then(|c| c.State().ok())
    }

    pub fn close(&mut self) {
        self.seen_opened = false;
        if let Some(c) = self.conn.take() {
            let _ = c.Close();
        }
    }
}

/// Which Bluetooth audio sources are paired and usable, kept current by Windows' device watcher. It replaced a
/// full device search every 10 s, which cost 63 ms of CPU and leaked 2.6 KB each time (measured with
/// `--probe bt-scan`); the watcher costs nothing until a device comes or goes.
pub struct BtWatch {
    watcher: windows::Devices::Enumeration::DeviceWatcher,
    ids: std::sync::Arc<std::sync::Mutex<std::collections::BTreeSet<String>>>,
}

impl BtWatch {
    pub fn start() -> Result<BtWatch> {
        use windows::Devices::Enumeration::{DeviceInformationUpdate, DeviceWatcher};
        let watcher = DeviceInformation::CreateWatcherAqsFilter(&AudioPlaybackConnection::GetDeviceSelector()?)?;
        let ids = std::sync::Arc::new(std::sync::Mutex::new(std::collections::BTreeSet::new()));
        let added = ids.clone();
        watcher.Added(&TypedEventHandler::<DeviceWatcher, DeviceInformation>::new(move |_, d| {
            if let Ok(d) = d.ok() {
                added.lock().unwrap().insert(d.Id()?.to_string());
            }
            Ok(())
        }))?;
        let removed = ids.clone();
        watcher.Removed(&TypedEventHandler::<DeviceWatcher, DeviceInformationUpdate>::new(move |_, u| {
            if let Ok(u) = u.ok() {
                removed.lock().unwrap().remove(&u.Id()?.to_string());
            }
            Ok(())
        }))?;
        // Windows only reports additions and removals to a watcher that also takes updates.
        watcher.Updated(&TypedEventHandler::<DeviceWatcher, DeviceInformationUpdate>::new(|_, _| Ok(())))?;
        watcher.Start()?;
        Ok(BtWatch { watcher, ids })
    }

    pub fn any(&self) -> bool {
        !self.ids.lock().unwrap().is_empty()
    }
}

impl Drop for BtWatch {
    fn drop(&mut self) {
        let _ = self.watcher.Stop();
    }
}

/// Media sessions the laptop is playing right now, as (app id, title). Includes Chrome tabs (YouTube) because
/// Chrome publishes its media to Windows.
pub fn laptop_media() -> Result<Vec<(String, String)>> {
    let mgr = MediaManager::RequestAsync()?.join()?;
    let mut playing = Vec::new();
    for s in mgr.GetSessions()? {
        let info = s.GetPlaybackInfo()?;
        if info.PlaybackStatus()? == PlaybackStatus::Playing {
            let app = s.SourceAppUserModelId()?.to_string();
            let title = s
                .TryGetMediaPropertiesAsync()
                .and_then(|a| a.join())
                .and_then(|p| p.Title())
                .map(|t| t.to_string())
                .unwrap_or_default();
            playing.push((app, title));
        }
    }
    Ok(playing)
}

/// The watcher's media check, once a second. `MediaManager::RequestAsync` each time cost 9 ms and leaked about
/// 2.8 KB per call (≈200 MB a day, measured with `--probe media`), so the manager is requested once and kept;
/// titles aren't fetched here because the watcher only needs which apps are playing.
#[derive(Default)]
pub struct MediaWatch {
    mgr: Option<MediaManager>,
}

impl MediaWatch {
    /// App ids of the media sessions playing right now.
    pub fn playing(&mut self) -> Vec<String> {
        if self.mgr.is_none() {
            self.mgr = MediaManager::RequestAsync().and_then(|a| a.join()).ok();
        }
        let Some(mgr) = &self.mgr else { return Vec::new() };
        let read = || -> Result<Vec<String>> {
            let mut out = Vec::new();
            for s in mgr.GetSessions()? {
                if s.GetPlaybackInfo()?.PlaybackStatus()? == PlaybackStatus::Playing {
                    out.push(s.SourceAppUserModelId()?.to_string());
                }
            }
            Ok(out)
        };
        read().unwrap_or_else(|_| {
            self.mgr = None; // ask again next time
            Vec::new()
        })
    }
}

/// The Windows accent palette as CSS hex colours: [light3, light2, light1, accent, dark1, dark2, dark3], and
/// whether apps are in dark mode.
pub fn accent(ui: &UISettings) -> Result<(Vec<String>, bool)> {
    let mut out = Vec::new();
    for t in [
        UIColorType::AccentLight3,
        UIColorType::AccentLight2,
        UIColorType::AccentLight1,
        UIColorType::Accent,
        UIColorType::AccentDark1,
        UIColorType::AccentDark2,
        UIColorType::AccentDark3,
    ] {
        let c = ui.GetColorValue(t)?;
        out.push(format!("#{:02x}{:02x}{:02x}", c.R, c.G, c.B));
    }
    let bg = ui.GetColorValue(UIColorType::Background)?;
    Ok((out, (bg.R as u32 + bg.G as u32 + bg.B as u32) < 384))
}

/// Calls `f` whenever the accent colour or light/dark mode changes. Keep the returned UISettings alive.
pub fn watch_accent(f: impl Fn() + Send + 'static) -> Result<UISettings> {
    let ui = UISettings::new()?;
    ui.ColorValuesChanged(&TypedEventHandler::new(move |_, _| {
        f();
        Ok(())
    }))?;
    Ok(ui)
}

/// The foreground app's exe name, and whether something is running full screen (a game, usually).
pub fn foreground() -> (Option<String>, bool) {
    unsafe {
        let hwnd: HWND = GetForegroundWindow();
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        let exe = crate::wasapi::process_name(pid);
        let full = matches!(SHQueryUserNotificationState(), Ok(s) if s == QUNS_RUNNING_D3D_FULL_SCREEN || s == QUNS_BUSY);
        (exe, full)
    }
}

/// Which paired Bluetooth audio source is the phone called `name_hint`. A name that matches nothing is NOT a
/// reason to take the one device that is paired: on 27 Oct 2026 the POCO was unpaired and the only paired source was
/// Ethan's Galaxy S24 Ultra, which the app then opened. The lone device is taken only when the phone's name is not
/// known at all (the hint is the placeholder "phone").
pub fn pick_device(devices: Vec<(windows::core::HSTRING, String)>, name_hint: &str) -> std::result::Result<(windows::core::HSTRING, String), String> {
    let only = (devices.len() == 1 && name_hint == UNKNOWN_PHONE).then(|| devices[0].clone());
    devices
        .into_iter()
        .find(|(_, n)| n.contains(name_hint) || (!n.is_empty() && name_hint.contains(n.as_str())))
        .or(only)
        .ok_or_else(|| format!("phone not paired: no Bluetooth audio device named like \"{name_hint}\" (pair it in Windows first)"))
}

/// What `core.rs` passes as the hint when the phone has told us no name at all.
pub const UNKNOWN_PHONE: &str = "phone";

#[cfg(test)]
mod pick_tests {
    use super::*;

    fn dev(id: &str, name: &str) -> (HSTRING, String) {
        (HSTRING::from(id), name.to_string())
    }

    #[test]
    fn the_phone_is_found_by_its_name() {
        let both = vec![dev("a", "Ethan's Galaxy S24 Ultra"), dev("b", "POCO X6 Pro 5G")];
        assert_eq!(pick_device(both, "POCO X6 Pro 5G").unwrap().1, "POCO X6 Pro 5G");
    }

    #[test]
    fn an_unpaired_phone_does_not_take_over_the_one_device_that_is_paired() {
        // 27 Oct 2026: the POCO was not paired, the S24 was the only source, and the app opened the S24.
        let only_s24 = vec![dev("a", "Ethan's Galaxy S24 Ultra")];
        let err = pick_device(only_s24, "POCO X6 Pro 5G").unwrap_err();
        assert!(err.contains("phone not paired"), "{err}");
    }

    #[test]
    fn a_phone_that_told_us_no_name_still_gets_the_lone_device() {
        let only = vec![dev("a", "Some Phone")];
        assert_eq!(pick_device(only, UNKNOWN_PHONE).unwrap().1, "Some Phone");
        let two = vec![dev("a", "One"), dev("b", "Two")];
        assert!(pick_device(two, UNKNOWN_PHONE).is_err());
    }
}
