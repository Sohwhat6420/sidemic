//! `true-phone-link.exe --probe <part> [times]`: calls one of the laptop watchers `times` times in a row and
//! prints how much private memory it left behind and how long each call took. This is how leaks and hot spots
//! in the 250 ms watcher loop are measured, one part at a time, without the UI running.

use std::time::Instant;

use windows::Win32::Media::Audio::eCapture;
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
use windows::Win32::System::Threading::GetCurrentProcess;

use crate::{wasapi, winrt};

pub const PARTS: &[&str] = &["find-cable", "mic-users", "sounds", "foreground", "bt-scan", "bt-watch", "tick-kept"];

thread_local! {
    static WATCH: std::cell::RefCell<(wasapi::Watch, winrt::MediaWatch)> = Default::default();
    static BT: Option<winrt::BtWatch> = {
        let w = winrt::BtWatch::start().ok();
        std::thread::sleep(std::time::Duration::from_millis(500));
        println!("bt-watch sees a paired source: {}", w.as_ref().is_some_and(winrt::BtWatch::any));
        w
    };
}

fn private_bytes() -> f64 {
    let mut c = PROCESS_MEMORY_COUNTERS_EX::default();
    unsafe {
        let _ = GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut c as *mut _ as *mut PROCESS_MEMORY_COUNTERS,
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        );
    }
    c.PrivateUsage as f64
}

fn call(part: &str) {
    match part {
        "find-cable" => {
            let _ = wasapi::find(eCapture, |n| n.contains("CABLE Output"));
        }
        "mic-users" => {
            let _ = wasapi::find(eCapture, |n| n.contains("CABLE Output")).and_then(|(d, _)| wasapi::active_sessions(&d));
        }
        "sounds" => {
            let _ = wasapi::default_render().and_then(|(d, _)| wasapi::sounding_sessions(&d));
        }
        "media" => {
            let _ = winrt::laptop_media();
        }
        "bt-scan" => {
            let _ = winrt::A2dp::devices();
        }
        "bt-watch" => BT.with(|w| {
            let _ = w.as_ref().map(winrt::BtWatch::any);
        }),
        "foreground" => {
            let _ = winrt::foreground();
        }
        "media-kept" => WATCH.with_borrow_mut(|(_, m)| {
            m.playing();
        }),
        // One watcher tick as the app runs it now, with the devices and the media manager kept.
        "tick-kept" => WATCH.with_borrow_mut(|(w, m)| {
            w.mic_users();
            w.sounding();
            m.playing();
            let _ = winrt::foreground();
        }),
        // One watcher tick as it used to run: everything looked up afresh.
        _ => {
            call("mic-users");
            call("sounds");
            call("media");
            call("foreground");
        }
    }
}

/// CPU time used by the whole process (every thread, including Windows' thread pool), in seconds.
pub fn cpu_seconds() -> f64 {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::GetProcessTimes;
    let (mut a, mut b, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    unsafe {
        let _ = GetProcessTimes(GetCurrentProcess(), &mut a, &mut b, &mut k, &mut u);
    }
    let t = |f: FILETIME| ((f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64) as f64 / 1e7;
    t(k) + t(u)
}

pub fn run(part: &str, times: u32) {
    wasapi::com_init();
    // Warm up so one-off allocations (COM, the WinRT factories) aren't counted as a leak.
    for _ in 0..20 {
        call(part);
    }
    let m0 = private_bytes();
    let c0 = cpu_seconds();
    let t0 = Instant::now();
    for _ in 0..times {
        call(part);
    }
    let per_call_us = t0.elapsed().as_secs_f64() * 1e6 / times as f64;
    let cpu_us = (cpu_seconds() - c0) * 1e6 / times as f64;
    let grew = private_bytes() - m0;
    println!(
        "{part}: {times} calls, {per_call_us:.0} us wall and {cpu_us:.0} us CPU per call, private memory {:+.1} MB ({:+.0} bytes per call)",
        grew / 1048576.0,
        grew / times as f64
    );
}
