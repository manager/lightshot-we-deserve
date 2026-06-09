#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::{Deserialize, Serialize};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{
    AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize, State, WebviewUrl,
    WebviewWindowBuilder,
};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

// ---------- logging ----------

static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();

fn data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("lightshot-we-deserve")
}

fn log_path() -> &'static PathBuf {
    LOG_PATH.get_or_init(|| {
        let dir = data_dir();
        let _ = fs::create_dir_all(&dir);
        dir.join("lightshot.log")
    })
}

fn log(msg: &str) {
    let line = format!(
        "[{}] {}\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
        msg
    );
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(log_path()) {
        let _ = f.write_all(line.as_bytes());
    }
    eprint!("{line}");
}

// ---------- settings ----------

fn default_true() -> bool {
    true
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct Settings {
    hotkey_area: String,
    hotkey_full: String,
    save_dir: String,
    #[serde(default = "default_true")]
    autostart: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            hotkey_area: "CmdOrCtrl+Shift+4".into(),
            hotkey_full: "CmdOrCtrl+Shift+3".into(),
            save_dir: String::new(),
            autostart: true,
        }
    }
}

fn settings_path() -> PathBuf {
    data_dir().join("settings.json")
}

fn load_settings() -> Settings {
    match fs::read_to_string(settings_path()) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
            log(&format!("settings parse failed ({e}); using defaults"));
            Settings::default()
        }),
        Err(_) => Settings::default(),
    }
}

fn store_settings(s: &Settings) -> Result<(), String> {
    let dir = data_dir();
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let json = serde_json::to_string_pretty(s).map_err(|e| e.to_string())?;
    fs::write(settings_path(), json).map_err(|e| e.to_string())
}

#[derive(Serialize, Clone)]
struct FrozenShot {
    data_url: String,
    width: u32,
    height: u32,
}

struct AppState {
    settings: Mutex<Settings>,
    area_sc: Mutex<Option<Shortcut>>,
    full_sc: Mutex<Option<Shortcut>>,
    frozen: Mutex<Option<FrozenShot>>,
}

fn resolve_save_dir(state: &AppState) -> PathBuf {
    let configured = state.settings.lock().unwrap().save_dir.trim().to_string();
    if !configured.is_empty() {
        let p = PathBuf::from(&configured);
        if p.is_dir() {
            return p;
        }
        log(&format!("save_dir '{configured}' is not a folder; using Desktop"));
    }
    dirs::desktop_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(std::env::temp_dir)
}

// ---------- capture ----------

fn timestamp_name() -> String {
    format!(
        "Screenshot {}.png",
        chrono::Local::now().format("%Y-%m-%d %H-%M-%S")
    )
}

fn primary_monitor() -> Result<xcap::Monitor, String> {
    let monitors = xcap::Monitor::all().map_err(|e| format!("Monitor::all failed: {e}"))?;
    monitors
        .into_iter()
        .find(|m| m.is_primary().unwrap_or(false))
        .ok_or_else(|| "no primary monitor found".to_string())
}

// Bounding box of the whole virtual desktop (all monitors) in physical px:
// (origin_x, origin_y, width, height).
fn virtual_bounds() -> Result<(i32, i32, u32, u32), String> {
    let monitors = xcap::Monitor::all().map_err(|e| format!("Monitor::all failed: {e}"))?;
    if monitors.is_empty() {
        return Err("no monitors found".to_string());
    }
    let (mut min_x, mut min_y) = (i32::MAX, i32::MAX);
    let (mut max_x, mut max_y) = (i32::MIN, i32::MIN);
    for m in &monitors {
        let x = m.x().map_err(|e| e.to_string())?;
        let y = m.y().map_err(|e| e.to_string())?;
        let w = m.width().map_err(|e| e.to_string())? as i32;
        let h = m.height().map_err(|e| e.to_string())? as i32;
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x + w);
        max_y = max_y.max(y + h);
    }
    Ok((min_x, min_y, (max_x - min_x) as u32, (max_y - min_y) as u32))
}

