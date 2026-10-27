//! Sidemic for Windows: a tray app. Left-click the tray icon for the flyout; the full window opens from
//! there. Everything that decides or touches audio lives in `core`; this file is the windows and the tray.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod a2dp_sm;
mod adb;
mod core;
mod echo;
mod hang;
mod legacy;
mod log;
mod mic;
mod phone;
mod probe;
mod wasapi;
mod winrt;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::window::{Effect, EffectsBuilder};
use tauri::{
    AppHandle, Emitter, LogicalSize, Manager, PhysicalPosition, RunEvent, Theme, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder, WindowEvent,
};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};

const FLYOUT_WIDTH: f64 = 360.0;
const AUTOSTART_ARG: &str = "--autostart";
/// Written to the data dir when Quit is chosen from the tray, removed at start-up (see `tools/tpl-watch.pyw`).
const QUIT_FLAG: &str = "quit.flag";

/// WebView2's arguments for both windows (they share one browser process, so they must match): wry's defaults
/// plus no GPU process. The GPU process held 76 MB at start and about 200 MB after a day for two small, mostly
/// hidden pages; software drawing renders them the same (checked 16 Oct).
const WEBVIEW_ARGS: &str = "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --disable-gpu";

/// The flyout is unloaded once it has been closed this long (Ethan's choice, 16 Oct): with no page open,
/// WebView2's six processes exit and the app idles at about 12 MB instead of about 106 MB. Opening it again
/// after that takes about half a second.
const FLYOUT_UNLOAD_AFTER: Duration = Duration::from_secs(120);

/// When the flyout last hid itself on losing focus. A tray click also takes focus away, so a click that lands
/// just after that hide means "close", not "open again".
static LAST_HIDE: Mutex<Option<Instant>> = Mutex::new(None);

/// When the flyout was last shown or hidden, for unloading it.
static LAST_USED: Mutex<Option<Instant>> = Mutex::new(None);

/// Set while a freshly loaded flyout waits for its first drawn frame; `flyout_height` then shows it, so it never
/// appears blank.
static SHOW_WHEN_READY: AtomicBool = AtomicBool::new(false);

fn touch_flyout() {
    *LAST_USED.lock().unwrap() = Some(Instant::now());
}

#[tauri::command]
fn state(h: tauri::State<core::Handle>) -> Value {
    h.snapshot.lock().unwrap().clone()
}

#[tauri::command]
fn accent() -> Value {
    accent_json()
}

#[tauri::command]
fn act(h: tauri::State<core::Handle>, action: String, arg: Option<String>) {
    let _ = h.tx.send(core::Msg::Act { action, arg });
}

// Async on purpose: a synchronous command that builds a window deadlocks on Windows (the build waits for the
// event loop, which is waiting for the command). The first open from the flyout froze the app that way.
#[tauri::command]
async fn open_window(app: AppHandle, page: String) {
    show_main(&app, &page);
}

#[tauri::command]
fn hide_flyout(app: AppHandle) {
    if let Some(w) = app.get_webview_window("flyout") {
        let _ = w.hide();
        touch_flyout();
    }
}

#[tauri::command]
fn flyout_height(app: AppHandle, height: f64) {
    if let Some(w) = app.get_webview_window("flyout") {
        let size = LogicalSize::new(FLYOUT_WIDTH, height.ceil());
        let _ = w.set_size(size);
        place_flyout(&w);
        if SHOW_WHEN_READY.swap(false, Ordering::SeqCst) {
            // A borderless window sized before it was ever shown comes out about 30 px too tall, and the page
            // only reports its height again when the state changes, so size it once more now it is on screen.
            let _ = w.show();
            let _ = w.set_size(size);
            place_flyout(&w);
            let _ = w.set_focus();
            touch_flyout();
        }
    }
}

