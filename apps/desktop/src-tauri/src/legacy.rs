//! Sidemic was "TRUE Phone Link" until 27 Oct 2026. Its installer now goes to `%LOCALAPPDATA%\Sidemic` and the
//! autostart plugin names its Run value after the product, so an upgraded machine would start two copies at sign-in
//! and keep a dead install folder. At the first start this removes the old "TRUE Phone Link" Run value and the files
//! of the old install folder (best effort). The data folder `%APPDATA%\com.truephonelink.desktop` keeps its name
//! (the identifier is not user-visible), so settings, `phone.json` and logs carry over untouched. The restart
//! watcher's own Run value is swapped by `tools/install-watcher.ps1`, not here: while the old watcher is still
//! registered the old folder is left alone, because that watcher restarts the app from it.

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const OLD_FOLDER: &str = "TRUE Phone Link";
const OLD_RUN_VALUE: &str = "TRUE Phone Link";
const OLD_WATCHER_VALUE: &str = "TRUE Phone Link watcher";
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const MARKER: &str = "legacy-cleaned";
/// What an old install put in its folder. Anything else (the `.bak-` rollback copy, `uninstall.exe` so that
/// Settings > Apps still works, files the user added) is left where it is.
const KNOWN: &[&str] = &["true-phone-link.exe", "tray.png", "tpl-watch.pyw", "phone"];

/// The old install folder under `local_app_data`, unless the running exe lives in it (then it is not stale).
/// Paths are compared as real paths when they exist, so a short 8.3 name or a `\\?\` prefix cannot hide it.
pub fn stale_folder(exe: &Path, local_app_data: &Path) -> Option<PathBuf> {
    let old = local_app_data.join(OLD_FOLDER);
    let real = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let (exe, old_real) = (real(exe), real(&old));
    let lower = |p: &Path| p.to_string_lossy().to_ascii_lowercase();
    let inside = lower(&exe).starts_with(&lower(&old_real));
    (!inside).then_some(old)
}

/// Deletes only the files an old install is known to have made, then the folder itself if that left it empty.
/// Returns one log line per thing that could not be removed or was kept on purpose.
pub fn clean_old_folder(old: &Path) -> Vec<String> {
    let mut notes = Vec::new();
    for name in KNOWN {
        let p = old.join(name);
        let r = match std::fs::symlink_metadata(&p) {
            Err(_) => continue,
            Ok(m) if m.is_dir() => std::fs::remove_dir_all(&p),
            Ok(_) => std::fs::remove_file(&p),
        };
        if let Err(e) = r {
            notes.push(format!("{name} stays ({e})"));
        }
    }
    match std::fs::remove_dir(old) {
        Ok(()) => {}
        Err(_) => {
            let left: Vec<String> = std::fs::read_dir(old)
                .map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
                .unwrap_or_default();
            if !left.is_empty() {
                notes.push(format!("folder kept, still holds: {}", left.join(", ")));
            }
        }
    }
    notes
}

fn reg(args: &[&str]) -> bool {
    Command::new("reg")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Runs once per data folder. Returns true when it removed an old "TRUE Phone Link" Run value, i.e. the old install
/// started at sign-in, so the caller must register the new "Sidemic" value (the `autostart_set` setting carries
/// over in the same data folder and would otherwise never write it).
pub fn migrate(data_dir: &Path) -> bool {
    let marker = data_dir.join(MARKER);
    // A dev build runs from target\debug, which would look like "somewhere else" and delete the live install.
    if cfg!(debug_assertions) || marker.exists() {
        return false;
    }
    let removed = reg(&["delete", RUN_KEY, "/v", OLD_RUN_VALUE, "/f"]);
    crate::log::line(format!(
        "legacy: old \"{OLD_RUN_VALUE}\" Run value {}",
        if removed { "removed" } else { "not removed (absent, or the registry refused)" }
    ));
    if reg(&["query", RUN_KEY, "/v", OLD_WATCHER_VALUE]) {
        // Not marked done: the next start retries once install-watcher.ps1 has replaced the old watcher.
        crate::log::line("legacy: the old restart watcher is still registered, so the old install folder is left alone for now");
        return removed;
    }
    if let (Ok(exe), Some(local)) = (std::env::current_exe(), std::env::var_os("LOCALAPPDATA")) {
        let local = PathBuf::from(local);
        if local.is_absolute() {
            if let Some(old) = stale_folder(&exe, &local) {
                if old.is_dir() {
                    let notes = clean_old_folder(&old);
                    if notes.is_empty() {
                        crate::log::line(format!("legacy: removed the old install folder {}", old.display()));
                    }
                    for n in notes {
                        crate::log::line(format!("legacy: {} {n}", old.display()));
                    }
                }
            }
        }
    }
    let _ = std::fs::write(marker, "the TRUE Phone Link leftovers were dealt with");
    removed
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

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sidemic-legacy-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_real_path_inside_the_old_folder_is_found_even_through_a_different_spelling() {
        let local = scratch("spell");
        let old = local.join(OLD_FOLDER);
        std::fs::create_dir_all(&old).unwrap();
        let exe = old.join("true-phone-link.exe");
        std::fs::write(&exe, b"x").unwrap();
        // the canonical form carries a \\?\ prefix; the plain form must still count as inside
        let canon = std::fs::canonicalize(&exe).unwrap();
        assert_eq!(stale_folder(&canon, &local), None);
        assert_eq!(stale_folder(&exe, &local), None);
        let _ = std::fs::remove_dir_all(&local);
    }

    #[test]
    fn only_the_known_files_go_and_everything_else_stays() {
        let local = scratch("known");
        let old = local.join(OLD_FOLDER);
        std::fs::create_dir_all(old.join("phone")).unwrap();
        std::fs::write(old.join("phone").join("tpl-server.jar"), b"j").unwrap();
        for f in ["true-phone-link.exe", "tray.png", "tpl-watch.pyw", "true-phone-link.exe.bak-2026-10-17", "uninstall.exe"] {
            std::fs::write(old.join(f), b"x").unwrap();
        }
        let notes = clean_old_folder(&old);
        assert!(!old.join("true-phone-link.exe").exists());
        assert!(!old.join("phone").exists());
        assert!(!old.join("tray.png").exists());
        assert!(!old.join("tpl-watch.pyw").exists());
        assert!(old.join("true-phone-link.exe.bak-2026-10-17").exists(), "the rollback copy must stay");
        assert!(old.join("uninstall.exe").exists(), "the uninstaller must stay so Settings > Apps works");
        assert!(old.exists());
        assert!(notes.iter().any(|n| n.contains("folder kept")), "{notes:?}");
        let _ = std::fs::remove_dir_all(&local);
    }

    #[test]
    fn a_folder_holding_only_known_files_is_removed_whole() {
        let local = scratch("whole");
        let old = local.join(OLD_FOLDER);
        std::fs::create_dir_all(old.join("phone")).unwrap();
        std::fs::write(old.join("true-phone-link.exe"), b"x").unwrap();
        assert!(clean_old_folder(&old).is_empty());
        assert!(!old.exists());
        let _ = std::fs::remove_dir_all(&local);
    }
}
