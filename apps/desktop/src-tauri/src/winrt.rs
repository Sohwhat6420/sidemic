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

/// Phone media over Bluetooth A2DP, played on the laptop's default speaker (the Pebble). The laptop opens and
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
        // With one Bluetooth audio source paired, that is the phone whatever it is called.
        let devices_len = devices.len();
        let only = devices.first().cloned();
        let (id, name) = devices
            .into_iter()
            .find(|(_, n)| n.contains(name_hint) || (!n.is_empty() && name_hint.contains(n.as_str())))
            .or_else(|| (devices_len == 1).then(|| only.clone()).flatten())
            .ok_or_else(|| format!("no Bluetooth audio device named like \"{name_hint}\" (is the phone paired?)"))?;
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