/// The Windows accent palette for the UI. The UI is dark-themed, so it always takes the dark-theme mapping.
fn accent_json() -> Value {
    match windows::UI::ViewManagement::UISettings::new().and_then(|ui| winrt::accent(&ui)) {
        Ok((palette, _)) => json!({"palette": palette, "dark": true}),
        Err(_) => json!(null),
    }
}

fn show_main(app: &AppHandle, page: &str) {
    if let Some(f) = app.get_webview_window("flyout") {
        let _ = f.hide();
        touch_flyout();
    }
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        let _ = w.emit_to("main", "page", page);
        return;
    }
    let url = WebviewUrl::App(format!("window.html?page={page}").into());
    let built = WebviewWindowBuilder::new(app, "main", url)
        .title("Sidemic")
        .inner_size(1000.0, 680.0)
        .min_inner_size(720.0, 480.0)
        .transparent(true)
        .theme(Some(Theme::Dark))
        .effects(EffectsBuilder::new().effect(Effect::Mica).build())
        .additional_browser_args(WEBVIEW_ARGS)
        .center()
        .build();
    if let Err(e) = built {
        eprintln!("main window: {e}");
    }
}

fn build_flyout(app: &AppHandle) -> tauri::Result<WebviewWindow> {
    let w = WebviewWindowBuilder::new(app, "flyout", WebviewUrl::App("flyout.html".into()))
        .title("Sidemic")
        .inner_size(FLYOUT_WIDTH, 420.0)
        .decorations(false)
        .transparent(true)
        .resizable(false)
        .maximizable(false)
        .minimizable(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .shadow(true)
        .visible(false)
        .theme(Some(Theme::Dark))
        .effects(EffectsBuilder::new().effect(Effect::Acrylic).build())
        .additional_browser_args(WEBVIEW_ARGS)
        .build()?;
    touch_flyout();
    round_corners(&w);
    // The flyout only exists while it has focus. If it is ever up without focus (Windows refused the focus, or
    // something showed it), it goes away within half a second instead of sitting on the screen.
    // Asks Windows directly: Tauri's own visible/focused flags don't see a window shown from outside.
    if let Ok(h) = w.hwnd() {
        let hwnd = h.0 as isize;
        std::thread::spawn(move || {
            use windows::Win32::Foundation::HWND;
            use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, IsWindow, IsWindowVisible, ShowWindow, SW_HIDE};
            let hwnd = HWND(hwnd as _);
            let mut unfocused = 0;
            loop {
                std::thread::sleep(Duration::from_millis(150));
                // The flyout was unloaded: this watchdog goes with it (a new one starts with the next flyout).
                if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
                    return;
                }
                let (visible, focused) = unsafe { (IsWindowVisible(hwnd).as_bool(), GetForegroundWindow() == hwnd) };
                if visible && !focused {
                    unfocused += 1;
                    if unfocused >= 3 {
                        unsafe {
                            let _ = ShowWindow(hwnd, SW_HIDE);
                        }
                        touch_flyout();
                        unfocused = 0;
                    }
                } else {
                    unfocused = 0;
                }
            }
        });
    }
    let me = w.clone();
    w.on_window_event(move |e| {
        if let WindowEvent::Focused(false) = e {
            if os_visible(&me) {
                let _ = me.hide();
                *LAST_HIDE.lock().unwrap() = Some(Instant::now());
                touch_flyout();
            }
        }
    });
    Ok(w)
}

/// Whether Windows shows the window (Tauri's own flag misses a hide done by the watchdog).
fn os_visible(w: &WebviewWindow) -> bool {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::IsWindowVisible;
    w.hwnd().is_ok_and(|h| unsafe { IsWindowVisible(HWND(h.0 as _)).as_bool() })
}

/// Windows 11 rounds undecorated windows only when asked.
fn round_corners(w: &WebviewWindow) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND};
    if let Ok(h) = w.hwnd() {
        let pref = DWMWCP_ROUND;
        unsafe {
            let _ = DwmSetWindowAttribute(
                HWND(h.0 as _),
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &pref as *const _ as *const _,
                std::mem::size_of_val(&pref) as u32,
            );
        }
    }
}

