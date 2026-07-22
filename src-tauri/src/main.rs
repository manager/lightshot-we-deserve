#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

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

// Cap the log so it can't grow without bound across months of uptime; one
// previous generation is kept for post-mortems.
const LOG_MAX_BYTES: u64 = 2 * 1024 * 1024;

fn log(msg: &str) {
    let line = format!(
        "[{}] {}\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
        msg
    );
    let path = log_path();
    if fs::metadata(path).map(|m| m.len() > LOG_MAX_BYTES).unwrap_or(false) {
        let old = path.with_extension("log.old");
        // Windows rename refuses to overwrite; clear the target first.
        let _ = fs::remove_file(&old);
        let _ = fs::rename(path, &old);
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
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
// One frozen monitor. Each monitor keeps its own 1:1 frame and gets its own
// overlay window: a single image stitched across monitors can only be right
// for one DPI scale factor, so mixed-DPI setups (100/125/150%) would misalign
// the selection on every non-primary screen.
struct FrozenShot {
    bytes: Vec<u8>,
    // Virtual-desktop origin of this monitor. The overlay windows are
    // positioned from THESE coordinates, not from a fresh monitor query: if
    // the layout changed between grab and show, sizing to new bounds would
    // stretch the old image across them.
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

struct Frozen {
    shots: Vec<FrozenShot>,
    nonce: u64,
}

#[derive(Serialize, Clone)]
struct FrozenInfo {
    x: i32,
    y: i32,
    url: String,
    width: u32,
    height: u32,
}

struct Recording {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

// Count of capture threads still flushing their video file. stop_recording
// detaches the thread (joining would freeze the UI), so quit must wait on this
// counter instead of a JoinHandle or it kills ffmpeg mid-write and leaves a
// corrupt mp4.
static FINALIZING: AtomicU32 = AtomicU32::new(0);

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

// Full-screen shot targets the monitor the cursor is on, so the hotkey shoots
// the screen the user is actually working on; primary is only the fallback.
fn full_capture_monitor() -> Result<xcap::Monitor, String> {
    if let Some((cx, cy)) = cursor_pos() {
        if let Ok(m) = xcap::Monitor::from_point(cx, cy) {
            return Ok(m);
        }
    }
    primary_monitor()
}

fn capture_full(dir: &PathBuf) -> Result<String, String> {
    let _guard = CAPTURE_LOCK.lock().unwrap();
    warm_up_if_dirty("full capture");
    // Same anti-glitch discipline the area capture has: a frame whose size
    // disagrees with the monitor is the squeezed mid-re-init capture Windows
    // hands out after wake/unlock. Retry until it settles; on the last attempt
    // accept whatever we get rather than fail the hotkey outright.
    const MAX_ATTEMPTS: u32 = 10;
    let mut image = None;
    for attempt in 1..=MAX_ATTEMPTS {
        let monitor = full_capture_monitor()?;
        let (w, h) = (monitor.width().unwrap_or(0), monitor.height().unwrap_or(0));
        match monitor.capture_image() {
            Ok(img) => {
                if attempt < MAX_ATTEMPTS
                    && w != 0
                    && h != 0
                    && (img.width() != w || img.height() != h)
                {
                    log(&format!(
                        "full capture came back {}x{} for a {w}x{h} monitor; retrying",
                        img.width(),
                        img.height()
                    ));
                } else {
                    image = Some(img);
                    break;
                }
            }
            Err(e) => {
                if attempt == MAX_ATTEMPTS {
                    return Err(format!("capture_image failed: {e}"));
                }
                log(&format!("full capture failed ({e}); retrying"));
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(120));
    }
    let image = image.ok_or_else(|| "no frame captured".to_string())?;
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
        let (w, h) = (m.width().unwrap_or(0), m.height().unwrap_or(0));
        layout.push((x, y, w, h));
        match m.capture_image() {
            Ok(img) => {
                // A frame whose size disagrees with what the monitor reports is
                // the "squeezed" capture Windows hands out while the display is
                // still re-initializing after wake/dock — reject the attempt so
                // the caller retries once it settles. NOTE: on the GDI path
                // (our build) BitBlt always returns the requested size, so this
                // check can never fire there; the event-driven dirty/clean
                // generations above are the real staleness defense.
                if !force && w != 0 && h != 0 && (img.width() != w || img.height() != h) {
                    log(&format!(
                        "monitor at ({x},{y}) reports {w}x{h} but frame is {}x{}; unsettled",
                        img.width(),
                        img.height()
                    ));
                    return Ok(None);
                }
                placed.push((x, y, img));
            }
            Err(e) => {
                log(&format!("monitor capture failed at ({x},{y}): {e}"));
                // Right after wake/unlock a monitor can refuse the grab while it
                // re-initializes. Treat that like an unsettled layout so the
                // caller retries, instead of silently shipping a composite with
                // that screen missing.
                if !force {
                    return Ok(None);
                }
            }
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

    // One shot per monitor, kept at its native pixel size. Uncompressed BMP:
    // the blob is only displayed in the overlay, so we skip the PNG
    // compression pass entirely: lossless and the fastest encode available.
    let mut shots = Vec::new();
    for (x, y, img) in placed {
        let (w, h) = (img.width(), img.height());
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            use image::ImageEncoder;
            image::codecs::bmp::BmpEncoder::new(&mut buf)
                .write_image(img.as_raw(), w, h, image::ExtendedColorType::Rgba8)
                .map_err(|e| format!("bmp encode failed: {e}"))?;
        }
        shots.push(FrozenShot {
            bytes: buf.into_inner(),
            x,
            y,
            width: w,
            height: h,
        });
    }
    log(&format!("frozen {} monitor frame(s)", shots.len()));
    Ok(Some(Frozen {
        shots,
        nonce: next_nonce(),
    }))
}

// Display "generation" counters. Anything that can invalidate the OS capture
// path (unlock, sleep resume, display change, layout move) bumps DIRTY_GEN;
// a successful warm-up records which generation it cleaned. On the GDI path a
// stale first frame LOOKS valid (BitBlt always returns the requested size), so
// staleness can only be tracked by events, never detected from the frame; and
// a plain boolean would lose an event that arrives mid-warm-up.
static DIRTY_GEN: AtomicU64 = AtomicU64::new(1); // session start counts as dirty
static CLEAN_GEN: AtomicU64 = AtomicU64::new(0);

fn mark_display_dirty(why: &str) {
    DIRTY_GEN.fetch_add(1, Ordering::SeqCst);
    log(&format!("display marked dirty: {why}"));
}

// Serializes all still captures (background warm-up vs user screenshots) so
// they never interleave. Recording frames don't take it: the watcher already
// stays off the screen while a recording runs.
static CAPTURE_LOCK: Mutex<()> = Mutex::new(());

// If the display changed since the last known-good capture, take one throwaway
// full grab so the visible re-init "squeeze" and the stale frame are burned
// here, and the caller's real capture is already the clean second frame.
// Call with CAPTURE_LOCK held.
fn warm_up_if_dirty(context: &str) {
    let dirty = DIRTY_GEN.load(Ordering::SeqCst);
    if CLEAN_GEN.load(Ordering::SeqCst) >= dirty {
        return;
    }
    log(&format!("{context}: display dirty; taking a throwaway warm-up frame"));
    match grab_frozen_once(true) {
        // Store the generation read BEFORE the grab: an event that lands
        // mid-grab keeps the display dirty instead of being lost.
        Ok(_) => CLEAN_GEN.store(dirty, Ordering::SeqCst),
        Err(e) => log(&format!("warm-up frame failed: {e}")),
    }
}

fn grab_frozen() -> Result<Frozen, String> {
    let _guard = CAPTURE_LOCK.lock().unwrap();
    warm_up_if_dirty("area capture");
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

// Windows brings a display up lazily after wake / re-dock: the first capture is
// what forces the re-init (the visible screen "squeeze"), and that capture comes
// back at the stale resolution — which is why the first screenshot after sleep
// or re-attaching a monitor never worked. Watch the layout from a background
// thread and take a throwaway capture as soon as it settles after a change, so
// the init cost is paid right away instead of at the user's next hotkey press.
// The hotkey paths warm up on demand (warm_up_if_dirty), so this watcher is
// the background half: it notices layout changes the event window can't see,
// and pays the warm-up cost during idle time so the user's next hotkey press
// usually doesn't have to.
fn spawn_display_watcher(app: AppHandle) {
    std::thread::spawn(move || {
        let mut last = monitor_layout().unwrap_or_default();
        // Consecutive failed warm-ups. After a few, fall back to a forced grab
        // and call it clean, otherwise one permanently uncapturable monitor
        // would make every future screenshot pay a throwaway frame.
        let mut fails: u32 = 0;
        loop {
            std::thread::sleep(std::time::Duration::from_millis(1000));
            let now = match monitor_layout() {
                Ok(l) => l,
                Err(_) => continue,
            };
            if now != last {
                last = now;
                mark_display_dirty("monitor layout changed");
                continue; // still moving — warm up only after two reads agree
            }
            let dirty = DIRTY_GEN.load(Ordering::SeqCst);
            if CLEAN_GEN.load(Ordering::SeqCst) >= dirty {
                fails = 0;
                continue;
            }
            // A full-desktop grab mid-recording could hiccup the video;
            // leave it dirty and retry after the recording ends.
            if app.state::<AppState>().recording.lock().unwrap().is_some() {
                continue;
            }
            let _guard = CAPTURE_LOCK.lock().unwrap();
            // Non-forced first: it refuses partial results (a monitor that
            // failed to grab), which must stay dirty rather than count as
            // warmed. The generation stored is the one read BEFORE grabbing.
            match grab_frozen_once(fails >= 5) {
                Ok(Some(_)) => {
                    CLEAN_GEN.store(dirty, Ordering::SeqCst);
                    fails = 0;
                    log("display warm-up capture done");
                }
                Ok(None) => {
                    fails += 1;
                    log("display warm-up unsettled; will retry");
                }
                Err(e) => {
                    fails += 1;
                    log(&format!("display warm-up capture failed: {e}"));
                }
            }
        }
    });
}

// Locking the session (Win+L), unlocking it, and sleep/resume all tear down the
// OS capture path WITHOUT changing the monitor layout, so the geometry poll in
// spawn_display_watcher never notices them. The next hotkey capture then pays
// the display re-init: the visible full-screen "squeeze" glitch. A hidden
// window subscribed to session and power broadcasts flags a warm-up instead, so
// the re-init happens right at unlock/resume where the screen is transitioning
// anyway, not at the user's next screenshot.
#[cfg(windows)]
fn spawn_session_watcher() {
    std::thread::spawn(|| unsafe {
        use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
        use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows_sys::Win32::System::RemoteDesktop::WTSRegisterSessionNotification;
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, RegisterClassW,
            TranslateMessage, MSG, WM_DISPLAYCHANGE, WM_POWERBROADCAST, WNDCLASSW,
        };

        unsafe extern "system" fn wnd_proc(
            hwnd: HWND,
            msg: u32,
            wp: WPARAM,
            lp: LPARAM,
        ) -> LRESULT {
            const WM_WTSSESSION_CHANGE: u32 = 0x02B1;
            const WTS_CONSOLE_CONNECT: WPARAM = 0x1;
            const WTS_SESSION_UNLOCK: WPARAM = 0x8;
            const PBT_APMRESUMESUSPEND: WPARAM = 0x7;
            const PBT_APMRESUMEAUTOMATIC: WPARAM = 0x12;
            match msg {
                WM_WTSSESSION_CHANGE
                    if wp == WTS_SESSION_UNLOCK || wp == WTS_CONSOLE_CONNECT =>
                {
                    mark_display_dirty("session unlocked");
                    0
                }
                WM_POWERBROADCAST
                    if wp == PBT_APMRESUMESUSPEND || wp == PBT_APMRESUMEAUTOMATIC =>
                {
                    mark_display_dirty("resumed from sleep");
                    1
                }
                // Resolution / display count / re-plug of a monitor. Also
                // covers an HDMI swap that ends up with the SAME geometry,
                // which the layout poll can never distinguish.
                WM_DISPLAYCHANGE => {
                    mark_display_dirty("display change broadcast");
                    0
                }
                _ => DefWindowProcW(hwnd, msg, wp, lp),
            }
        }

        let class_name: Vec<u16> = "lwd-session-watch\0".encode_utf16().collect();
        let hinstance = GetModuleHandleW(std::ptr::null());
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class_name.as_ptr(),
        };
        if RegisterClassW(&wc) == 0 {
            log("session watcher: RegisterClassW failed");
            return;
        }
        // A real (hidden) top-level window, not a message-only one: broadcasts
        // like WM_POWERBROADCAST are never delivered to message-only windows.
        let hwnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            class_name.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinstance,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            log("session watcher: CreateWindowExW failed");
            return;
        }
        // NOTIFY_FOR_THIS_SESSION: lock/unlock of our own session only.
        if WTSRegisterSessionNotification(hwnd, 0) == 0 {
            log("session watcher: WTSRegisterSessionNotification failed");
        }
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    });
}

#[cfg(not(windows))]
fn spawn_session_watcher() {}

// The exported PNG arrives as a raw binary invoke body (Tauri v2), not a
// base64 data URL: on a large selection the multi-MB base64 JSON string was
// the save/copy latency culprit.
fn request_png<'a>(request: &'a tauri::ipc::Request) -> Result<&'a [u8], String> {
    match request.body() {
        tauri::ipc::InvokeBody::Raw(b) => Ok(b.as_slice()),
        _ => Err("expected binary png payload".into()),
    }
}

// Minimal decodeURIComponent counterpart: filenames can be any unicode but
// HTTP-style invoke headers cannot, so the overlay percent-encodes the name.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
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

// Per-recording capture source. Resolves the monitor under the region once and
// then grabs ONLY the region rect each frame; the old path re-enumerated all
// monitors and copied the whole screen 30-60 times a second, which is what made
// recording eat CPU. Fills `buf` as tightly-packed RGBA of exactly w*h pixels;
// anything outside the monitor stays black, so the frame size never changes
// mid-recording (ffmpeg's raw input requires a fixed size). On any grab error
// the cached monitor is dropped and re-resolved next frame, so a display change
// mid-recording degrades to black frames instead of ending the video.
struct RegionGrabber {
    mon: Option<xcap::Monitor>,
    ax: i32,
    ay: i32,
    w: u32,
    h: u32,
}

impl RegionGrabber {
    fn new(ax: i32, ay: i32, w: u32, h: u32) -> Self {
        Self { mon: None, ax, ay, w, h }
    }

    fn fill(&mut self, buf: &mut [u8]) {
        for b in buf.iter_mut() {
            *b = 0;
        }
        if self.mon.is_none() {
            let cx = self.ax + (self.w as i32) / 2;
            let cy = self.ay + (self.h as i32) / 2;
            self.mon = xcap::Monitor::from_point(cx, cy).ok();
        }
        let mon = match &self.mon {
            Some(m) => m,
            None => return,
        };
        let mx = mon.x().unwrap_or(0);
        let my = mon.y().unwrap_or(0);
        let mw = mon.width().unwrap_or(0) as i32;
        let mh = mon.height().unwrap_or(0) as i32;
        // Region in monitor-local coordinates, clipped to the monitor.
        let ox = self.ax - mx;
        let oy = self.ay - my;
        let cx0 = ox.max(0);
        let cy0 = oy.max(0);
        let cx1 = (ox + self.w as i32).min(mw);
        let cy1 = (oy + self.h as i32).min(mh);
        if cx1 <= cx0 || cy1 <= cy0 {
            return;
        }
        let (rw, rh) = ((cx1 - cx0) as u32, (cy1 - cy0) as u32);
        let img = match mon.capture_region(cx0, cy0, rw, rh) {
            Ok(i) => i,
            Err(_) => {
                self.mon = None;
                return;
            }
        };
        if img.width() != rw || img.height() != rh {
            self.mon = None;
            return;
        }
        let raw = img.as_raw();
        let dx = (cx0 - ox) as usize;
        let dy = (cy0 - oy) as usize;
        let span = rw as usize * 4;
        for row in 0..rh as usize {
            let si = row * span;
            let di = ((dy + row) * self.w as usize + dx) * 4;
            buf[di..di + span].copy_from_slice(&raw[si..si + span]);
        }
    }
}

// ---------- windows ----------

// Freezing the desktop can take a while right after wake (capture retries up
// to ~1.2s), and the hotkey handler runs on the UI thread: grab on a worker
// so the tray and windows never stall, and swallow repeat presses while a
// grab is already in flight.
static FREEZING: AtomicBool = AtomicBool::new(false);

fn begin_area_capture(app: &AppHandle) {
    if FREEZING.swap(true, Ordering::SeqCst) {
        log("area capture already in progress; ignoring re-trigger");
        return;
    }
    let app = app.clone();
    std::thread::spawn(move || {
        match grab_frozen() {
            Ok(shot) => {
                *app.state::<AppState>().frozen.lock().unwrap() = Some(shot);
                log("screen frozen for area capture");
                show_overlay(&app);
            }
            Err(e) => log(&format!("freeze failed: {e}")),
        }
        FREEZING.store(false, Ordering::SeqCst);
    });
}

// One selection overlay per monitor, labeled overlay-0..N-1. Each window shows
// its own monitor's 1:1 frame, which keeps mixed-DPI setups pixel-accurate.
// The JS derives its shot index from the window label.
const OVERLAY_PREFIX: &str = "overlay-";

fn overlay_label(i: usize) -> String {
    format!("{OVERLAY_PREFIX}{i}")
}

fn build_overlay_window(app: &AppHandle, i: usize) -> Option<tauri::WebviewWindow> {
    match WebviewWindowBuilder::new(app, overlay_label(i), WebviewUrl::App("overlay.html".into()))
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
        Ok(w) => Some(w),
        Err(e) => {
            log(&format!("overlay {i} build failed: {e}"));
            None
        }
    }
}

