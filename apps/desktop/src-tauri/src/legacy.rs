//! Sidemic was "TRUE Phone Link" until 27 Oct 2026. Its installer now goes to `%LOCALAPPDATA%\Sidemic` and the
//! autostart plugin names its Run value after the product, so an upgraded machine would start two copies at sign-in
//! and keep a dead install folder. At the first start this removes the old "TRUE Phone Link" Run value and the old
//! install folder (best effort; a file still in use stays, and the next start does not retry once the marker exists).
//! The data folder `%APPDATA%\com.truephonelink.desktop` keeps its name (the identifier is not user-visible), so
//! settings, `phone.json` and logs carry over untouched. The restart watcher's own Run value is swapped by
//! `tools/install-watcher.ps1`, not here.

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const OLD_FOLDER: &str = "TRUE Phone Link";
const OLD_RUN_VALUE: &str = "TRUE Phone Link";
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const MARKER: &str = "legacy-cleaned";

/// The old install folder under `local_app_data`, unless the running exe lives in it (then it is not stale).
pub fn stale_folder(exe: &Path, local_app_data: &Path) -> Option<PathBuf> {
    let old = local_app_data.join(OLD_FOLDER);
    let inside = exe.to_string_lossy().to_ascii_lowercase().starts_with(&old.to_string_lossy().to_ascii_lowercase());
    (!inside).then_some(old)
}

/// Runs once per data folder: removes the old Run value and the old install folder, logging each outcome.
pub fn migrate(data_dir: &Path) {
    let marker = data_dir.join(MARKER);
    // A dev build runs from target\debug, which would look like "somewhere else" and delete the live install.
    if cfg!(debug_assertions) || marker.exists() {
        return;
    }
    let removed = Command::new("reg")
        .args(["delete", RUN_KEY, "/v", OLD_RUN_VALUE, "/f"])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    crate::log::line(format!(
        "legacy: old \"{OLD_RUN_VALUE}\" Run value {}",
        if removed { "removed" } else { "not present" }
    ));
    if let (Ok(exe), Ok(local)) = (std::env::current_exe(), std::env::var("LOCALAPPDATA")) {
        if let Some(old) = stale_folder(&exe, Path::new(&local)) {
            if old.exists() {
                // The watcher script runs from here until install-watcher.ps1 moves it, so a failure is normal.
                match std::fs::remove_dir_all(&old) {
                    Ok(()) => crate::log::line(format!("legacy: removed the old install folder {}", old.display())),
                    Err(e) => crate::log::line(format!("legacy: old install folder {} stays ({e})", old.display())),
                }
            }
        }
    }
    let _ = std::fs::write(marker, "the TRUE Phone Link leftovers were dealt with");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_old_folder_is_stale_when_the_exe_is_elsewhere() {
        let local = Path::new(r"C:\Users\e\AppData\Local");
        let exe = Path::new(r"C:\Users\e\AppData\Local\Sidemic\true-phone-link.exe");
        assert_eq!(stale_folder(exe, local), Some(local.join("TRUE Phone Link")));
    }

    #[test]
    fn a_copy_still_running_from_the_old_folder_is_left_alone() {
        let local = Path::new(r"C:\Users\e\AppData\Local");
        let exe = Path::new(r"c:\users\e\appdata\local\true phone link\true-phone-link.exe");
        assert_eq!(stale_folder(exe, local), None);
    }
}
