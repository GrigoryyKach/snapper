//! Resident mode (default): hidden message window, tray icon and menu, global hotkey, and the
//! hand-off to the capture overlay. A plain Win32 message loop: no eframe, so it stays small.
//! The settings window is a separate process (`Snapper.exe --settings`) that signals this one
//! through a registered window message after it saves the config.

use crate::config::{self, Config};
use crate::files;
use crate::hotkey::{Hotkey, MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN};
use crate::i18n::{self, Lang};
use crate::nimvoro::{UpdateState, Updater};
use crate::support;
use crate::geom::Rect;
use crate::overlay::{self, Action, Done, Params};
use crate::scroll;
use crate::strings;
use crate::sys::{self, wide};
use crate::theme::ThemeMode;
use std::cell::RefCell;
use std::path::PathBuf;
use std::process::Child;
use windows::core::{w, PCSTR, PCWSTR};
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::{CreateMutexW, GetCurrentProcess, SetProcessWorkingSetSize};
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

const WM_TRAY: u32 = WM_APP + 1;
const WM_UPDATE_DONE: u32 = WM_APP + 2;
const WM_SCROLL_DONE: u32 = WM_APP + 3;
const WM_TEXT_DONE: u32 = WM_APP + 4;
const WM_TEXT_BUSY: u32 = WM_APP + 5;
const HOTKEY_ID: i32 = 1;
const HOTKEY_SCROLL_ID: i32 = 2;
/// Temporary hotkeys while a scrolling capture runs (the user is browsing, so Esc is not ours).
const HOTKEY_SCROLL_DONE: i32 = 3;
const HOTKEY_SCROLL_CANCEL: i32 = 4;
const TIMER_CAPTURE: usize = 1;
const TIMER_PREWARM: usize = 2;
const TIMER_SCROLL: usize = 3;
const NIN_SELECT_: u32 = 0x400;
const NIN_BALLOONUSERCLICK_: u32 = 0x405;

const ID_TAKE: usize = 1;
const ID_SCROLL: usize = 4;
const ID_SETTINGS: usize = 2;
const ID_FOLDER: usize = 3;
const ID_CHECK_NOW: usize = 10;
const ID_CHECK_START: usize = 11;
const ID_UPDATE: usize = 12;
const ID_SUPPORT: usize = 90;
const ID_EXIT: usize = 99;

#[derive(Clone)]
enum BalloonKind {
    None,
    Saved(PathBuf),
    Update,
    /// Opens the settings window (hotkey problems).
    Settings,
}

struct State {
    hwnd: HWND,
    cfg: Config,
    lang: Lang,
    hotkey_ok: bool,
    updater: Updater,
    settings: Option<Child>,
    balloon: BalloonKind,
    msg_reload: u32,
    msg_settings: u32,
    msg_taskbar: u32,
    announced_update: bool,
    /// A scrolling capture is running: its shared state and the area it grabs.
    scroll: Option<(std::sync::Arc<scroll::Shared>, Rect)>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn with<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|s| s.try_borrow_mut().ok().and_then(|mut b| b.as_mut().map(f)))
}