// Build the overlay windows once, hidden, at startup. Creating a webview the
// first time a hotkey fires makes the screen flash on that first capture;
// pre-creating means the windows already exist and only need to be filled with
// the frozen frame and shown, seamless from the very first press.
fn create_overlay_windows(app: &AppHandle) {
    let count = xcap::Monitor::all().map(|m| m.len()).unwrap_or(1).max(1);
    for i in 0..count {
        if app.get_webview_window(&overlay_label(i)).is_none()
            && build_overlay_window(app, i).is_some()
        {
            log(&format!("overlay window {i} pre-created"));
        }
    }
}

fn show_overlay(app: &AppHandle) {
    let geoms: Vec<(i32, i32, u32, u32)> = app
        .state::<AppState>()
        .frozen
        .lock()
        .unwrap()
        .as_ref()
        .map(|f| f.shots.iter().map(|s| (s.x, s.y, s.width, s.height)).collect())
        .unwrap_or_default();
    if geoms.is_empty() {
        log("show_overlay: no frozen shots");
        return;
    }
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        let app = handle;
        for (i, &(x, y, w, h)) in geoms.iter().enumerate() {
            let win = match app.get_webview_window(&overlay_label(i)) {
                Some(w) => Some(w),
                // A monitor was attached after startup; build its window now.
                None => build_overlay_window(&app, i),
            };
            let win = match win {
                Some(w) => w,
                None => continue,
            };
            let _ = win.set_position(PhysicalPosition::new(x, y));
            let _ = win.set_size(PhysicalSize::new(w, h));
        }
        // A monitor may have disappeared since the last capture; make sure the
        // extra windows stay out of the way.
        let mut i = geoms.len();
        while let Some(win) = app.get_webview_window(&overlay_label(i)) {
            let _ = win.hide();
            i += 1;
        }
        // Each overlay stays hidden until its JS has painted its own frame and
        // called `overlay_ready`, so the dim never appears a beat late.
        let _ = app.emit("frozen-ready", ());
    });
}

