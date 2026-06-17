#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

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

fn default_quality() -> String {
    "high".into()
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct Settings {
    hotkey_area: String,
    hotkey_full: String,
    save_dir: String,
    #[serde(default = "default_true")]
    autostart: bool,
    #[serde(default = "default_quality")]
    video_quality: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            hotkey_area: "CmdOrCtrl+Shift+4".into(),
            hotkey_full: "CmdOrCtrl+Shift+3".into(),
            save_dir: String::new(),
            autostart: true,
            video_quality: "high".into(),
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

// Frozen screen kept in memory as uncompressed BMP bytes. Served to the overlay
// via a custom URI scheme so the multi-MB image never crosses the JSON IPC bridge
// (base64 + giant-string transfer was the capture-latency culprit). BMP skips the
// PNG compression pass entirely — lossless and faster to encode.
struct Frozen {
    bytes: Vec<u8>,
    width: u32,
    height: u32,
    nonce: u64,
}

#[derive(Serialize, Clone)]
struct FrozenInfo {
    url: String,
    width: u32,
    height: u32,
}

struct Recording {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

struct AppState {
    settings: Mutex<Settings>,
    area_sc: Mutex<Option<Shortcut>>,
    full_sc: Mutex<Option<Shortcut>>,
    frozen: Mutex<Option<Frozen>>,
    recording: Mutex<Option<Recording>>,
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

// Turn a user-typed name into a safe single-segment filename ending in .png.
// Strips path separators and characters Windows rejects; empty -> timestamp.
fn custom_name(name: &str) -> String {
    let cleaned: String = name
        .trim()
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    let cleaned = cleaned.trim_matches(['.', ' ']).to_string();
    if cleaned.is_empty() {
        return timestamp_name();
    }
    if cleaned.to_lowercase().ends_with(".png") {
        cleaned
    } else {
        format!("{cleaned}.png")
    }
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

// Snapshot of every monitor's (x, y, width, height), sorted so two reads taken a
// moment apart can be compared for equality. Used to tell whether the desktop
// geometry has stopped moving after a wake / dock event.
fn monitor_layout() -> Result<Vec<(i32, i32, u32, u32)>, String> {
    let monitors = xcap::Monitor::all().map_err(|e| format!("Monitor::all failed: {e}"))?;
    let mut layout: Vec<(i32, i32, u32, u32)> = monitors
        .iter()
        .map(|m| {
            (
                m.x().unwrap_or(0),
                m.y().unwrap_or(0),
                m.width().unwrap_or(0),
                m.height().unwrap_or(0),
            )
        })
        .collect();
    layout.sort_unstable();
    Ok(layout)
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

fn next_nonce() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// One capture attempt. Geometry and pixels are taken from the SAME enumeration
// pass so they can't disagree within an attempt; the layout is then re-read and,
// unless `force` is set, the attempt is rejected (Ok(None)) if it moved while we
// were capturing — the tell-tale of an unsettled post-wake/dock desktop.
fn grab_frozen_once(force: bool) -> Result<Option<Frozen>, String> {
    let monitors = xcap::Monitor::all().map_err(|e| format!("Monitor::all failed: {e}"))?;
    if monitors.is_empty() {
        return Err("no monitors found".to_string());
    }

    // xcap's Monitor wraps a raw OS handle that is neither Send nor Sync, so the
    // grabs can't be parallelized across threads — capture each screen in turn.
    let mut placed: Vec<(i32, i32, image::RgbaImage)> = Vec::new();
    let mut layout: Vec<(i32, i32, u32, u32)> = Vec::new();
    for m in &monitors {
        let x = m.x().unwrap_or(0);
        let y = m.y().unwrap_or(0);
        layout.push((x, y, m.width().unwrap_or(0), m.height().unwrap_or(0)));
        match m.capture_image() {
            Ok(img) => placed.push((x, y, img)),
            Err(e) => log(&format!("monitor capture failed at ({x},{y}): {e}")),
        }
    }
    if placed.is_empty() {
        return Err("all monitor captures failed".to_string());
    }
    layout.sort_unstable();

    // If the OS reports a different layout now than when we started, the desktop
    // geometry was still shifting and the composite would be garbled — bail so
    // the caller can retry once things settle.
    if !force && layout != monitor_layout()? {
        return Ok(None);
    }

    // Bounds derived from the frames we actually captured (their real pixel
    // sizes), so a screen can never be clipped or overlapped in the stitch.
    let (mut min_x, mut min_y) = (i32::MAX, i32::MAX);
    let (mut max_x, mut max_y) = (i32::MIN, i32::MIN);
    for (x, y, img) in &placed {
        min_x = min_x.min(*x);
        min_y = min_y.min(*y);
        max_x = max_x.max(*x + img.width() as i32);
        max_y = max_y.max(*y + img.height() as i32);
    }
    let (vw, vh) = ((max_x - min_x) as u32, (max_y - min_y) as u32);

    let mut canvas = image::RgbaImage::new(vw, vh);
    for (x, y, img) in &placed {
        image::imageops::replace(&mut canvas, img, (*x - min_x) as i64, (*y - min_y) as i64);
    }

    // Uncompressed BMP: the blob is only displayed in the overlay, so we skip the
    // PNG compression pass entirely — lossless and the fastest encode available.
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        use image::ImageEncoder;
        image::codecs::bmp::BmpEncoder::new(&mut buf)
            .write_image(canvas.as_raw(), vw, vh, image::ExtendedColorType::Rgba8)
            .map_err(|e| format!("bmp encode failed: {e}"))?;
    }
    log(&format!("frozen virtual desktop {vw}x{vh} at ({min_x},{min_y})"));
    Ok(Some(Frozen {
        bytes: buf.into_inner(),
        width: vw,
        height: vh,
        nonce: next_nonce(),
    }))
}

fn grab_frozen() -> Result<Frozen, String> {
    // After waking from sleep or (re)attaching a dock, Windows reports a shifting
    // monitor layout for up to ~1s; a capture taken then looks "caught mid-resize".
    // Retry until the layout reads stable, then fall back to a forced grab so a
    // screenshot is never blocked outright.
    const MAX_ATTEMPTS: u32 = 10;
    for attempt in 1..=MAX_ATTEMPTS {
        match grab_frozen_once(false) {
            Ok(Some(frozen)) => {
                if attempt > 1 {
                    log(&format!("monitor layout settled after {attempt} attempts"));
                }
                return Ok(frozen);
            }
            Ok(None) => log(&format!(
                "monitor layout unsettled (attempt {attempt}/{MAX_ATTEMPTS}); retrying"
            )),
            Err(e) => return Err(e),
        }
        std::thread::sleep(std::time::Duration::from_millis(120));
    }
    log("monitor layout never settled; taking a best-effort capture");
    grab_frozen_once(true)?.ok_or_else(|| "forced capture produced no frame".to_string())
}

fn decode_png_data_url(s: &str) -> Result<Vec<u8>, String> {
    let data = s
        .rsplit_once(",")
        .map(|(_, b)| b)
        .unwrap_or(s);
    STANDARD.decode(data).map_err(|e| format!("base64 decode failed: {e}"))
}

// ---------- video recording ----------

fn video_name() -> String {
    format!(
        "Recording {}.mp4",
        chrono::Local::now().format("%Y-%m-%d %H-%M-%S")
    )
}

// (frames_per_second, x264 crf, optional downscale filter). Lower crf = higher
// quality + bigger file.
fn quality_params(q: &str) -> (u32, u32, Option<String>) {
    match q {
        // Cap the long edge at 1280 px; never upscales (min picks the source
        // size when it's already smaller). -2 keeps aspect with an even height,
        // which yuv420p requires.
        "low" => (15, 30, Some("scale='min(1280,iw)':-2".into())),
        // Native resolution, 60 fps, near-visually-lossless crf. Big files.
        "vhigh" => (60, 15, None),
        _ => (30, 23, None), // high: native resolution
    }
}

// ffmpeg is shipped next to the app (bundled resource). Fall back to the exe
// folder, then to a PATH lookup so a dev machine with ffmpeg installed works.
fn resolve_ffmpeg(app: &AppHandle) -> PathBuf {
    if let Ok(res) = app.path().resource_dir() {
        for cand in [res.join("binaries").join("ffmpeg.exe"), res.join("ffmpeg.exe")] {
            if cand.is_file() {
                return cand;
            }
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let cand = dir.join("ffmpeg.exe");
            if cand.is_file() {
                return cand;
            }
        }
    }
    PathBuf::from("ffmpeg")
}

// Current mouse pointer in physical desktop px, so the recorder can show where
// the user is pointing. Windows-only; other platforms record without a cursor.
#[cfg(windows)]
fn cursor_pos() -> Option<(i32, i32)> {
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;
    let mut p = POINT { x: 0, y: 0 };
    if unsafe { GetCursorPos(&mut p) } != 0 {
        Some((p.x, p.y))
    } else {
        None
    }
}
#[cfg(not(windows))]
fn cursor_pos() -> Option<(i32, i32)> {
    None
}

// Classic arrow pointer. '#' = black outline, '.' = white fill, ' ' = clear.
// The tip is the top-left cell, placed exactly at the cursor position.
const CURSOR_ART: [&str; 19] = [
    "#",
    "##",
    "#.#",
    "#..#",
    "#...#",
    "#....#",
    "#.....#",
    "#......#",
    "#.......#",
    "#........#",
    "#.....####",
    "#..#..#",
    "#.# ##.#",
    "##  #..#",
    "#    #..#",
    "     #..#",
    "      #..#",
    "      #..#",
    "       ##",
];

// Paint the pointer into `buf` (w*h RGBA) at frame-local (cx, cy) so the user
// can see what they're pointing at in the recording.
fn draw_cursor(buf: &mut [u8], w: u32, h: u32, cx: i32, cy: i32) {
    let (w, h) = (w as i32, h as i32);
    for (ry, row) in CURSOR_ART.iter().enumerate() {
        for (rx, ch) in row.bytes().enumerate() {
            let (r, g, b) = match ch {
                b'#' => (0u8, 0u8, 0u8),
                b'.' => (255u8, 255u8, 255u8),
                _ => continue,
            };
            let px = cx + rx as i32;
            let py = cy + ry as i32;
            if px < 0 || px >= w || py < 0 || py >= h {
                continue;
            }
            let di = (((py * w) + px) * 4) as usize;
            buf[di] = r;
            buf[di + 1] = g;
            buf[di + 2] = b;
            buf[di + 3] = 255;
        }
    }
}

// Copy the recorded screen region into `buf` as tightly-packed RGBA of exactly
// w*h pixels. Anything outside the captured monitor stays black, so the frame
// size never changes mid-recording (ffmpeg's raw input requires a fixed size).
fn fill_region_frame(ax: i32, ay: i32, w: u32, h: u32, buf: &mut [u8]) {
    for b in buf.iter_mut() {
        *b = 0;
    }
    let monitors = match xcap::Monitor::all() {
        Ok(m) => m,
        Err(_) => return,
    };
    let cx = ax + (w as i32) / 2;
    let cy = ay + (h as i32) / 2;
    let mon = monitors.into_iter().find(|m| {
        let mx = m.x().unwrap_or(0);
        let my = m.y().unwrap_or(0);
        let mw = m.width().unwrap_or(0) as i32;
        let mh = m.height().unwrap_or(0) as i32;
        cx >= mx && cx < mx + mw && cy >= my && cy < my + mh
    });
    let mon = match mon {
        Some(m) => m,
        None => return,
    };
    let mx = mon.x().unwrap_or(0);
    let my = mon.y().unwrap_or(0);
    let img = match mon.capture_image() {
        Ok(i) => i,
        Err(_) => return,
    };
    let iw = img.width() as i32;
    let ih = img.height() as i32;
    let raw = img.as_raw();
    let ox = ax - mx; // source origin within this monitor
    let oy = ay - my;
    let c_start = 0.max(-ox);
    let c_end = (w as i32).min(iw - ox);
    if c_end <= c_start {
        return;
    }
    let span = ((c_end - c_start) * 4) as usize;
    for row in 0..h as i32 {
        let sy = oy + row;
        if sy < 0 || sy >= ih {
            continue;
        }
        let si = (((sy * iw) + (ox + c_start)) * 4) as usize;
        let di = (((row * w as i32) + c_start) * 4) as usize;
        buf[di..di + span].copy_from_slice(&raw[si..si + span]);
    }
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

// Build the area-selection overlay once, hidden, at startup. Creating the
// webview the first time a hotkey fires makes the screen flash on that first
// capture; pre-creating it means the window already exists and only needs to be
// filled with the frozen frame and shown — seamless from the very first press.
fn create_overlay_window(app: &AppHandle) {
    if app.get_webview_window("overlay").is_some() {
        return;
    }
    match WebviewWindowBuilder::new(app, "overlay", WebviewUrl::App("overlay.html".into()))
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
        Ok(_) => log("overlay window pre-created"),
        Err(e) => log(&format!("overlay pre-create failed: {e}")),
    }
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

const REC_BORDER: i32 = 3;

// Build the two recording-indicator windows once, hidden, at startup. Creating
// a webview while a recording is already running (capture thread + ffmpeg
// hammering the machine) intermittently deadlocked, so the windows that show
// the border and Stop bar are created up front on the main thread and merely
// repositioned + shown when recording begins.
fn create_indicator_windows(app: &AppHandle) {
    if app.get_webview_window("recborder").is_none() {
        match WebviewWindowBuilder::new(app, "recborder", WebviewUrl::App("border.html".into()))
            .title("Recording area")
            .decorations(false)
            .transparent(true)
            .always_on_top(true)
            .skip_taskbar(true)
            .resizable(false)
            .shadow(false)
            .focused(false)
            .visible(false)
            .build()
        {
            Ok(win) => {
                let _ = win.set_ignore_cursor_events(true);
                log("recborder window created");
            }
            Err(e) => log(&format!("recborder build failed: {e}")),
        }
    }
    if app.get_webview_window("recorder").is_none() {
        match WebviewWindowBuilder::new(app, "recorder", WebviewUrl::App("recorder.html".into()))
            .title("Recording")
            .decorations(false)
            .transparent(true)
            .always_on_top(true)
            .skip_taskbar(true)
            .resizable(false)
            .shadow(false)
            .inner_size(150.0, 40.0)
            .visible(false)
            .build()
        {
            Ok(_) => log("recorder window created"),
            Err(e) => log(&format!("recorder build failed: {e}")),
        }
    }
}

// While recording, frame the captured region with a thin border (so the user
// sees exactly what is being recorded) and show a small Stop/timer bar just
// OUTSIDE the region. Capture grabs only the region rect, so neither the border
// nor the bar ever lands in the video. (ax, ay) are physical px in the virtual-
// desktop space, matching the overlay/window coordinate origin.
fn show_record_ui(app: &AppHandle, ax: i32, ay: i32, w: u32, h: u32) {
    let bx = ax - REC_BORDER;
    let by = ay - REC_BORDER;
    let bw = (w as i32 + REC_BORDER * 2).max(1) as u32;
    let bh = (h as i32 + REC_BORDER * 2).max(1) as u32;

    let (cw, ch) = (150i32, 40i32);
    let (vx, vy, vw, vh) =
        virtual_bounds().unwrap_or((bx, by, bw + 400, bh + 400));
    let vx2 = vx + vw as i32;
    let vy2 = vy + vh as i32;

    // Prefer the bar just below the region; flip above if it would run off-screen.
    let mut cy = ay + h as i32 + REC_BORDER + 6;
    if cy + ch > vy2 {
        cy = ay - REC_BORDER - 6 - ch;
    }
    if cy < vy {
        cy = vy;
    }
    let cx = ax.min(vx2 - cw).max(vx);

    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        let app = handle;
        // Windows are pre-created at startup; here we only move + reveal them.
        // Avoiding webview creation on this path is what fixed the recording
        // stall (building a window mid-record could deadlock).
        create_indicator_windows(&app);

        if let Some(win) = app.get_webview_window("recborder") {
            let _ = win.set_position(PhysicalPosition::new(bx, by));
            let _ = win.set_size(PhysicalSize::new(bw, bh));
            let _ = win.set_ignore_cursor_events(true);
            let _ = win.show();
            log("record border shown");
        } else {
            log("recborder window missing");
        }

        if let Some(win) = app.get_webview_window("recorder") {
            let _ = win.set_position(PhysicalPosition::new(cx, cy));
            let _ = win.show();
            let _ = win.set_focus();
            log("recorder bar shown");
        } else {
            log("recorder window missing");
        }
    });
}

fn hide_recorder(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("recorder") {
        let _ = w.hide();
    }
    if let Some(w) = app.get_webview_window("recborder") {
        let _ = w.hide();
    }
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
fn get_frozen(state: State<AppState>) -> Option<FrozenInfo> {
    let guard = state.frozen.lock().unwrap();
    guard.as_ref().map(|f| FrozenInfo {
        url: format!("http://frozen.localhost/{}.bmp", f.nonce),
        width: f.width,
        height: f.height,
    })
}

#[tauri::command]
fn save_capture(app: AppHandle, png_data_url: String, name: Option<String>) -> Result<String, String> {
    let bytes = decode_png_data_url(&png_data_url)?;
    let dir = resolve_save_dir(&app.state::<AppState>());
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let file_name = match name {
        Some(n) if !n.trim().is_empty() => custom_name(&n),
        _ => timestamp_name(),
    };
    let path = dir.join(file_name);
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

// Begin recording the selected region. (x, y, w, h) are physical px relative to
// the virtual-desktop origin (the overlay's top-left), matching the frozen frame
// the user selected on. The overlay is dismissed before the first frame so it
// never appears in the video.
#[tauri::command]
fn start_recording(
    app: AppHandle,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    quality: Option<String>,
) -> Result<(), String> {
    log(&format!("start_recording requested: x={x} y={y} w={w} h={h}"));
    let state = app.state::<AppState>();
    if state.recording.lock().unwrap().is_some() {
        log("start_recording rejected: already recording");
        return Err("already recording".into());
    }

    // yuv420p needs even dimensions.
    let w = (w & !1).max(2);
    let h = (h & !1).max(2);

    let (vx, vy, _, _) = virtual_bounds()?;
    let ax = vx + x;
    let ay = vy + y;

    // The overlay can pick quality per-recording; fall back to the saved setting.
    let (fps, crf, scale) = {
        let q = quality
            .filter(|q| q == "low" || q == "high" || q == "vhigh")
            .unwrap_or_else(|| state.settings.lock().unwrap().video_quality.clone());
        quality_params(&q)
    };

    let dir = resolve_save_dir(&state);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let out = dir.join(video_name());
    let out_str = out.to_string_lossy().to_string();
    let ffmpeg = resolve_ffmpeg(&app);
    log(&format!("ffmpeg path: {}", ffmpeg.display()));

    let mut args: Vec<String> = vec![
        "-y".into(),
        "-f".into(),
        "rawvideo".into(),
        "-pixel_format".into(),
        "rgba".into(),
        "-video_size".into(),
        format!("{w}x{h}"),
        "-framerate".into(),
        fps.to_string(),
        "-i".into(),
        "-".into(),
        "-an".into(),
        "-r".into(),
        fps.to_string(),
        "-c:v".into(),
        "libx264".into(),
        "-preset".into(),
        "veryfast".into(),
        "-crf".into(),
        crf.to_string(),
        "-pix_fmt".into(),
        "yuv420p".into(),
    ];
    if let Some(vf) = scale {
        args.push("-vf".into());
        args.push(vf);
    }
    args.push(out_str.clone());

    // Keep ffmpeg's diagnostics so a failed encode can be inspected after the
    // fact (the indicator window only tells the user that it failed).
    let ff_log = data_dir().join("recording-ffmpeg.log");
    let ff_err = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&ff_log)
        .map(Stdio::from)
        .unwrap_or_else(|_| Stdio::null());

    let mut cmd = Command::new(&ffmpeg);
    cmd.args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(ff_err);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd.spawn().map_err(|e| {
        let m = format!("ffmpeg spawn failed ({}): {e}", ffmpeg.display());
        log(&m);
        m
    })?;
    let mut stdin = match child.stdin.take() {
        Some(s) => s,
        None => {
            log("ffmpeg stdin unavailable; aborting recording");
            let _ = child.kill();
            let _ = child.wait();
            return Err("ffmpeg stdin unavailable".into());
        }
    };
    log("ffmpeg stdin acquired");

    // Start grabbing frames immediately. The capture thread owns ffmpeg's stdin
    // and child process, so the recording runs to completion independently of the
    // indicator windows. The overlay is hidden and the border/Stop bar are shown
    // AFTER this, as best-effort UI: if any of that misbehaves, video still records.
    let stop = Arc::new(AtomicBool::new(false));
    let stop_t = stop.clone();
    let app_t = app.clone();
    let handle = std::thread::spawn(move || {
        log("capture thread running");
        // Let the overlay actually disappear from the compositor first.
        std::thread::sleep(std::time::Duration::from_millis(300));
        let frame_bytes = (w as usize) * (h as usize) * 4;
        let mut buf = vec![0u8; frame_bytes];
        let interval = std::time::Duration::from_micros(1_000_000 / fps as u64);
        let start = std::time::Instant::now();
        let mut written: u64 = 0;
        let mut ffmpeg_died = false;
        while !stop_t.load(Ordering::Relaxed) {
            let tick_start = std::time::Instant::now();
            fill_region_frame(ax, ay, w, h, &mut buf);
            if let Some((px, py)) = cursor_pos() {
                draw_cursor(&mut buf, w, h, px - ax, py - ay);
            }
            // Emit as many copies of this frame as wall-clock time says are due.
            // If a capture took longer than one frame, the gap is filled with
            // duplicates so playback runs at real speed instead of slow-motion.
            let due = (start.elapsed().as_secs_f64() * fps as f64) as u64 + 1;
            while written < due {
                if stdin.write_all(&buf).is_err() {
                    ffmpeg_died = true;
                    break;
                }
                if written == 0 {
                    log("first frame written");
                }
                written += 1;
            }
            if ffmpeg_died {
                break;
            }
            // Nap the rest of one frame interval so we don't busy-spin; if the
            // capture already overran it, loop straight into the next grab.
            let spent = tick_start.elapsed();
            if interval > spent {
                std::thread::sleep(interval - spent);
            }
        }
        drop(stdin); // EOF -> ffmpeg finalizes and writes the moov atom
        // Wait for ffmpeg to flush the moov atom, but never wait forever: if it
        // wedges, kill it so the file is closed and nothing leaks.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {
                    if std::time::Instant::now() > deadline {
                        log("ffmpeg did not exit in time; terminating");
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(_) => break,
            }
        }
        if ffmpeg_died {
            log("recording aborted: ffmpeg exited early (see recording-ffmpeg.log)");
            // Free the slot so the user can start another recording, and tell
            // the indicator to show the failure instead of a running timer.
            let _ = app_t.state::<AppState>().recording.lock().unwrap().take();
            let _ = app_t.emit("recording-error", "Recording failed — video encoder stopped");
        } else {
            log(&format!("recording finished: {out_str} ({written} frames)"));
        }
    });

    *state.recording.lock().unwrap() = Some(Recording {
        stop,
        handle: Some(handle),
    });
    log("capture thread spawned");

    // Best-effort indicator UI. Recording is already underway above, so even if
    // these calls stall the command thread, frames keep flowing.
    log("hiding overlay");
    hide_overlay(&app);
    log("showing record ui");
    show_record_ui(&app, ax, ay, w, h);
    // The indicator window is reused across recordings, so its timer/Stop state
    // must be reset each time — tell it a fresh recording just began.
    let _ = app.emit("recording-started", ());

    log(&format!(
        "recording started -> {} ({w}x{h} @ {fps}fps, crf {crf})",
        out.display()
    ));
    Ok(())
}

#[tauri::command]
fn stop_recording(app: AppHandle) -> Result<(), String> {
    let rec = app.state::<AppState>().recording.lock().unwrap().take();
    if let Some(mut r) = rec {
        r.stop.store(true, Ordering::Relaxed);
        // Detach the capture thread instead of joining here: this command runs on
        // the UI thread, and ffmpeg can take a beat to finalize the file. Joining
        // would freeze the window and global hotkeys until it returns. The thread
        // closes ffmpeg's input and writes the file on its own.
        drop(r.handle.take());
        log("recording stop requested");
    }
    hide_recorder(&app);
    Ok(())
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
        .register_uri_scheme_protocol("frozen", |ctx, _req| {
            let app = ctx.app_handle();
            let guard = app.state::<AppState>();
            let frozen = guard.frozen.lock().unwrap();
            match frozen.as_ref() {
                Some(f) => tauri::http::Response::builder()
                    .header("Content-Type", "image/bmp")
                    .header("Cache-Control", "no-store")
                    .header("Access-Control-Allow-Origin", "*")
                    .body(f.bytes.clone())
                    .unwrap(),
                None => tauri::http::Response::builder()
                    .status(404)
                    .body(Vec::new())
                    .unwrap(),
            }
        })
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
                    if !is_area && !is_full {
                        return;
                    }
                    // While recording, either screenshot hotkey acts as a global
                    // stop — a reliable way out even if the indicator lost focus.
                    if state.recording.lock().unwrap().is_some() {
                        log("hotkey while recording -> stop");
                        let _ = stop_recording(app.clone());
                        return;
                    }
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
            start_recording,
            stop_recording,
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
                recording: Mutex::new(None),
            });
            apply_shortcuts(&handle);
            if let Err(e) = build_tray(&handle) {
                log(&format!("build_tray failed: {e}"));
            }
            let want_autostart = handle.state::<AppState>().settings.lock().unwrap().autostart;
            apply_autostart(&handle, want_autostart);
            create_indicator_windows(&handle);
            create_overlay_window(&handle);
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