pub fn run() -> i32 {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        // Single instance: a second launch just asks the first one to open Settings.
        let mutex = CreateMutexW(None, true, PCWSTR(sys::named("Local\\Snapper.Resident").as_ptr()));
        if mutex.is_ok() && GetLastError() == ERROR_ALREADY_EXISTS {
            if let Some(h) = sys::resident_window() {
                let _ = PostMessageW(Some(h), sys::msg_show_settings(), WPARAM(0), LPARAM(0));
            }
            return 0;
        }
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        let first_run = !config::config_path().exists();
        let mut cfg = Config::load();
        let lang = i18n::resolve(cfg.language.as_deref());
        if cfg.language.as_deref() != Some(lang.code()) {
            cfg.language = Some(lang.code().to_string());
            cfg.save();
        }

        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let class_name = sys::named("SnapperResident");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            lpszClassName: PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            PCWSTR(class_name.as_ptr()),
            w!("Snapper"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinst.into()),
            None,
        ) else {
            return 1;
        };

        let updater = Updater::new("snapper", "GrigoryyKach/snapper", env!("CARGO_PKG_VERSION")).exit_with(exit_resident);
        let state = State {
            hwnd,
            cfg,
            lang,
            hotkey_ok: false,
            updater,
            settings: None,
            balloon: BalloonKind::None,
            msg_reload: sys::msg_reload(),
            msg_settings: sys::msg_show_settings(),
            msg_taskbar: RegisterWindowMessageW(w!("TaskbarCreated")),
            announced_update: false,
            scroll: None,
        };
        STATE.with(|s| *s.borrow_mut() = Some(state));

        apply_menu_theme();
        add_tray();
        register_hotkey(false);
        if first_run {
            with(|s| {
                let t = strings::tr(s.lang);
                let hk = s.cfg.hotkey.clone();
                balloon(s, t.welcome_title, &t.welcome_body.replace("{hk}", &hk), BalloonKind::None);
            });
        }
        with(|s| {
            if s.cfg.check_updates {
                start_update_check(s);
            }
        });
        overlay::prewarm(hwnd);
        trim_memory();

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        0
    }
}

fn trim_memory() {
    unsafe {
        let _ = SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_HOTKEY => {
            match wp.0 as i32 {
                HOTKEY_SCROLL_ID => {
                    // pressed again while capturing: that finishes it
                    if !scroll_request(true) {
                        start_capture(true);
                    }
                }
                HOTKEY_SCROLL_DONE => {
                    scroll_request(true);
                }
                HOTKEY_SCROLL_CANCEL => {
                    scroll_request(false);
                }
                _ => start_capture(false),
            }
            return LRESULT(0);
        }
        WM_SCROLL_DONE => {
            scroll_done();
            return LRESULT(0);
        }
        WM_TEXT_DONE => {
            if let Some(text) = TEXT_NOTICE.lock().ok().and_then(|mut n| n.take()) {
                with(|s| balloon(s, "Snapper", &text, BalloonKind::None));
            }
            return LRESULT(0);
        }
        WM_TEXT_BUSY => {
            with(|s| balloon(s, "Snapper", strings::tr(s.lang).text_busy, BalloonKind::None));
            return LRESULT(0);
        }
        WM_TIMER if wp.0 == TIMER_SCROLL => {
            let _ = KillTimer(Some(hwnd), TIMER_SCROLL);
            start_capture(true);
            return LRESULT(0);
        }
        WM_TIMER if wp.0 == TIMER_CAPTURE => {
            let _ = KillTimer(Some(hwnd), TIMER_CAPTURE);
            start_capture(false);
            return LRESULT(0);
        }
        WM_TIMER if wp.0 == TIMER_PREWARM => {
            let _ = KillTimer(Some(hwnd), TIMER_PREWARM);
            if !overlay::is_active() {
                overlay::prewarm(hwnd);
                trim_memory();
            }
            return LRESULT(0);
        }
        WM_TRAY => {
            let event = (lp.0 & 0xffff) as u32;
            match event {
                NIN_SELECT_ | WM_LBUTTONUP => {
                    let _ = SetTimer(Some(hwnd), TIMER_CAPTURE, 60, None);
                }
                WM_RBUTTONUP | WM_CONTEXTMENU => show_menu(hwnd),
                NIN_BALLOONUSERCLICK_ => on_balloon_click(),
                _ => {}
            }
            return LRESULT(0);
        }
        WM_UPDATE_DONE => {
            with(|s| {
                if let UpdateState::Available { version, .. } = s.updater.state() {
                    if !s.announced_update {
                        s.announced_update = true;
                        let t = strings::tr(s.lang);
                        let u = crate::nimvoro::strings(s.lang);
                        balloon(s, t.update_title, &u.available.replace("{v}", &version), BalloonKind::Update);
                    }
                }
            });
            return LRESULT(0);
        }
        WM_ENDSESSION | WM_DESTROY => {
            remove_tray(hwnd);
            if msg == WM_DESTROY {
                PostQuitMessage(0);
            }
        }
        _ => {}
    }
    let (reload, show_settings, taskbar) = with(|s| (s.msg_reload, s.msg_settings, s.msg_taskbar)).unwrap_or((0, 0, 0));
    if msg == reload && msg != 0 {
        reload_config();
        return LRESULT(0);
    }
    if msg == show_settings && msg != 0 {
        open_settings();
        return LRESULT(0);
    }
    if msg == taskbar && msg != 0 {
        add_tray();
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, msg, wp, lp)
}