fn capture_full(dir: &PathBuf) -> Result<String, String> {
    let monitor = primary_monitor()?;
    let image = monitor
        .capture_image()
        .map_err(|e| format!("capture_image failed: {e}"))?;
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let path = dir.join(timestamp_name());
    image
        .save(&path)
        .map_err(|e| format!("save failed: {e}"))?;
    Ok(path.to_string_lossy().to_string())
}

fn grab_frozen() -> Result<FrozenShot, String> {
    let (min_x, min_y, vw, vh) = virtual_bounds()?;
    let monitors = xcap::Monitor::all().map_err(|e| format!("Monitor::all failed: {e}"))?;
    let mut canvas = image::RgbaImage::new(vw, vh);
    for m in &monitors {
        let x = m.x().map_err(|e| e.to_string())?;
        let y = m.y().map_err(|e| e.to_string())?;
        match m.capture_image() {
            Ok(img) => {
                image::imageops::replace(&mut canvas, &img, (x - min_x) as i64, (y - min_y) as i64);
            }
            Err(e) => log(&format!("monitor capture failed at ({x},{y}): {e}")),
        }
    }
    log(&format!("frozen virtual desktop {vw}x{vh} at ({min_x},{min_y})"));
    // Fast PNG: this image is only shown in the overlay for selection, so we
    // trade a bigger in-memory blob for a much quicker encode (default zlib
    // compression on a multi-monitor image cost seconds).
    let mut buf = std::io::Cursor::new(Vec::new());
    let encoder = image::codecs::png::PngEncoder::new_with_quality(
        &mut buf,
        image::codecs::png::CompressionType::Fast,
        image::codecs::png::FilterType::NoFilter,
    );
    {
        use image::ImageEncoder;
        encoder
            .write_image(canvas.as_raw(), vw, vh, image::ExtendedColorType::Rgba8)
            .map_err(|e| format!("png encode failed: {e}"))?;
    }
    let b64 = STANDARD.encode(buf.into_inner());
    Ok(FrozenShot {
        data_url: format!("data:image/png;base64,{b64}"),
        width: vw,
        height: vh,
    })
}

fn decode_png_data_url(s: &str) -> Result<Vec<u8>, String> {
    let data = s
        .rsplit_once(",")
        .map(|(_, b)| b)
        .unwrap_or(s);
    STANDARD.decode(data).map_err(|e| format!("base64 decode failed: {e}"))
}

// ---------- windows ----------

fn begin_area_capture(app: &AppHandle) {
    match grab_frozen() {
        Ok(shot) => {
            *app.state::<AppState>().frozen.lock().unwrap() = Some(shot);
            log("screen frozen for area capture");
        }
        Err(e) => {
            log(&format!("freeze failed: {e}"));
            return;
        }
    }
    show_overlay(app);
}

fn show_overlay(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("overlay") {
        if let Ok((x, y, vw, vh)) = virtual_bounds() {
            let _ = w.set_position(PhysicalPosition::new(x, y));
            let _ = w.set_size(PhysicalSize::new(vw, vh));
        }
        // Stay hidden until JS has rendered the frozen frame, then it calls
        // `overlay_ready` to reveal — avoids the dim appearing a beat late.
        let _ = w.emit("frozen-ready", ());
        return;
    }
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        let app = handle;
        match WebviewWindowBuilder::new(&app, "overlay", WebviewUrl::App("overlay.html".into()))
            .title("Select area")
            .decorations(false)
            .transparent(true)
            .always_on_top(true)
            .skip_taskbar(true)
            .resizable(false)
            .shadow(false)
            .visible(false)
            .build()
        {
            Ok(win) => {
                match virtual_bounds() {
                    Ok((x, y, w, h)) => {
                        let _ = win.set_position(PhysicalPosition::new(x, y));
                        let _ = win.set_size(PhysicalSize::new(w, h));
                    }
                    Err(e) => log(&format!("virtual_bounds failed: {e}")),
                }
                // Built hidden; JS reveals it via `overlay_ready` once the
                // frozen frame is painted, so dim + crosshair appear together.
                let _ = win.emit("frozen-ready", ());
                log("overlay prepared");
            }
            Err(e) => log(&format!("overlay build failed: {e}")),
        }
    });
}

