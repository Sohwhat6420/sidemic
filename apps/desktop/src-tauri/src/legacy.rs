//! Sidemic was "TRUE Phone Link" until 27 Oct 2026. Its installer now goes to `%LOCALAPPDATA%\Sidemic` and the
//! autostart plugin names its Run value after the product, so an upgraded machine would start two copies at sign-in
//! and keep a dead install folder. At the first start this removes the old "TRUE Phone Link" Run value and the files
//! of the old install folder (best effort). The data folder `%APPDATA%\com.truephonelink.desktop` keeps its name
//! (the identifier is not user-visible), so settings, `phone.json` and logs carry over untouched. The restart
//! watcher's own Run value is swapped by the watcher's installer (a script on the author's machine, not in this
//! repository), not here: while the old watcher is still registered the old folder is left alone, because that
//! watcher restarts the app from it.

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const OLD_FOLDER: &str = "TRUE Phone Link";
const OLD_RUN_VALUE: &str = "TRUE Phone Link";
/// The autostart plugin names its Run value after the product.
const RUN_VALUE: &str = "Sidemic";
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

/// The program path inside a Run value's data: `"C:\x\a.exe" --autostart` or `C:\x\a.exe --autostart`.
pub fn run_value_exe(data: &str) -> Option<PathBuf> {
    let d = data.trim();
    if let Some(rest) = d.strip_prefix('"') {
        return rest.split('"').next().filter(|p| !p.is_empty()).map(PathBuf::from);
    }
    // The first ".exe" that ends the path: followed by whitespace or the end, so a folder like `a.exeter` is not cut.
    let lower = d.to_ascii_lowercase();
    let end = lower.match_indices(".exe").map(|(i, m)| i + m.len()).find(|&e| d[e..].chars().next().is_none_or(char::is_whitespace))?;
    Some(PathBuf::from(&d[..end]))
}

/// True when a Run value's data starts some other program than `exe` (compared case-blind, ignoring a `\\?\`
/// prefix and forward slashes).
pub fn points_elsewhere(data: &str, exe: &Path) -> bool {
    match run_value_exe(data) {
        Some(target) => norm(&target) != norm(exe),
        None => true,
    }
}