// ───────────────────────── capture ─────────────────────────

fn start_capture(scroll: bool) {
    if overlay::is_active() || with(|s| s.scroll.is_some()).unwrap_or(true) {
        return;
    }
    let Some((lang, color, stroke)) = with(|s| {
        let color = config::parse_hex_color(&s.cfg.last_color).unwrap_or(0xff3b30);
        (s.lang, color, s.cfg.last_stroke as usize)
    }) else {
        return;
    };
    overlay::start(Params { lang, color, stroke, scroll });
}

/// Starts the scrolling capture of `area` (the overlay is already gone). The screen stays usable:
/// Done and Cancel are the bar's buttons and two temporary global hotkeys.
fn start_scroll(area: Rect) {
    let Some((hwnd, max_rows, lang, own_key)) = with(|s| (s.hwnd, s.cfg.scroll_max_height as usize, s.lang, s.cfg.scroll_hotkey.clone())) else { return };
    let shared = scroll::spawn(area, max_rows, hwnd, WM_SCROLL_DONE);
    let reg = |id: i32, vk: u32| unsafe { RegisterHotKey(Some(hwnd), id, HOT_KEY_MODIFIERS(MOD_CONTROL | MOD_SHIFT | 0x4000), vk).is_ok() };
    let done_ok = reg(HOTKEY_SCROLL_DONE, 0x0D);
    let cancel_ok = reg(HOTKEY_SCROLL_CANCEL, 0x08);
    let own = Hotkey::parse(&own_key).map(|h| h.format());
    let done_key = own.unwrap_or_else(|| if done_ok { "Ctrl+Shift+Enter".to_string() } else { String::new() });
    let cancel_key = if cancel_ok { "Ctrl+Shift+Backspace" } else { "" };
    scroll::show_ui(shared.clone(), lang, area, &done_key, cancel_key);
    with(|s| s.scroll = Some((shared, area)));
}

/// Done (`true`) or Cancel for the running scrolling capture. Returns `false` when none runs.
fn scroll_request(done: bool) -> bool {
    let Some(Some((sh, _))) = with(|s| s.scroll.clone()) else { return false };
    let flag = if done { &sh.done_req } else { &sh.cancel_req };
    flag.store(true, std::sync::atomic::Ordering::Relaxed);
    true
}

/// The worker finished: open the stitched picture in the viewer and say what happened.
fn scroll_done() {
    scroll::close_ui();
    if let Some(h) = with(|s| s.hwnd) {
        unsafe {
            let _ = UnregisterHotKey(Some(h), HOTKEY_SCROLL_DONE);
            let _ = UnregisterHotKey(Some(h), HOTKEY_SCROLL_CANCEL);
        }
    }
    let Some(Some((shared, area))) = with(|s| s.scroll.take()) else { return };
    let outcome = shared.outcome.lock().ok().and_then(|mut o| o.take());
    let Some((lang, color, stroke)) = with(|s| (s.lang, config::parse_hex_color(&s.cfg.last_color).unwrap_or(0xff3b30), s.cfg.last_stroke as usize)) else {
        return;
    };
    let t = strings::tr(lang);
    let Some(outcome) = outcome else { return };
    let rows = outcome.image.as_ref().map_or(0, |i| i.h);
    crate::sys::log(&format!("scroll done: {:?}, {rows} rows", outcome.note));
    let note = match outcome.note {
        scroll::Note::None => None,
        scroll::Note::Limit => Some(t.scroll_limit.replace("{h}", &rows.to_string())),
        scroll::Note::Nothing => Some(t.scroll_nothing.to_string()),
        scroll::Note::Failed => Some(t.scroll_failed.to_string()),
    };
    let mut ok = false;
    let had_image = outcome.image.is_some();
    if let Some(img) = outcome.image {
        ok = overlay::start_view(Params { lang, color, stroke, scroll: false }, img, area);
    }
    if !ok && note.is_none() && had_image {
        with(|s| balloon(s, "Snapper", t.scroll_failed, BalloonKind::None));
    }
    if let Some(n) = note {
        with(|s| balloon(s, "Snapper", &n, BalloonKind::None));
    }
}