fn hide_overlay(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("overlay") {
        let _ = w.hide();
    }
    *app.state::<AppState>().frozen.lock().unwrap() = None;
}

fn show_settings(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("settings") {
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        let app = handle;
        match WebviewWindowBuilder::new(&app, "settings", WebviewUrl::App("settings.html".into()))
            .title("Lightshot We Deserve - Settings")
            .inner_size(460.0, 420.0)
            .resizable(false)
            .center()
            .build()
        {
            Ok(_) => log("settings opened"),
            Err(e) => log(&format!("settings build failed: {e}")),
        }
    });
}

// ---------- shortcuts ----------

fn apply_shortcuts(app: &AppHandle) {
    let gs = app.global_shortcut();
    if let Err(e) = gs.unregister_all() {
        log(&format!("unregister_all failed: {e}"));
    }
    let state = app.state::<AppState>();
    let (area_str, full_str) = {
        let s = state.settings.lock().unwrap();
        (s.hotkey_area.clone(), s.hotkey_full.clone())
    };

    match area_str.parse::<Shortcut>() {
        Ok(sc) => match gs.register(sc.clone()) {
            Ok(_) => {
                *state.area_sc.lock().unwrap() = Some(sc);
                log(&format!("registered area hotkey: {area_str}"));
            }
            Err(e) => log(&format!("register area '{area_str}' failed: {e}")),
        },
        Err(e) => log(&format!("parse area hotkey '{area_str}' failed: {e:?}")),
    }

    match full_str.parse::<Shortcut>() {
        Ok(sc) => match gs.register(sc.clone()) {
            Ok(_) => {
                *state.full_sc.lock().unwrap() = Some(sc);
                log(&format!("registered full hotkey: {full_str}"));
            }
            Err(e) => log(&format!("register full '{full_str}' failed: {e}")),
        },
        Err(e) => log(&format!("parse full hotkey '{full_str}' failed: {e:?}")),
    }
}

fn apply_autostart(app: &AppHandle, enabled: bool) {
    use tauri_plugin_autostart::ManagerExt;
    let mgr = app.autolaunch();
    let res = if enabled { mgr.enable() } else { mgr.disable() };
    match res {
        Ok(_) => log(&format!("autostart set to {enabled}")),
        Err(e) => log(&format!("autostart set to {enabled} failed: {e}")),
    }
}

fn do_full_capture(app: &AppHandle) {
    let state = app.state::<AppState>();
    let dir = resolve_save_dir(&state);
    std::thread::spawn(move || match capture_full(&dir) {
        Ok(p) => log(&format!("saved full screenshot: {p}")),
        Err(e) => log(&format!("full capture error: {e}")),
    });
}

// ---------- commands ----------

#[tauri::command]
fn get_settings(state: State<AppState>) -> Settings {
    state.settings.lock().unwrap().clone()
}

#[tauri::command]
fn save_settings(app: AppHandle, state: State<AppState>, settings: Settings) -> Result<(), String> {
    store_settings(&settings)?;
    let autostart = settings.autostart;
    *state.settings.lock().unwrap() = settings;
    apply_shortcuts(&app);
    apply_autostart(&app, autostart);
    log("settings saved");
    Ok(())
}

#[tauri::command]
fn get_frozen(state: State<AppState>) -> Option<FrozenShot> {
    state.frozen.lock().unwrap().clone()
}

#[tauri::command]
fn save_capture(app: AppHandle, png_data_url: String) -> Result<String, String> {
    let bytes = decode_png_data_url(&png_data_url)?;
    let dir = resolve_save_dir(&app.state::<AppState>());
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(timestamp_name());
    fs::write(&path, &bytes).map_err(|e| format!("save failed: {e}"))?;
    let p = path.to_string_lossy().to_string();
    log(&format!("saved area screenshot: {p}"));
    hide_overlay(&app);
    Ok(p)
}

