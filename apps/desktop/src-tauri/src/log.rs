//! The run log: one plain-text file per day in `%APPDATA%\com.truephonelink.desktop\logs\`, kept for 14 days.
//! It records what the app saw (mic users, laptop and phone media, links), what it did and why, and any marks
//! Ethan made from the tray ("Mark a problem now"), so a week of real use can be read back afterwards.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use windows::Win32::System::SystemInformation::GetLocalTime;

const KEEP_DAYS: usize = 14;

struct Log {
    dir: PathBuf,
    day: String,
    file: Option<File>,
}

static LOG: Mutex<Option<Log>> = Mutex::new(None);

pub fn init(dir: PathBuf) {
    let _ = fs::create_dir_all(&dir);
    // Keep the newest KEEP_DAYS files; the names sort by date.
    if let Ok(rd) = fs::read_dir(&dir) {
        let mut files: Vec<PathBuf> =
            rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "log")).collect();
        files.sort();
        let excess = files.len().saturating_sub(KEEP_DAYS);
        for f in files.into_iter().take(excess) {
            let _ = fs::remove_file(f);
        }
    }
    *LOG.lock().unwrap() = Some(Log { dir, day: String::new(), file: None });
}

pub fn dir() -> Option<PathBuf> {
    LOG.lock().unwrap().as_ref().map(|l| l.dir.clone())
}

/// Appends one timestamped line (local time). Also echoed to stderr for dev runs.
pub fn line(text: impl AsRef<str>) {
    let t = unsafe { GetLocalTime() };
    let day = format!("{:04}-{:02}-{:02}", t.wYear, t.wMonth, t.wDay);
    let stamp = format!("{:02}:{:02}:{:02}.{:03}", t.wHour, t.wMinute, t.wSecond, t.wMilliseconds);
    let text = text.as_ref();
    eprintln!("{stamp} {text}");
    let mut guard = LOG.lock().unwrap();
    let Some(log) = guard.as_mut() else { return };
    if log.day != day || log.file.is_none() {
        log.day = day.clone();
        log.file = OpenOptions::new().create(true).append(true).open(log.dir.join(format!("{day}.log"))).ok();
    }
    if let Some(f) = log.file.as_mut() {
        let _ = writeln!(f, "{stamp} {text}");
    }
}