/// Called by the overlay when it closes.
pub fn capture_done(d: Done) {
    with(|s| {
        // remember colour and stroke without clobbering settings changed meanwhile
        let mut disk = Config::load();
        let color = config::format_hex_color(d.color);
        if disk.last_color != color || disk.last_stroke as usize != d.stroke {
            disk.last_color = color;
            disk.last_stroke = d.stroke as u8;
            disk.save();
        }
        s.cfg.last_color = disk.last_color.clone();
        s.cfg.last_stroke = disk.last_stroke;
    });
    if d.action == Action::Scroll {
        if let Some(r) = d.rect {
            start_scroll(r);
        }
        return;
    }
    if d.action == Action::CopyText {
        if let (Some(img), Some(r)) = (d.image, d.rect) {
            start_text_copy(img, r);
        }
        overlay::release_graphics();
        trim_memory();
        if let Some(h) = with(|s| s.hwnd) {
            unsafe {
                let _ = SetTimer(Some(h), TIMER_PREWARM, 1500, None);
            }
        }
        return;
    }
    if let Some(img) = &d.image {
        match d.action {
            Action::Scroll | Action::CopyText => {}
            Action::Copy => {
                if crate::clipboard::copy_image(img).is_err() {
                    with(|s| balloon(s, "Snapper", strings::tr(s.lang).copy_failed, BalloonKind::None));
                }
            }
            Action::Save => save_image(img),
            Action::Cancel => {}
        }
    }
    // Drop the GPU-side caches, then warm up again a moment later so the next capture is still fast.
    overlay::release_graphics();
    trim_memory();
    if let Some(h) = with(|s| s.hwnd) {
        unsafe {
            let _ = SetTimer(Some(h), TIMER_PREWARM, 1500, None);
        }
    }
}

/// The notice of the last finished text copy, taken by the message window.
static TEXT_NOTICE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Reads the text of the region (raw pixels `img` at `area`) off the UI thread, puts it on the
/// clipboard and shows a short notice. A slow read first shows "Reading text".
fn start_text_copy(img: files::Image, area: Rect) {
    let Some((hwnd, lang)) = with(|s| (s.hwnd, s.lang)) else { return };
    let hwnd_raw = hwnd.0 as isize;
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = done.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(1200));
        if !flag.load(std::sync::atomic::Ordering::SeqCst) {
            unsafe {
                let _ = PostMessageW(Some(HWND(hwnd_raw as *mut _)), WM_TEXT_BUSY, WPARAM(0), LPARAM(0));
            }
        }
    });
    std::thread::spawn(move || {
        let out = crate::textcopy::extract(&img, area, lang.code());
        let mut notice = crate::textcopy::notice(lang, &out);
        if let Some(text) = &out.text {
            if crate::clipboard::copy_text(text).is_err() {
                notice = strings::tr(lang).copy_failed.to_string();
            }
        }
        done.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Ok(mut n) = TEXT_NOTICE.lock() {
            *n = Some(notice);
        }
        unsafe {
            let _ = PostMessageW(Some(HWND(hwnd_raw as *mut _)), WM_TEXT_DONE, WPARAM(0), LPARAM(0));
        }
    });
}

