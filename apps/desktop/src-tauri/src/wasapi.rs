//! Thin WASAPI helpers: find an endpoint by name, play into one, record from one, and list which processes have
//! an active audio session on one (how we see that Discord or a game has opened the microphone).

use windows::core::{Interface, Result, PWSTR};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, E_FAIL, S_OK};
use windows::Win32::Media::Audio::*;
use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::System::Com::*;
use windows::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION};

pub const RATE: u32 = 48_000;

/// Every thread that touches audio calls this once (multithreaded apartment).
pub fn com_init() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
}

fn enumerator() -> Result<IMMDeviceEnumerator> {
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
}

pub fn friendly_name(d: &IMMDevice) -> Result<String> {
    unsafe {
        let store = d.OpenPropertyStore(STGM_READ)?;
        Ok(store.GetValue(&PKEY_Device_FriendlyName)?.to_string())
    }
}

/// The first active endpoint whose name passes `pred`.
pub fn find(flow: EDataFlow, pred: impl Fn(&str) -> bool) -> Result<(IMMDevice, String)> {
    unsafe {
        let col = enumerator()?.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)?;
        for i in 0..col.GetCount()? {
            let d = col.Item(i)?;
            let name = friendly_name(&d)?;
            if pred(&name) {
                return Ok((d, name));
            }
        }
    }
    Err(E_FAIL.into())
}

pub fn default_render() -> Result<(IMMDevice, String)> {
    unsafe {
        let d = enumerator()?.GetDefaultAudioEndpoint(eRender, eConsole)?;
        let n = friendly_name(&d)?;
        Ok((d, n))
    }
}

fn float_format(channels: u16) -> WAVEFORMATEXTENSIBLE {
    WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_EXTENSIBLE as u16,
            nChannels: channels,
            nSamplesPerSec: RATE,
            nAvgBytesPerSec: RATE * 4 * channels as u32,
            nBlockAlign: 4 * channels,
            wBitsPerSample: 32,
            cbSize: 22,
        },
        Samples: WAVEFORMATEXTENSIBLE_0 { wValidBitsPerSample: 32 },
        dwChannelMask: if channels == 2 { 3 } else { 4 },
        SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
    }
}

fn open_client(d: &IMMDevice, buffer_ms: i64) -> Result<IAudioClient> {
    unsafe {
        let client: IAudioClient = d.Activate(CLSCTX_ALL, None)?;
        // Windows converts our 48 kHz float stereo to whatever the device runs at.
        let fmt = float_format(2);
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
            buffer_ms * 10_000,
            0,
            &fmt as *const _ as *const WAVEFORMATEX,
            None,
        )?;
        Ok(client)
    }
}

/// Plays mono 48 kHz float into an endpoint (both channels).
pub struct Render {
    client: IAudioClient,
    render: IAudioRenderClient,
    frames: u32,
}

impl Render {
    pub fn open(d: &IMMDevice) -> Result<Render> {
        unsafe {
            let client = open_client(d, 40)?;
            let render: IAudioRenderClient = client.GetService()?;
            let frames = client.GetBufferSize()?;
            client.Start()?;
            Ok(Render { client, render, frames })
        }
    }

    /// Frames queued in the device buffer that haven't played yet.
    pub fn queued(&self) -> u32 {
        unsafe { self.client.GetCurrentPadding().unwrap_or(self.frames) }
    }

    pub fn free(&self) -> u32 {
        self.frames.saturating_sub(self.queued())
    }

    pub fn write(&self, mono: &[f32]) -> Result<()> {
        let n = mono.len().min(self.free() as usize);
        if n == 0 {
            return Ok(());
        }
        unsafe {
            let p = self.render.GetBuffer(n as u32)? as *mut f32;
            let out = std::slice::from_raw_parts_mut(p, n * 2);
            for (i, s) in mono[..n].iter().enumerate() {
                out[2 * i] = *s;
                out[2 * i + 1] = *s;
            }
            self.render.ReleaseBuffer(n as u32, 0)
        }
    }
}

impl Drop for Render {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
        }
    }
}

/// Records an endpoint as mono 48 kHz float.
pub struct Capture {
    client: IAudioClient,
    capture: IAudioCaptureClient,
}