/// Bottom-right of the work area, above the taskbar, like the Windows quick settings flyout.
fn place_flyout(w: &WebviewWindow) {
    let Ok(Some(m)) = w.primary_monitor() else { return };
    let area = m.work_area();
    let Ok(size) = w.outer_size() else { return };
    let margin = (12.0 * m.scale_factor()) as i32;
    let x = area.position.x + area.size.width as i32 - size.width as i32 - margin;
    let y = area.position.y + area.size.height as i32 - size.height as i32 - margin;
    let _ = w.set_position(PhysicalPosition::new(x, y));
}

fn toggle_flyout(app: &AppHandle) {
    let Some(w) = app.get_webview_window("flyout") else {
        // Not loaded (unloaded after a while closed, or never opened yet): load it; it shows once drawn.
        SHOW_WHEN_READY.store(true, Ordering::SeqCst);
        if let Err(e) = build_flyout(app) {
            SHOW_WHEN_READY.store(false, Ordering::SeqCst);
            log::line(format!("flyout: {e}"));
        }
        return;
    };
    if SHOW_WHEN_READY.load(Ordering::SeqCst) {
        return; // still loading from the last click
    }
    if os_visible(&w) {
        let _ = w.hide();
        touch_flyout();
        return;
    }
    if LAST_HIDE.lock().unwrap().is_some_and(|t| t.elapsed() < Duration::from_millis(300)) {
        return; // this click is what just closed it
    }
    place_flyout(&w);
    let _ = w.show();
    let _ = w.set_focus();
    touch_flyout();
}

/// Unloads the flyout once it has been closed for `FLYOUT_UNLOAD_AFTER`.
fn unload_idle_flyout(app: AppHandle) {
    std::thread::Builder::new()
        .name("tpl-unload".into())
        .spawn(move || loop {
            std::thread::sleep(Duration::from_secs(10));
            let Some(w) = app.get_webview_window("flyout") else { continue };
            let idle = LAST_USED.lock().unwrap().is_some_and(|t| t.elapsed() >= FLYOUT_UNLOAD_AFTER);
            if idle && !os_visible(&w) && !SHOW_WHEN_READY.load(Ordering::SeqCst) {
                let _ = w.destroy();
            }
        })
        .expect("unload thread");
}

// The core calls these two from its own thread. They are posted to the UI thread rather than waited on, so the
// switching core keeps running even if the UI thread is ever stuck (16 Oct: it was, and the core stalled with it).
pub fn set_tray_tooltip(app: &AppHandle, text: &str) {
    let (app2, text) = (app.clone(), text.to_owned());
    let _ = app.run_on_main_thread(move || {
        if let Some(t) = app2.tray_by_id("tray") {
            let _ = t.set_tooltip(Some(&text));
        }
    });
}

/// The tray menu's Pause item, so its label can follow the state.
struct PauseItem(MenuItem<tauri::Wry>);

pub fn set_pause_label(app: &AppHandle, paused: bool) {
    let app2 = app.clone();
    let _ = app.run_on_main_thread(move || {
        if let Some(p) = app2.try_state::<PauseItem>() {
            let _ = p.0.set_text(if paused { "Resume Sidemic" } else { "Pause Sidemic" });
        }
    });
}

/// One row of the tray's Phones submenu.
#[derive(Clone)]
pub struct PhoneMenuRow {
    pub id: String,
    pub label: String,
    pub ready: bool,
    pub active: bool,
}

/// The tray's Phones submenu, rebuilt whenever the list of phones changes.
struct PhonesMenu(Submenu<tauri::Wry>);

/// The latest rows, kept so a submenu built after the core first published (setup order) still gets them.
static PHONES: Mutex<Option<(Vec<PhoneMenuRow>, bool)>> = Mutex::new(None);