fn save_image(img: &files::Image) {
    let Some((cfg, lang)) = with(|s| (s.cfg.clone(), s.lang)) else { return };
    let t = strings::tr(lang);
    let dir = cfg.save_folder();
    let result = (|| -> Result<PathBuf, String> {
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let now = unsafe { GetLocalTime() };
        let base = files::base_name(now.wYear, now.wMonth, now.wDay, now.wHour, now.wMinute, now.wSecond);
        let path = files::unique_path(&dir, &base, cfg.format.ext(), |p| p.exists());
        let data = files::encode(img, cfg.format)?;
        std::fs::write(&path, data).map_err(|e| e.to_string())?;
        Ok(path)
    })();
    match result {
        Ok(path) => {
            if cfg.copy_on_save {
                let _ = crate::clipboard::copy_image(img);
            }
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            with(|s| balloon(s, "Snapper", &t.saved.replace("{f}", &name), BalloonKind::Saved(path)));
        }
        Err(_) => {
            with(|s| balloon(s, "Snapper", t.save_failed, BalloonKind::None));
        }
    }
}

fn on_balloon_click() {
    let kind = with(|s| s.balloon.clone()).unwrap_or(BalloonKind::None);
    match kind {
        BalloonKind::Saved(p) => sys::reveal_file(&p),
        BalloonKind::Update => {
            if with(|s| s.updater.apply_if(s.cfg.save())).unwrap_or(false) {
                quit();
            }
        }
        BalloonKind::Settings => open_settings(),
        BalloonKind::None => {}
    }
}

// ───────────────────────── config / hotkey ─────────────────────────

fn reload_config() {
    with(|s| {
        let mut cfg = Config::load();
        let lang = i18n::resolve(cfg.language.as_deref());
        if cfg.language.is_none() {
            cfg.language = Some(lang.code().to_string());
        }
        s.cfg = cfg;
        s.lang = lang;
    });
    apply_menu_theme();
    update_tray_tip();
    register_hotkey(true);
}

/// (Re)registers the configured hotkey and publishes the result for the settings window.
fn register_hotkey(notify: bool) {
    let Some((hwnd, text, lang)) = with(|s| (s.hwnd, s.cfg.hotkey.clone(), s.lang)) else { return };
    unsafe {
        let _ = UnregisterHotKey(Some(hwnd), HOTKEY_ID);
    }
    let hk = Hotkey::parse(&text).unwrap_or_default();
    let mods = hk.mods & (MOD_ALT | MOD_CONTROL | MOD_SHIFT | MOD_WIN);
    let ok = unsafe { RegisterHotKey(Some(hwnd), HOTKEY_ID, HOT_KEY_MODIFIERS(mods | 0x4000), hk.vk).is_ok() };
    register_scroll_hotkey(hwnd, lang);
    with(|s| s.hotkey_ok = ok);
    sys::write_status(&text, ok);
    update_tray_tip();
    if !ok {
        let t = strings::tr(lang);
        let mut body = t.hk_failed_body.replace("{hk}", &hk.format());
        if hk.vk == crate::hotkey::VK_SNAPSHOT && sys::prtsc_snipping_enabled() {
            body.push_str(t.hk_prtsc_extra);
        }
        with(|s| balloon(s, t.hk_failed_title, &body, BalloonKind::Settings));
    }
    let _ = notify;
}

/// The optional scrolling-capture hotkey; a conflict is reported like the main hotkey.
fn register_scroll_hotkey(hwnd: HWND, lang: Lang) {
    unsafe {
        let _ = UnregisterHotKey(Some(hwnd), HOTKEY_SCROLL_ID);
    }
    let Some(text) = with(|s| s.cfg.scroll_hotkey.clone()) else { return };
    let Some(hk) = Hotkey::parse(&text) else { return };
    let mods = hk.mods & (MOD_ALT | MOD_CONTROL | MOD_SHIFT | MOD_WIN);
    let ok = unsafe { RegisterHotKey(Some(hwnd), HOTKEY_SCROLL_ID, HOT_KEY_MODIFIERS(mods | 0x4000), hk.vk).is_ok() };
    if !ok {
        let t = strings::tr(lang);
        let body = t.hk_failed_body.replace("{hk}", &hk.format());
        with(|s| balloon(s, t.hk_failed_title, &body, BalloonKind::Settings));
    }
}