fn norm(p: &Path) -> String {
    let real = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    real.to_string_lossy().replace('/', "\\").trim_start_matches(r"\\?\").to_ascii_lowercase()
}

/// When to rewrite the Run value `data` for the exe that is running now. Never on a mere difference: a release exe
/// started from a test folder (an A/B build, `target\release`) must not take sign-in away from a working install.
/// Repair only when the value's program no longer exists, or when this exe IS the real install (`install_exe`)
/// and so may take the value back from some other exe.
pub fn should_repair(data: &str, exe: &Path, install_exe: Option<&Path>) -> bool {
    if !points_elsewhere(data, exe) {
        return false;
    }
    let target_gone = run_value_exe(data).map_or(true, |t| !t.exists());
    let this_is_the_install = install_exe.is_some_and(|i| norm(i) == norm(exe));
    target_gone || this_is_the_install
}

/// The data of our own "Sidemic" Run value, or None when there is none (autostart off, or never set). Read through
/// the registry API: `reg query` writes the OEM code page, which mangles a non-ASCII profile path.
fn run_value_data() -> Option<String> {
    windows_registry::CURRENT_USER.open(r"Software\Microsoft\Windows\CurrentVersion\Run").ok()?.get_string(RUN_VALUE).ok()
}

/// Release starts only: true when our own "Sidemic" Run value exists and `should_repair` says to rewrite it for this
/// exe. A missing value is left alone, since that means the user turned start at sign-in off; the caller also
/// skips Task Manager's "Disabled" choice. The rewrite goes through the autostart plugin, which only ever touches
/// the "Sidemic" value.
pub fn run_value_is_stale() -> bool {
    if cfg!(debug_assertions) {
        return false;
    }
    let Ok(exe) = std::env::current_exe() else { return false };
    let install = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .map(|p| p.join("Sidemic").join("true-phone-link.exe"));
    match run_value_data() {
        Some(data) if should_repair(&data, &exe, install.as_deref()) => {
            crate::log::line(format!("legacy: the \"{RUN_VALUE}\" Run value pointed at {data}, not at this exe; rewriting it"));
            true
        }
        _ => false,
    }
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
    fn the_exe_is_read_out_of_a_run_value_quoted_or_not() {
        let a = r#""C:\Users\e\AppData\Local\Sidemic\true-phone-link.exe" --autostart"#;
        let b = r"C:\Users\e\AppData\Local\Sidemic\true-phone-link.exe --autostart";
        let want = PathBuf::from(r"C:\Users\e\AppData\Local\Sidemic\true-phone-link.exe");
        assert_eq!(run_value_exe(a), Some(want.clone()));
        assert_eq!(run_value_exe(b), Some(want));
        assert_eq!(run_value_exe(""), None);
        assert_eq!(run_value_exe("garbage"), None);
    }

    #[test]
    fn a_run_value_for_this_exe_is_left_alone_whatever_its_spelling() {
        let exe = Path::new(r"C:\Users\e\AppData\Local\Sidemic\true-phone-link.exe");
        assert!(!points_elsewhere(r#""C:\Users\e\AppData\Local\Sidemic\true-phone-link.exe" --autostart"#, exe));
        assert!(!points_elsewhere(r"c:/users/e/appdata/local/sidemic/TRUE-PHONE-LINK.EXE --autostart", exe));
    }

    #[test]
    fn a_run_value_for_another_exe_counts_as_pointing_elsewhere() {
        let exe = Path::new(r"C:\Users\e\AppData\Local\Sidemic\true-phone-link.exe");
        assert!(points_elsewhere(r"C:\Users\e\AppData\Local\TRUE Phone Link\true-phone-link.exe --autostart", exe));
        assert!(points_elsewhere(r"C:\Other\sidemic-test.exe --autostart", exe));
        assert!(points_elsewhere("", exe));
    }

    #[test]
    fn an_unquoted_path_with_spaces_is_read_whole() {
        let d = r"C:\Users\Jane Doe\AppData\Local\Sidemic\true-phone-link.exe --autostart";
        assert_eq!(run_value_exe(d), Some(PathBuf::from(r"C:\Users\Jane Doe\AppData\Local\Sidemic\true-phone-link.exe")));
    }

    #[test]
    fn a_folder_with_exe_inside_its_name_does_not_cut_the_path_short() {
        let d = r"C:\Users\a.exeter\AppData\Local\Sidemic\true-phone-link.exe --autostart";
        assert_eq!(run_value_exe(d), Some(PathBuf::from(r"C:\Users\a.exeter\AppData\Local\Sidemic\true-phone-link.exe")));
        assert_eq!(run_value_exe(r"C:\x\app.exe"), Some(PathBuf::from(r"C:\x\app.exe")));
    }

    #[test]
    fn a_value_whose_exe_is_gone_is_repaired_but_one_whose_exe_exists_is_not() {
        let dir = scratch("repair");
        let live_other = dir.join("sdm-ab.exe");
        std::fs::write(&live_other, b"x").unwrap();
        let running = dir.join("test-run.exe");
        std::fs::write(&running, b"x").unwrap();
        let gone = format!("{} --autostart", dir.join("deleted-install.exe").display());
        let exists = format!("{} --autostart", live_other.display());
        // a test exe running from some folder: fixes a dead value, never steals a working one
        assert!(should_repair(&gone, &running, None));
        assert!(!should_repair(&exists, &running, None));
        // the value already names this exe: nothing to do
        assert!(!should_repair(&format!("{} --autostart", running.display()), &running, None));
        // the real install takes the value back even from another exe that still exists
        assert!(should_repair(&exists, &running, Some(&running)));
        // an empty or unreadable value is repaired
        assert!(should_repair("", &running, None));
        // the production case: an install exists and differs from the running exe, whose own target still exists
        let install = dir.join("install.exe");
        std::fs::write(&install, b"x").unwrap();
        assert!(!should_repair(&exists, &running, Some(&install)));
        assert!(should_repair(&gone, &running, Some(&install)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_real_file_behind_a_different_spelling_is_not_stale() {
        let dir = scratch("runval");
        let exe = dir.join("true-phone-link.exe");
        std::fs::write(&exe, b"x").unwrap();
        let canon = std::fs::canonicalize(&exe).unwrap();
        assert!(!points_elsewhere(&format!("\"{}\" --autostart", canon.display()), &exe));
        let _ = std::fs::remove_dir_all(&dir);
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