/// Posted to the UI thread like the other tray updates.
pub fn set_phones_menu(app: &AppHandle, rows: Vec<PhoneMenuRow>, auto_switch: bool) {
    *PHONES.lock().unwrap() = Some((rows, auto_switch));
    apply_phones_menu(app);
}

fn apply_phones_menu(app: &AppHandle) {
    let app2 = app.clone();
    let _ = app.run_on_main_thread(move || {
        let Some(menu) = app2.try_state::<PhonesMenu>() else { return };
        let Some((rows, auto_switch)) = PHONES.lock().unwrap().clone() else { return };
        if let Ok(old) = menu.0.items() {
            for item in old {
                let _ = menu.0.remove(&item);
            }
        }
        if rows.is_empty() {
            if let Ok(none) = MenuItem::with_id(&app2, "phones-none", "No phone is ready", false, None::<&str>) {
                let _ = menu.0.append(&none);
            }
        }
        for r in &rows {
            let label = if r.ready { r.label.clone() } else { format!("{} (not reachable)", r.label) };
            if let Ok(item) = CheckMenuItem::with_id(&app2, format!("phone:{}", r.id), label, r.ready, r.active, None::<&str>) {
                let _ = menu.0.append(&item);
            }
        }
        if let Ok(sep) = PredefinedMenuItem::separator(&app2) {
            let _ = menu.0.append(&sep);
        }
        if let Ok(auto) =
            CheckMenuItem::with_id(&app2, "auto-switch", "Auto-switch to the last-used ready phone", true, auto_switch, None::<&str>)
        {
            let _ = menu.0.append(&auto);
        }
    });
}

pub fn autostart_enabled(app: &AppHandle) -> bool {
    app.autolaunch().is_enabled().unwrap_or(false)
}

pub fn set_autostart(app: &AppHandle, on: bool) {
    let r = if on { app.autolaunch().enable() } else { app.autolaunch().disable() };
    if let Err(e) = r {
        eprintln!("autostart: {e}");
    }
}