// ───────────────────────── tray ─────────────────────────

fn copy_wide<const N: usize>(dst: &mut [u16; N], s: &str) {
    let mut i = 0;
    for c in s.encode_utf16() {
        if i >= N - 1 {
            break;
        }
        dst[i] = c;
        i += 1;
    }
    dst[i] = 0;
}

fn tray_icon() -> HICON {
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let cx = GetSystemMetrics(SM_CXSMICON);
        let cy = GetSystemMetrics(SM_CYSMICON);
        if let Ok(h) = LoadImageW(Some(hinst.into()), PCWSTR(1usize as *const u16), IMAGE_ICON, cx, cy, LR_DEFAULTCOLOR) {
            return HICON(h.0);
        }
        LoadIconW(None, IDI_APPLICATION).unwrap_or_default()
    }
}

fn nid(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW { cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32, hWnd: hwnd, uID: 1, ..Default::default() }
}

fn tip_text() -> String {
    with(|s| strings::tr(s.lang).tray_tip.replace("{hk}", &s.cfg.hotkey)).unwrap_or_else(|| "Snapper".into())
}

fn add_tray() {
    let Some(hwnd) = with(|s| s.hwnd) else { return };
    let mut n = nid(hwnd);
    n.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
    n.uCallbackMessage = WM_TRAY;
    n.hIcon = tray_icon();
    copy_wide(&mut n.szTip, &tip_text());
    unsafe {
        let _ = Shell_NotifyIconW(NIM_ADD, &n);
        n.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        let _ = Shell_NotifyIconW(NIM_SETVERSION, &n);
    }
}

fn update_tray_tip() {
    let Some(hwnd) = with(|s| s.hwnd) else { return };
    let mut n = nid(hwnd);
    n.uFlags = NIF_TIP | NIF_SHOWTIP;
    copy_wide(&mut n.szTip, &tip_text());
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &n);
    }
}

fn remove_tray(hwnd: HWND) {
    let n = nid(hwnd);
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &n);
    }
}

fn balloon(s: &mut State, title: &str, text: &str, kind: BalloonKind) {
    s.balloon = kind;
    let mut n = nid(s.hwnd);
    n.uFlags = NIF_INFO;
    n.dwInfoFlags = NIIF_INFO;
    copy_wide(&mut n.szInfoTitle, title);
    copy_wide(&mut n.szInfo, text);
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &n);
    }
}

/// Dark / light popup menus through the (undocumented but stable) uxtheme ordinals 135 and 136.
fn apply_menu_theme() {
    let mode = with(|s| s.cfg.theme).unwrap_or(ThemeMode::System);
    unsafe {
        let Ok(lib) = LoadLibraryW(w!("uxtheme.dll")) else { return };
        let set = GetProcAddress(lib, PCSTR(135usize as *const u8));
        let flush = GetProcAddress(lib, PCSTR(136usize as *const u8));
        if let Some(set) = set {
            let set: unsafe extern "system" fn(i32) -> i32 = std::mem::transmute(set);
            set(match mode {
                ThemeMode::System => 1, // AllowDark: follows the system setting
                ThemeMode::Dark => 2,   // ForceDark
                ThemeMode::Light => 3,  // ForceLight
            });
        }
        if let Some(flush) = flush {
            let flush: unsafe extern "system" fn() = std::mem::transmute(flush);
            flush();
        }
    }
}

