//! Keeps the app alive if its window thread stops answering. On 16 Oct the UI thread got stuck waiting on another
//! process (Windows logged a cross-process hang), the switching core stalled behind it, and the app stayed dead
//! until it was started by hand. Now a watcher pings the tray window every few seconds; after 30 s with no answer
//! it saves a minidump next to the logs (so the stuck call can be found) and starts a fresh copy of the app.

use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::time::{Duration, Instant};

use windows::core::BOOL;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, WPARAM};
use windows::Win32::System::Diagnostics::Debug::{
    MiniDumpWithThreadInfo, MiniDumpWithUnloadedModules, MiniDumpWriteDump, MINIDUMP_TYPE,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, OpenProcess, TerminateProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetWindowThreadProcessId, SendMessageTimeoutW, SMTO_ABORTIFHUNG, WM_NULL,
};

use crate::log;

pub const RESTART_ARG: &str = "--restart-after";
const HUNG_FOR: Duration = Duration::from_secs(30);
const KEEP_DUMPS: usize = 3;

/// A copy started by `restart` waits here for the stuck one to be gone, so the single-instance check doesn't
/// hand over to it.
pub fn wait_for_previous(args: &[String]) -> bool {
    let Some(pid) = args.iter().position(|a| a == RESTART_ARG).and_then(|i| args.get(i + 1)?.parse::<u32>().ok())
    else {
        return false;
    };
    unsafe {
        if let Ok(h) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
            WaitForSingleObject(h, 15_000);
            let _ = CloseHandle(h);
        }
    }
    true
}

/// The tray's message window, which lives on the UI thread.
fn tray_window() -> Option<HWND> {
    unsafe extern "system" fn each(h: HWND, found: LPARAM) -> BOOL {
        let mut pid = 0;
        unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
        if pid == unsafe { GetCurrentProcessId() } {
            let mut name = [0u16; 64];
            let n = unsafe { GetClassNameW(h, &mut name) } as usize;
            if String::from_utf16_lossy(&name[..n]) == "tray_icon_app" {
                unsafe { *(found.0 as *mut isize) = h.0 as isize };
                return false.into();
            }
        }
        true.into()
    }
    let mut found: isize = 0;
    unsafe {
        let _ = EnumWindows(Some(each), LPARAM(&mut found as *mut isize as isize));
    }
    (found != 0).then_some(HWND(found as *mut _))
}

fn answers(h: HWND) -> bool {
    let mut out = 0usize;
    unsafe { SendMessageTimeoutW(h, WM_NULL, WPARAM(0), LPARAM(0), SMTO_ABORTIFHUNG, 5_000, Some(&mut out)).0 != 0 }
}

pub fn watch() {
    std::thread::Builder::new()
        .name("tpl-hang".into())
        .spawn(|| {
            let tray = loop {
                std::thread::sleep(Duration::from_secs(5));
                if let Some(h) = tray_window() {
                    break h;
                }
            };
            let mut stuck_since: Option<Instant> = None;
            loop {
                std::thread::sleep(Duration::from_secs(5));
                if answers(tray) {
                    stuck_since = None;
                    continue;
                }
                let since = *stuck_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= HUNG_FOR {
                    restart();
                }
            }
        })
        .expect("hang watcher");
}

fn restart() {
    log::line("the window thread stopped answering for 30 s; saving a dump and restarting");
    if let Some(dir) = log::dir() {
        match dump(&dir) {
            Ok(name) => log::line(format!("hang dump: {name}")),
            Err(e) => log::line(format!("hang dump failed: {e}")),
        }
    }
    let pid = std::process::id().to_string();
    match std::env::current_exe().and_then(|exe| {
        std::process::Command::new(exe).args([crate::AUTOSTART_ARG, RESTART_ARG, pid.as_str()]).spawn()
    }) {
        Ok(_) => log::line("restarting"),
        Err(e) => log::line(format!("restart failed: {e}")),
    }
    // Not a normal exit: that would wait on the stuck thread.
    unsafe {
        let _ = TerminateProcess(GetCurrentProcess(), 1);
    }
}

fn dump(dir: &Path) -> Result<String, String> {
    let name = format!("hang-{}.dmp", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs());
    let file = std::fs::File::create(dir.join(&name)).map_err(|e| e.to_string())?;
    let kind = MINIDUMP_TYPE(MiniDumpWithThreadInfo.0 | MiniDumpWithUnloadedModules.0);
    unsafe {
        MiniDumpWriteDump(
            GetCurrentProcess(),
            GetCurrentProcessId(),
            HANDLE(file.as_raw_handle()),
            kind,
            None,
            None,
            None,
        )
        .map_err(|e| e.to_string())?;
    }
    // Keep only the newest few.
    if let Ok(rd) = std::fs::read_dir(dir) {
        let mut dumps: Vec<_> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "dmp"))
            .collect();
        dumps.sort();
        let excess = dumps.len().saturating_sub(KEEP_DUMPS);
        for d in dumps.into_iter().take(excess) {
            let _ = std::fs::remove_file(d);
        }
    }
    Ok(name)
}