/// The jar and voice library pushed to the phone: bundled resources when installed, the build output in dev.
fn phone_dir(app: &AppHandle) -> PathBuf {
    let dev = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../phone-server/out");
    // A dev build's resource copy is frozen at its last cargo build; the phone server rebuilds separately.
    if cfg!(debug_assertions) && dev.join("tpl-server.jar").exists() {
        return dev;
    }
    if let Ok(r) = app.path().resource_dir() {
        let p = r.join("phone");
        if p.join("tpl-server.jar").exists() {
            return p;
        }
    }
    dev
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--probe") {
        let part = args.get(2).map_or("tick", String::as_str);
        let times = args.get(3).and_then(|n| n.parse().ok()).unwrap_or(2000);
        if part == "all" {
            probe::PARTS.iter().for_each(|p| probe::run(p, times));
        } else {
            probe::run(part, times);
        }
        return;
    }
    let restarted = hang::wait_for_previous(&args);
    // Tauri's async runtime starts one worker per logical core (20 here) for a handful of async commands; two
    // are plenty.
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("tokio runtime");
    tauri::async_runtime::set(rt.handle().clone());
    std::mem::forget(rt);
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| show_main(app, "home")))
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, Some(vec![AUTOSTART_ARG])))
        .invoke_handler(tauri::generate_handler![state, accent, act, open_window, flyout_height, hide_flyout])
        .setup(move |app| {
            let handle = app.handle().clone();
            let data = app.path().app_data_dir()?;
            log::init(data.join("logs"));
            let _ = std::fs::remove_file(data.join(QUIT_FLAG));
            let at_sign_in = std::env::args().any(|a| a == AUTOSTART_ARG);
            log::line(format!(
                "Sidemic {} started{}",
                env!("CARGO_PKG_VERSION"),
                if restarted {
                    " again after the window stopped answering"
                } else if at_sign_in {
                    " at sign-in"
                } else {
                    ""
                }
            ));
            // The old install started at sign-in: register the new "Sidemic" Run value, since the stored
            // `autostart_set` setting carries over and would never write it.
            // A test install can leave the "Sidemic" value pointing at an exe that is gone: repoint it.
            // (A value the user removed, or turned off in Task Manager, makes autostart_enabled false: left alone.)
            let migrated = legacy::migrate(&data);
            if migrated || (autostart_enabled(&handle) && legacy::run_value_is_stale()) {
                set_autostart(&handle, true);
            }
            app.manage(core::start(handle.clone(), data, phone_dir(&handle)));
            hang::watch();

            // The flyout loads on the first tray click, not at sign-in, and unloads when left closed.
            unload_idle_flyout(handle.clone());

            let open = MenuItem::with_id(app, "open", "Open Sidemic", true, None::<&str>)?;
            let mark = MenuItem::with_id(app, "mark", "Mark a problem now", true, None::<&str>)?;
            let logs = MenuItem::with_id(app, "logs", "Open the log folder", true, None::<&str>)?;
            let pause = MenuItem::with_id(app, "pause", "Pause Sidemic", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let phones = Submenu::with_id(app, "phones", "Phones", true)?;
            let menu = Menu::with_items(app, &[&open, &phones, &pause, &mark, &logs, &quit])?;
            app.manage(PauseItem(pause));
            app.manage(PhonesMenu(phones));
            apply_phones_menu(app.handle());
            // The brain may have published before the item existed (started paused): catch up from its snapshot.
            let paused = app.state::<core::Handle>().snapshot.lock().unwrap()["paused"].as_bool() == Some(true);
            set_pause_label(app.handle(), paused);
            TrayIconBuilder::with_id("tray")
                .icon(Image::from_bytes(include_bytes!("../icons/tray.png"))?)
                .tooltip("Sidemic")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, e| match e.id.as_ref() {
                    "open" => show_main(app, "home"),
                    "mark" | "pause" => {
                        let action = if e.id.as_ref() == "mark" { "mark" } else { "toggle_pause" };
                        let h = app.state::<core::Handle>();
                        let _ = h.tx.send(core::Msg::Act { action: action.into(), arg: None });
                    }
                    "logs" => {
                        if let Some(d) = log::dir() {
                            let _ = std::process::Command::new("explorer.exe").arg(d).spawn();
                        }
                    }
                    "quit" => {
                        // Tells the restart watcher (tools/tpl-watch.pyw) this stop is deliberate.
                        if let Ok(d) = app.path().app_data_dir() {
                            let _ = std::fs::write(d.join(QUIT_FLAG), "quit from the tray");
                        }
                        app.exit(0)
                    }
                    "auto-switch" => {
                        let h = app.state::<core::Handle>();
                        let _ = h.tx.send(core::Msg::Act { action: "toggle_auto_switch".into(), arg: None });
                    }
                    id => {
                        if let Some(phone) = id.strip_prefix("phone:") {
                            let h = app.state::<core::Handle>();
                            let _ = h.tx.send(core::Msg::Act { action: "use_phone".into(), arg: Some(phone.into()) });
                        }
                    }
                })
                .on_tray_icon_event(|tray, e| {
                    if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
                        toggle_flyout(tray.app_handle());
                    }
                })
                .build(app)?;

            // Follow the Windows accent colour live. The registration lives as long as the app.
            let h = handle.clone();
            if let Ok(ui) = winrt::watch_accent(move || {
                let _ = h.emit("accent", accent_json());
            }) {
                std::mem::forget(ui);
            }

            // Started by hand (not at sign-in): show the window so it's obvious it's running.
            if !std::env::args().any(|a| a == AUTOSTART_ARG) {
                show_main(&handle, "home");
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("Sidemic failed to start");

    app.run(|_, e| match e {
        // Closing the window keeps the tray app running; only Quit exits.
        RunEvent::ExitRequested { code: None, api, .. } => api.prevent_exit(),
        RunEvent::Exit => {
            log::line("quitting");
            core::shutdown();
        }
        _ => {}
    });
}