fn show_menu(hwnd: HWND) {
    apply_menu_theme();
    let Some((lang, hotkey, scroll_hk, check_start, state, status)) =
        with(|s| (s.lang, s.cfg.hotkey.clone(), s.cfg.scroll_hotkey.clone(), s.cfg.check_updates, s.updater.state(), s.updater.status_text(s.lang)))
    else {
        return;
    };
    let t = strings::tr(lang);
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        let Ok(updates) = CreatePopupMenu() else { return };
        let item = |m: HMENU, flags: MENU_ITEM_FLAGS, id: usize, text: &str| {
            let w = wide(text);
            let _ = AppendMenuW(m, flags, id, PCWSTR(w.as_ptr()));
        };
        item(menu, MF_STRING, ID_TAKE, &format!("{}\t{}", t.tray_take, hotkey));
        let scroll_label = if scroll_hk.trim().is_empty() { t.t_scroll.to_string() } else { format!("{}\t{}", t.t_scroll, scroll_hk) };
        item(menu, MF_STRING, ID_SCROLL, &scroll_label);
        item(menu, MF_STRING, ID_SETTINGS, t.tray_settings);
        item(menu, MF_STRING, ID_FOLDER, t.tray_folder);
        // Updates submenu
        item(updates, MF_STRING | MF_GRAYED, 0, &status);
        if let UpdateState::Available { version, .. } = &state {
            item(updates, MF_STRING, ID_UPDATE, &t.tray_update_to.replace("{v}", version));
        }
        let _ = AppendMenuW(updates, MF_SEPARATOR, 0, PCWSTR::null());
        item(updates, MF_STRING, ID_CHECK_NOW, t.tray_check_now);
        item(updates, if check_start { MF_STRING | MF_CHECKED } else { MF_STRING }, ID_CHECK_START, t.tray_check_start);
        let w = wide(t.tray_updates);
        let _ = AppendMenuW(menu, MF_POPUP, updates.0 as usize, PCWSTR(w.as_ptr()));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        item(menu, MF_STRING, ID_SUPPORT, support::tray_label(lang));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        item(menu, MF_STRING, ID_EXIT, t.tray_exit);

        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, pt.x, pt.y, Some(0), hwnd, None);
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        match cmd.0 as usize {
            ID_TAKE => {
                let _ = SetTimer(Some(hwnd), TIMER_CAPTURE, 200, None);
            }
            ID_SCROLL => {
                let _ = SetTimer(Some(hwnd), TIMER_SCROLL, 200, None);
            }
            ID_SETTINGS => open_settings(),
            ID_FOLDER => {
                if let Some(dir) = with(|s| s.cfg.save_folder()) {
                    sys::open_folder(&dir);
                }
            }
            ID_CHECK_NOW => {
                with(|s| {
                    s.announced_update = false;
                    start_update_check(s);
                });
            }
            ID_CHECK_START => {
                with(|s| {
                    let mut disk = Config::load();
                    disk.check_updates = !check_start;
                    disk.save();
                    s.cfg.check_updates = disk.check_updates;
                });
            }
            ID_UPDATE => {
                if with(|s| s.updater.apply_if(s.cfg.save())).unwrap_or(false) {
                    quit();
                }
            }
            ID_SUPPORT => support::open(),
            ID_EXIT => quit(),
            _ => {}
        }
    }
}

fn start_update_check(s: &mut State) {
    let h = s.hwnd.0 as isize;
    s.updater.check(move || unsafe {
        let _ = PostMessageW(Some(HWND(h as *mut _)), WM_UPDATE_DONE, WPARAM(0), LPARAM(0));
    });
}

fn open_settings() {
    with(|s| {
        if let Some(child) = &mut s.settings {
            if matches!(child.try_wait(), Ok(None)) {
                // already running: the settings process focuses its own window when launched again
            } else {
                s.settings = None;
            }
        }
        let Ok(exe) = std::env::current_exe() else { return };
        if let Ok(child) = std::process::Command::new(exe).arg("--settings").spawn() {
            s.settings = Some(child);
        }
    });
}

/// Called from the update thread once the new version is running: asks this process to quit by its normal path
/// (the window procedure destroys the window, which removes the tray icon).
fn exit_resident() {
    if let Some(h) = sys::resident_window() {
        unsafe {
            let _ = PostMessageW(Some(h), windows::Win32::UI::WindowsAndMessaging::WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
}

fn quit() {
    overlay::cancel();
    scroll_request(false);
    scroll::close_ui();
    if let Some(h) = with(|s| s.hwnd) {
        unsafe {
            let _ = DestroyWindow(h);
        }
    }
}