impl Capture {
    pub fn open(d: &IMMDevice) -> Result<Capture> {
        unsafe {
            let client = open_client(d, 40)?;
            let capture: IAudioCaptureClient = client.GetService()?;
            client.Start()?;
            Ok(Capture { client, capture })
        }
    }

    /// Appends whatever has arrived since the last call.
    pub fn read(&self, out: &mut Vec<f32>) -> Result<()> {
        unsafe {
            loop {
                if self.capture.GetNextPacketSize()? == 0 {
                    return Ok(());
                }
                let mut data = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                self.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                if flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 || data.is_null() {
                    out.extend(std::iter::repeat(0.0).take(frames as usize));
                } else {
                    let s = std::slice::from_raw_parts(data as *const f32, frames as usize * 2);
                    out.extend(s.chunks_exact(2).map(|c| 0.5 * (c[0] + c[1])));
                }
                self.capture.ReleaseBuffer(frames)?;
            }
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
        }
    }
}

/// Process ids with an *active* audio session on this endpoint (a capture endpoint: who is listening;
/// a render endpoint: who is making sound).
pub fn active_sessions(d: &IMMDevice) -> Result<Vec<u32>> {
    let mut pids = Vec::new();
    unsafe {
        let mgr: IAudioSessionManager2 = d.Activate(CLSCTX_ALL, None)?;
        let list = mgr.GetSessionEnumerator()?;
        for i in 0..list.GetCount()? {
            let s = list.GetSession(i)?;
            if s.GetState()? == AudioSessionStateActive {
                let s2: IAudioSessionControl2 = s.cast()?;
                // IsSystemSoundsSession is S_OK for the system sounds session (no real process); skip that one.
                if s2.IsSystemSoundsSession() != S_OK {
                    pids.push(s2.GetProcessId().unwrap_or(0));
                }
            }
        }
    }
    Ok(pids)
}

/// Process ids whose session on this render endpoint is actually making sound right now (peak above about
/// -50 dBFS). Plenty of apps keep a silent stream open all day, so "active" alone would duck the phone for ever.
pub fn sounding_sessions(d: &IMMDevice) -> Result<Vec<u32>> {
    let mut pids = Vec::new();
    unsafe {
        let mgr: IAudioSessionManager2 = d.Activate(CLSCTX_ALL, None)?;
        let list = mgr.GetSessionEnumerator()?;
        for i in 0..list.GetCount()? {
            let s = list.GetSession(i)?;
            if s.GetState()? != AudioSessionStateActive {
                continue;
            }
            let s2: IAudioSessionControl2 = s.cast()?;
            if s2.IsSystemSoundsSession() == S_OK {
                continue;
            }
            let Ok(meter) = s.cast::<windows::Win32::Media::Audio::Endpoints::IAudioMeterInformation>() else { continue };
            if meter.GetPeakValue().unwrap_or(0.0) > 0.003 {
                pids.push(s2.GetProcessId().unwrap_or(0));
            }
        }
    }
    Ok(pids)
}

/// Every active session on this endpoint as "name pid peak", for the TPL_DEBUG log.
pub fn session_debug(d: &IMMDevice) -> Result<Vec<String>> {
    let mut rows = Vec::new();
    unsafe {
        let mgr: IAudioSessionManager2 = d.Activate(CLSCTX_ALL, None)?;
        let list = mgr.GetSessionEnumerator()?;
        for i in 0..list.GetCount()? {
            let s = list.GetSession(i)?;
            if s.GetState()? != AudioSessionStateActive {
                continue;
            }
            let s2: IAudioSessionControl2 = s.cast()?;
            let pid = s2.GetProcessId().unwrap_or(0);
            let peak = s
                .cast::<windows::Win32::Media::Audio::Endpoints::IAudioMeterInformation>()
                .and_then(|m| m.GetPeakValue())
                .unwrap_or(-1.0);
            let sys = s2.IsSystemSoundsSession() == S_OK;
            rows.push(format!("{}#{pid} peak={peak:.4}{}", process_name(pid).unwrap_or_default(), if sys { " (system)" } else { "" }));
        }
    }
    Ok(rows)
}

/// "Discord.exe" for a process id, or None if it's gone or protected.
pub fn process_name(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(h);
        if !ok {
            return None;
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        path.rsplit('\\').next().map(str::to_owned)
    }
}