#[tauri::command]
fn copy_capture(app: AppHandle, png_data_url: String) -> Result<(), String> {
    let bytes = decode_png_data_url(&png_data_url)?;
    let img = image::load_from_memory(&bytes)
        .map_err(|e| format!("decode failed: {e}"))?
        .to_rgba8();
    let (w, h) = (img.width() as usize, img.height() as usize);
    let mut cb = arboard::Clipboard::new().map_err(|e| format!("clipboard open failed: {e}"))?;
    cb.set_image(arboard::ImageData {
        width: w,
        height: h,
        bytes: std::borrow::Cow::Owned(img.into_raw()),
    })
    .map_err(|e| format!("clipboard write failed: {e}"))?;
    log("copied area screenshot to clipboard");
    hide_overlay(&app);
    Ok(())
}

#[tauri::command]
fn overlay_ready(app: AppHandle) {
    if let Some(w) = app.get_webview_window("overlay") {
        let _ = w.show();
        let _ = w.set_focus();
    }
}

#[tauri::command]
fn cancel_area(app: AppHandle) {
    log("area selection cancelled");
    hide_overlay(&app);
}

#[tauri::command]
fn capture_full_now(app: AppHandle) -> Result<String, String> {
    let dir = resolve_save_dir(&app.state::<AppState>());
    let r = capture_full(&dir);
    match &r {
        Ok(p) => log(&format!("saved full screenshot (manual): {p}")),
        Err(e) => log(&format!("manual full capture error: {e}")),
    }
    r
}

#[tauri::command]
fn close_settings(app: AppHandle) {
    if let Some(w) = app.get_webview_window("settings") {
        let _ = w.close();
    }
}

// ---------- tray ----------

fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let area = MenuItem::with_id(app, "area", "Capture area", true, None::<&str>)?;
    let full = MenuItem::with_id(app, "full", "Capture full screen", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let settings = MenuItem::with_id(app, "settings", "Settings", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&area, &full, &sep, &settings, &quit])?;

    let mut builder = TrayIconBuilder::with_id("main")
        .tooltip("Lightshot We Deserve")
        .menu(&menu)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "area" => begin_area_capture(app),
            "full" => do_full_capture(app),
            "settings" => show_settings(app),
            "quit" => {
                log("quit requested from tray");
                std::process::exit(0);
            }
            _ => {}
        });

    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;
    log("tray icon created");
    Ok(())
}

// ---------- entry ----------

fn run() {
    log("=== lightshot-we-deserve launching ===");
    tauri::Builder::default()
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    if event.state() != ShortcutState::Pressed {
                        return;
                    }
                    let state = app.state::<AppState>();
                    let is_area = state
                        .area_sc
                        .lock()
                        .unwrap()
                        .as_ref()
                        .map(|s| s == shortcut)
                        .unwrap_or(false);
                    let is_full = state
                        .full_sc
                        .lock()
                        .unwrap()
                        .as_ref()
                        .map(|s| s == shortcut)
                        .unwrap_or(false);
                    if is_area {
                        log("hotkey: capture area");
                        begin_area_capture(app);
                    } else if is_full {
                        log("hotkey: capture full");
                        do_full_capture(app);
                    }
                })
                .build(),
        )
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec![]),
        ))
        .invoke_handler(tauri::generate_handler![
            get_settings,
            save_settings,
            get_frozen,
            save_capture,
            copy_capture,
            overlay_ready,
            cancel_area,
            capture_full_now,
            close_settings
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            app.manage(AppState {
                settings: Mutex::new(load_settings()),
                area_sc: Mutex::new(None),
                full_sc: Mutex::new(None),
                frozen: Mutex::new(None),
            });
            apply_shortcuts(&handle);
            if let Err(e) = build_tray(&handle) {
                log(&format!("build_tray failed: {e}"));
            }
            let want_autostart = handle.state::<AppState>().settings.lock().unwrap().autostart;
            apply_autostart(&handle, want_autostart);
            log("setup complete");
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("failed to build tauri app")
        .run(|_app, event| {
            if let tauri::RunEvent::ExitRequested { api, .. } = event {
                api.prevent_exit();
            }
        });
}

fn main() {
    std::panic::set_hook(Box::new(|info| {
        log(&format!("PANIC: {info}"));
    }));
    if let Err(_) = std::panic::catch_unwind(run) {
        log("fatal: run() panicked, exiting");
    }
}
