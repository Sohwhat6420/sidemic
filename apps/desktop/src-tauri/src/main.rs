//! TRUE Phone Link for Windows: a tray app. Left-click the tray icon for the flyout; the full window opens from
//! there. Everything that decides or touches audio lives in `core`; this file is the windows and the tray.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod adb;
mod core;
mod mic;
mod phone;
mod wasapi;
mod winrt;

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::window::{Effect, EffectsBuilder};
use tauri::{
    AppHandle, Emitter, LogicalSize, Manager, PhysicalPosition, RunEvent, Theme, WebviewUrl, WebviewWindow,
    WebviewWindowBuilder, WindowEvent,
};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};

const FLYOUT_WIDTH: f64 = 360.0;
const AUTOSTART_ARG: &str = "--autostart";

/// When the flyout last hid itself on losing focus. A tray click also takes focus away, so a click that lands
/// just after that hide means "close", not "open again".
static LAST_HIDE: Mutex<Option<Instant>> = Mutex::new(None);

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

#[tauri::command]
fn open_window(app: AppHandle, page: String) {
    show_main(&app, &page);
}

#[tauri::command]
fn hide_flyout(app: AppHandle) {
    if let Some(w) = app.get_webview_window("flyout") {
        let _ = w.hide();
    }
}

#[tauri::command]
fn flyout_height(app: AppHandle, height: f64) {
    if let Some(w) = app.get_webview_window("flyout") {
        let _ = w.set_size(LogicalSize::new(FLYOUT_WIDTH, height.ceil()));
        place_flyout(&w);
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
        .title("TRUE Phone Link")
        .inner_size(1000.0, 680.0)
        .min_inner_size(720.0, 480.0)
        .transparent(true)
        .theme(Some(Theme::Dark))
        .effects(EffectsBuilder::new().effect(Effect::Mica).build())
        .center()
        .build();
    if let Err(e) = built {
        eprintln!("main window: {e}");
    }
}

fn build_flyout(app: &AppHandle) -> tauri::Result<WebviewWindow> {
    let w = WebviewWindowBuilder::new(app, "flyout", WebviewUrl::App("flyout.html".into()))
        .title("TRUE Phone Link")
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
        .build()?;
    round_corners(&w);
    // The flyout only exists while it has focus. If it is ever up without focus (Windows refused the focus, or
    // something showed it), it goes away within half a second instead of sitting on the screen.
    // Asks Windows directly: Tauri's own visible/focused flags don't see a window shown from outside.
    if let Ok(h) = w.hwnd() {
        let hwnd = h.0 as isize;
        std::thread::spawn(move || {
            use windows::Win32::Foundation::HWND;
            use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, IsWindowVisible, ShowWindow, SW_HIDE};
            let hwnd = HWND(hwnd as _);
            let mut unfocused = 0;
            loop {
                std::thread::sleep(Duration::from_millis(150));
                let (visible, focused) = unsafe { (IsWindowVisible(hwnd).as_bool(), GetForegroundWindow() == hwnd) };
                if visible && !focused {
                    unfocused += 1;
                    if unfocused >= 3 {
                        unsafe {
                            let _ = ShowWindow(hwnd, SW_HIDE);
                        }
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
    let Some(w) = app.get_webview_window("flyout") else { return };
    if os_visible(&w) {
        let _ = w.hide();
        return;
    }
    if LAST_HIDE.lock().unwrap().is_some_and(|t| t.elapsed() < Duration::from_millis(300)) {
        return; // this click is what just closed it
    }
    place_flyout(&w);
    let _ = w.show();
    let _ = w.set_focus();
}

pub fn set_tray_tooltip(app: &AppHandle, text: &str) {
    if let Some(t) = app.tray_by_id("tray") {
        let _ = t.set_tooltip(Some(text));
    }
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
    if let Ok(r) = app.path().resource_dir() {
        let p = r.join("phone");
        if p.join("tpl-server.jar").exists() {
            return p;
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../phone-server/out")
}

fn main() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| show_main(app, "home")))
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, Some(vec![AUTOSTART_ARG])))
        .invoke_handler(tauri::generate_handler![state, accent, act, open_window, flyout_height, hide_flyout])
        .setup(|app| {
            let handle = app.handle().clone();
            let data = app.path().app_data_dir()?;
            app.manage(core::start(handle.clone(), data, phone_dir(&handle)));

            build_flyout(&handle)?;

            let open = MenuItem::with_id(app, "open", "Open TRUE Phone Link", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open, &quit])?;
            TrayIconBuilder::with_id("tray")
                .icon(Image::from_bytes(include_bytes!("../icons/tray.png"))?)
                .tooltip("TRUE Phone Link")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, e| match e.id.as_ref() {
                    "open" => show_main(app, "home"),
                    "quit" => app.exit(0),
                    _ => {}
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
        .expect("TRUE Phone Link failed to start");

    app.run(|_, e| match e {
        // Closing the window keeps the tray app running; only Quit exits.
        RunEvent::ExitRequested { code: None, api, .. } => api.prevent_exit(),
        RunEvent::Exit => core::shutdown(),
        _ => {}
    });
}