fn hide_overlay(app: &AppHandle) {
    for (label, win) in app.webview_windows() {
        if label.starts_with(OVERLAY_PREFIX) {
            let _ = win.hide();
        }
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
fn get_frozen(state: State<AppState>, idx: usize) -> Option<FrozenInfo> {
    let guard = state.frozen.lock().unwrap();
    let f = guard.as_ref()?;
    let s = f.shots.get(idx)?;
    Some(FrozenInfo {
        url: format!("http://frozen.localhost/{}/{idx}.bmp", f.nonce),
        x: s.x,
        y: s.y,
        width: s.width,
        height: s.height,
    })
}

// A selection can only live on one monitor at a time: when the user starts
// dragging on one overlay, the others clear theirs.
#[tauri::command]
fn claim_overlay(app: AppHandle, idx: usize) {
    let _ = app.emit("overlay-claimed", idx);
}

#[tauri::command]
fn save_capture(app: AppHandle, request: tauri::ipc::Request) -> Result<String, String> {
    let bytes = request_png(&request)?;
    let name = request
        .headers()
        .get("x-name")
        .and_then(|v| v.to_str().ok())
        .map(percent_decode);
    let dir = resolve_save_dir(&app.state::<AppState>());
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let file_name = match name {
        Some(n) if !n.trim().is_empty() => custom_name(&n),
        _ => timestamp_name(),
    };
    let path = dir.join(file_name);
    fs::write(&path, bytes).map_err(|e| format!("save failed: {e}"))?;
    let p = path.to_string_lossy().to_string();
    log(&format!("saved area screenshot: {p}"));
    hide_overlay(&app);
    Ok(p)
}

#[tauri::command]
fn copy_capture(app: AppHandle, request: tauri::ipc::Request) -> Result<(), String> {
    let bytes = request_png(&request)?;
    let img = image::load_from_memory(bytes)
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
fn overlay_ready(window: tauri::WebviewWindow) {
    let _ = window.show();
    // Only the overlay under the mouse takes focus, so Esc and Ctrl+Z go where
    // the user is. If the cursor can't be read, take focus anyway rather than
    // leave the keyboard dead.
    let focus = match (cursor_pos(), window.outer_position(), window.outer_size()) {
        (Some((cx, cy)), Ok(pos), Ok(size)) => {
            cx >= pos.x
                && cx < pos.x + size.width as i32
                && cy >= pos.y
                && cy < pos.y + size.height as i32
        }
        _ => true,
    };
    if focus {
        let _ = window.set_focus();
    }
}

#[tauri::command]
fn cancel_area(app: AppHandle) {
    log("area selection cancelled");
    hide_overlay(&app);
}

// Begin recording the selected region. (x, y, w, h) are absolute physical px
// in virtual-desktop space, matching the frozen frame the user selected on.
// The overlay is dismissed before the first frame so it never appears in the
// video.
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

    // (x, y) already arrive as absolute virtual-desktop physical coordinates:
    // each overlay adds its own monitor's origin before invoking.
    let ax = x;
    let ay = y;

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
    // Incremented before spawn so quit can never race past a starting recording.
    FINALIZING.fetch_add(1, Ordering::SeqCst);
    let handle = std::thread::spawn(move || {
        log("capture thread running");
        // Let the overlay actually disappear from the compositor first.
        std::thread::sleep(std::time::Duration::from_millis(300));
        let frame_bytes = (w as usize) * (h as usize) * 4;
        let mut buf = vec![0u8; frame_bytes];
        let mut grabber = RegionGrabber::new(ax, ay, w, h);
        let interval = std::time::Duration::from_micros(1_000_000 / fps as u64);
        let start = std::time::Instant::now();
        let mut written: u64 = 0;
        let mut ffmpeg_died = false;
        while !stop_t.load(Ordering::Relaxed) {
            let tick_start = std::time::Instant::now();
            grabber.fill(&mut buf);
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
        FINALIZING.fetch_sub(1, Ordering::SeqCst);
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
                // Finish an in-flight recording first so the file isn't left
                // corrupt: signal stop, then wait for the capture thread(s) to
                // flush ffmpeg (bounded: ffmpeg itself is killed after 15s).
                let rec = app.state::<AppState>().recording.lock().unwrap().take();
                if let Some(r) = rec {
                    log("quit: stopping active recording");
                    r.stop.store(true, Ordering::Relaxed);
                }
                let deadline =
                    std::time::Instant::now() + std::time::Duration::from_secs(20);
                while FINALIZING.load(Ordering::SeqCst) > 0
                    && std::time::Instant::now() < deadline
                {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
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
        .register_uri_scheme_protocol("frozen", |ctx, req| {
            let app = ctx.app_handle();
            let guard = app.state::<AppState>();
            let frozen = guard.frozen.lock().unwrap();
            // Path shape: /{nonce}/{idx}.bmp. The nonce busts the webview
            // cache per capture, the index picks the monitor's shot.
            let idx = req
                .uri()
                .path()
                .rsplit('/')
                .next()
                .and_then(|seg| seg.strip_suffix(".bmp"))
                .and_then(|s| s.parse::<usize>().ok());
            let shot = idx.and_then(|i| frozen.as_ref().and_then(|f| f.shots.get(i)));
            match shot {
                Some(s) => tauri::http::Response::builder()
                    .header("Content-Type", "image/bmp")
                    .header("Cache-Control", "no-store")
                    .header("Access-Control-Allow-Origin", "*")
                    .body(s.bytes.clone())
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
            claim_overlay,
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
            create_overlay_windows(&handle);
            spawn_display_watcher(handle.clone());
            spawn_session_watcher();
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
