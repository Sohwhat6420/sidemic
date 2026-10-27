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
    /// The paired source the open link goes to.
    name: String,
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

    /// Name of the source the open link goes to (empty when none is open).
    pub fn open_name(&self) -> &str {
        &self.name
    }

    /// Opens the link to the paired source `hint` describes (e.g. "Galaxy S24 Ultra"); never to another phone.
    pub fn open(&mut self, hint: &Hint) -> std::result::Result<String, String> {
        if self.conn.is_some() {
            return Ok("already open".into());
        }
        let devices = Self::devices().map_err(|e| e.to_string())?;
        let (id, name) = pick_device(devices, hint)?;
        let conn = AudioPlaybackConnection::TryCreateFromId(&id).map_err(|e| e.to_string())?;
        conn.StartAsync().and_then(|a| a.join()).map_err(|e| e.to_string())?;
        let result = conn.OpenAsync().and_then(|a| a.join()).map_err(|e| e.to_string())?;
        let status = result.Status().map_err(|e| e.to_string())?;
        if status != AudioPlaybackConnectionOpenResultStatus::Success {
            return Err(format!("Bluetooth audio from {name} didn't open ({status:?}); is the phone in range?"));
        }
        self.conn = Some(conn);
        self.name = name.clone();
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
        self.name.clear();
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
    /// Paired source id -> name.
    ids: std::sync::Arc<std::sync::Mutex<std::collections::BTreeMap<String, String>>>,
}

impl BtWatch {
    pub fn start() -> Result<BtWatch> {
        use windows::Devices::Enumeration::{DeviceInformationUpdate, DeviceWatcher};
        let watcher = DeviceInformation::CreateWatcherAqsFilter(&AudioPlaybackConnection::GetDeviceSelector()?)?;
        let ids = std::sync::Arc::new(std::sync::Mutex::new(std::collections::BTreeMap::new()));
        let added = ids.clone();
        watcher.Added(&TypedEventHandler::<DeviceWatcher, DeviceInformation>::new(move |_, d| {
            if let Ok(d) = d.ok() {
                added.lock().unwrap().insert(d.Id()?.to_string(), d.Name()?.to_string());
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

    /// Whether the phone `hint` describes is among the paired sources right now.
    pub fn has(&self, hint: &Hint) -> bool {
        let map = self.ids.lock().unwrap();
        let names: Vec<&str> = map.values().map(String::as_str).collect();
        hint.pick(&names).is_some()
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

/// Who to look for among the paired Bluetooth audio sources: the names the phone is known by. A paired source
/// matches when its name contains one of them, or one contains its name. There is deliberately no "take the one
/// paired source whatever it is" fallback: on 27 Oct 2026 the POCO was unpaired, the S24 was the only source, and
/// the app opened the S24. An empty hint (no phone has introduced itself) matches nothing.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Hint {
    pub names: Vec<String>,
}

impl Hint {
    /// Index of the paired source this hint means, among the sources' names.
    pub fn pick(&self, names: &[&str]) -> Option<usize> {
        names.iter().position(|n| self.matches(n))
    }

    pub fn matches(&self, name: &str) -> bool {
        self.names.iter().any(|h| name.contains(h.as_str()) || (!name.is_empty() && h.contains(name)))
    }

    fn describe(&self) -> String {
        if self.names.is_empty() {
            "no phone has introduced itself yet".into()
        } else {
            format!("no Bluetooth audio device named like {:?}", self.names)
        }
    }
}

/// Which paired Bluetooth audio source is the phone `hint` describes, or "phone not paired".
pub fn pick_device(devices: Vec<(HSTRING, String)>, hint: &Hint) -> std::result::Result<(HSTRING, String), String> {
    let names: Vec<&str> = devices.iter().map(|(_, n)| n.as_str()).collect();
    match hint.pick(&names) {
        Some(i) => Ok(devices[i].clone()),
        None => Err(format!("phone not paired: {} (pair it in Windows first)", hint.describe())),
    }
}

#[cfg(test)]
mod pick_tests {
    use super::*;

    fn dev(id: &str, name: &str) -> (HSTRING, String) {
        (HSTRING::from(id), name.to_string())
    }

    fn hint(names: &[&str]) -> Hint {
        Hint { names: names.iter().map(|s| s.to_string()).collect() }
    }

    #[test]
    fn the_phone_is_found_by_its_name() {
        let both = vec![dev("a", "Ethan's Galaxy S24 Ultra"), dev("b", "POCO X6 Pro 5G")];
        assert_eq!(pick_device(both, &hint(&["POCO X6 Pro 5G"])).unwrap().1, "POCO X6 Pro 5G");
    }

    #[test]
    fn an_unpaired_phone_does_not_take_over_the_one_device_that_is_paired() {
        // 27 Oct 2026: the POCO was not paired, the S24 was the only source, and the app opened the S24.
        let only_s24 = vec![dev("a", "Ethan's Galaxy S24 Ultra")];
        let err = pick_device(only_s24, &hint(&["POCO X6 Pro 5G", "23122PCD1G"])).unwrap_err();
        assert!(err.contains("phone not paired"), "{err}");
    }

    #[test]
    fn nothing_is_opened_before_a_phone_has_introduced_itself() {
        let only_s24 = vec![dev("a", "Ethan's Galaxy S24 Ultra")];
        let err = pick_device(only_s24, &Hint::default()).unwrap_err();
        assert!(err.contains("phone not paired") && err.contains("introduced"), "{err}");
    }

    #[test]
    fn any_known_name_may_match() {
        let h = hint(&["Ethan's POCO", "POCO X6"]);
        assert_eq!(h.pick(&["Galaxy S24", "POCO X6 Pro 5G"]), Some(1));
        assert_eq!(h.pick(&[]), None);
    }
}
