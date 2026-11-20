//! Resident mode on Linux (default): tray icon (StatusNotifier/AppIndicator through `tray-icon`),
//! global hotkey on X11 (`global-hotkey`), a control socket for `--capture` and `--settings`, and
//! the hand-off to the capture overlay.
//!
//! A capture works like this: this process grabs the screen (X11 `GetImage` or the Wayland
//! screenshot portal), writes it to a private temp folder, and starts `Snapper --overlay <folder>`.
//! The overlay is a separate process because an egui event loop can run only once per process.
//! When it exits, this process reads the result and copies or saves it. The clipboard is owned by
//! this long-lived process, which X11 requires.

use crate::config::{self, Config};
use crate::files;
use crate::hotkey::{Hotkey, MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN};
use crate::i18n::{self, Lang};
use crate::nimvoro::{UpdateState, Updater};
use crate::overlay::{self, Action, Done};
use crate::strings;
use crate::support;
use crate::sys;
use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

#[derive(Debug, PartialEq)]
pub enum Msg {
    Capture,
    Settings,
    Reload,
    Quit,
}

pub fn parse_msg(s: &str) -> Option<Msg> {
    match s {
        "capture" => Some(Msg::Capture),
        "settings" => Some(Msg::Settings),
        "reload" => Some(Msg::Reload),
        "quit" => Some(Msg::Quit),
        _ => None,
    }
}

// ───────────────────────── hotkey mapping ─────────────────────────

/// W3C `KeyboardEvent.code` name for one of our key names (see `hotkey::key_name`).
pub fn code_name(key: &str) -> Option<String> {
    let k = key.trim();
    if k.len() == 1 {
        let c = k.chars().next()?;
        if c.is_ascii_alphabetic() {
            return Some(format!("Key{}", c.to_ascii_uppercase()));
        }
        if c.is_ascii_digit() {
            return Some(format!("Digit{c}"));
        }
    }
    if let Some(n) = k.strip_prefix('F') {
        if n.parse::<u32>().is_ok_and(|n| (1..=24).contains(&n)) {
            return Some(k.to_string());
        }
    }
    Some(
        match k {
            "PrintScreen" => "PrintScreen",
            "Pause" => "Pause",
            "ScrollLock" => "ScrollLock",
            "Insert" => "Insert",
            "Delete" => "Delete",
            "Home" => "Home",
            "End" => "End",
            "PageUp" => "PageUp",
            "PageDown" => "PageDown",
            "Space" => "Space",
            "Enter" => "Enter",
            "Tab" => "Tab",
            "Backspace" => "Backspace",
            "Left" => "ArrowLeft",
            "Up" => "ArrowUp",
            "Right" => "ArrowRight",
            "Down" => "ArrowDown",
            "Tilde" => "Backquote",
            _ => return None,
        }
        .to_string(),
    )
}

pub fn to_global(hk: &Hotkey) -> Option<HotKey> {
    let code = Code::from_str(&code_name(&crate::hotkey::key_name(hk.vk)?)?).ok()?;
    let mut m = Modifiers::empty();
    if hk.mods & MOD_CONTROL != 0 {
        m |= Modifiers::CONTROL;
    }
    if hk.mods & MOD_ALT != 0 {
        m |= Modifiers::ALT;
    }
    if hk.mods & MOD_SHIFT != 0 {
        m |= Modifiers::SHIFT;
    }
    if hk.mods & MOD_WIN != 0 {
        m |= Modifiers::SUPER;
    }
    Some(HotKey::new(if m.is_empty() { None } else { Some(m) }, code))
}

/// Settings uses this to check a combination before saving it. Wayland cannot grab keys: always `true` there.
pub fn can_register(hk: &Hotkey) -> bool {
    if sys::is_wayland() {
        return true;
    }
    let Some(g) = to_global(hk) else { return false };
    let Ok(m) = GlobalHotKeyManager::new() else { return false };
    let ok = m.register(g).is_ok();
    let _ = m.unregister(g);
    ok
}

/// Trigger in the XDG shortcuts format (`CTRL+SHIFT+s`, `Print`) for the GlobalShortcuts portal.
pub fn portal_trigger(hk: &Hotkey) -> Option<String> {
    let key = crate::hotkey::key_name(hk.vk)?;
    let sym = match key.as_str() {
        "PrintScreen" => "Print".to_string(),
        "Enter" => "Return".to_string(),
        "Space" => "space".to_string(),
        "Tilde" => "grave".to_string(),
        "Backspace" => "BackSpace".to_string(),
        "PageUp" => "Page_Up".to_string(),
        "PageDown" => "Page_Down".to_string(),
        "ScrollLock" => "Scroll_Lock".to_string(),
        k if k.len() == 1 => k.to_ascii_lowercase(),
        k => k.to_string(),
    };
    let mut parts: Vec<String> = Vec::new();
    if hk.mods & MOD_CONTROL != 0 {
        parts.push("CTRL".into());
    }
    if hk.mods & MOD_ALT != 0 {
        parts.push("ALT".into());
    }
    if hk.mods & MOD_SHIFT != 0 {
        parts.push("SHIFT".into());
    }
    if hk.mods & MOD_WIN != 0 {
        parts.push("LOGO".into());
    }
    parts.push(sym);
    Some(parts.join("+"))
}

/// Wayland: binds the capture shortcut through the GlobalShortcuts portal (KDE and recent GNOME).
/// Runs for the life of the process; on any failure the system-shortcut fallback in Settings stays.
fn spawn_portal_shortcut(tx: Sender<Msg>, trigger: String, hotkey_text: String) {
    use ashpd::desktop::global_shortcuts::{GlobalShortcuts, NewShortcut};
    use futures_util::StreamExt;
    std::thread::spawn(move || {
        let res: Result<(), String> = pollster::block_on(async move {
            let gs = GlobalShortcuts::new().await.map_err(|e| e.to_string())?;
            let session = gs.create_session().await.map_err(|e| e.to_string())?;
            let sc = NewShortcut::new("capture", "Take a screenshot").preferred_trigger(trigger.as_str());
            gs.bind_shortcuts(&session, &[sc], None).await.map_err(|e| e.to_string())?.response().map_err(|e| e.to_string())?;
            sys::write_status(&hotkey_text, true);
            let mut activated = gs.receive_activated().await.map_err(|e| e.to_string())?;
            while let Some(a) = activated.next().await {
                if a.shortcut_id() == "capture" {
                    let _ = tx.send(Msg::Capture);
                }
            }
            Ok(())
        });
        if let Err(e) = res {
            sys::log(&format!("GlobalShortcuts portal: {e}"));
        }
    });
}

// ───────────────────────── state ─────────────────────────

struct Job {
    child: Child,
    dir: PathBuf,
}

struct Items {
    take: MenuItem,
    settings: MenuItem,
    folder: MenuItem,
    updates: Submenu,
    status: MenuItem,
    update_to: MenuItem,
    check_now: MenuItem,
    check_start: CheckMenuItem,
    support: MenuItem,
    exit: MenuItem,
}

struct State {
    cfg: Config,
    lang: Lang,
    updater: Updater,
    update_flag: Arc<AtomicBool>,
    announced_update: bool,
    settings: Option<Child>,
    manager: Option<GlobalHotKeyManager>,
    hotkey: Option<HotKey>,
    job: Option<Job>,
    pending: Option<Instant>,
    items: Items,
    _tray: Option<TrayIcon>,
    rx: Receiver<Msg>,
    tx: Sender<Msg>,
    portal_started: bool,
    update_item_visible: bool,
}

static JOB_SEQ: AtomicU32 = AtomicU32::new(0);

fn load_icon() -> Option<Icon> {
    let img = image::load_from_memory(include_bytes!("../assets/icon.png")).ok()?;
    let img = img.resize_exact(64, 64, image::imageops::FilterType::Lanczos3).to_rgba8();
    Icon::from_rgba(img.into_raw(), 64, 64).ok()
}

/// `capture_on_start`: take a screenshot right after startup (`Snapper --capture` with nothing running).
pub fn run(capture_on_start: bool) -> i32 {
    let Some(listener) = sys::listen() else {
        // Already running: a plain second launch opens Settings, `--capture` takes a screenshot.
        let ok = sys::send(if capture_on_start { "capture" } else { "settings" });
        return if ok { 0 } else { 1 };
    };
    if gtk::init().is_err() {
        eprintln!("Snapper: cannot open the display (GTK init failed)");
        return 1;
    }
    let first_run = !config::config_path().exists();
    let mut cfg = Config::load();
    let lang = i18n::resolve(cfg.language.as_deref());
    if cfg.language.as_deref() != Some(lang.code()) {
        cfg.language = Some(lang.code().to_string());
        cfg.save();
    }

    let (tx, rx) = channel::<Msg>();
    spawn_listener(listener, tx.clone());

    let t = strings::tr(lang);
    let menu = Menu::new();
    let items = Items {
        take: MenuItem::new(t.tray_take, true, None),
        settings: MenuItem::new(t.tray_settings, true, None),
        folder: MenuItem::new(t.tray_folder, true, None),
        updates: Submenu::new(t.tray_updates, true),
        status: MenuItem::new("", false, None),
        update_to: MenuItem::new("", false, None),
        check_now: MenuItem::new(t.tray_check_now, true, None),
        check_start: CheckMenuItem::new(t.tray_check_start, true, cfg.check_updates, None),
        support: MenuItem::new(support::tray_label(lang), true, None),
        exit: MenuItem::new(t.tray_exit, true, None),
    };
    let _ = items.updates.append_items(&[&items.status, &items.update_to, &PredefinedMenuItem::separator(), &items.check_now, &items.check_start]);
    let _ = menu.append_items(&[
        &items.take,
        &items.settings,
        &items.folder,
        &items.updates,
        &PredefinedMenuItem::separator(),
        &items.support,
        &PredefinedMenuItem::separator(),
        &items.exit,
    ]);
    let mut builder = TrayIconBuilder::new().with_menu(Box::new(menu)).with_tooltip(t.tray_tip.replace("{hk}", &cfg.hotkey)).with_title("Snapper");
    if let Some(i) = load_icon() {
        builder = builder.with_icon(i);
    }
    let tray = builder.build().ok();
    if tray.is_none() {
        sys::log("tray icon could not be created");
    }

    let updater = Updater::new("snapper", "GrigoryyKach/snapper", env!("CARGO_PKG_VERSION")).exit_with(exit_resident);
    let mut st = State {
        cfg,
        lang,
        updater,
        update_flag: Arc::new(AtomicBool::new(false)),
        announced_update: false,
        settings: None,
        manager: None,
        hotkey: None,
        job: None,
        pending: if capture_on_start { Some(Instant::now() + Duration::from_millis(300)) } else { None },
        items,
        _tray: tray,
        rx,
        tx,
        portal_started: false,
        update_item_visible: false,
    };
    register_hotkey(&mut st);
    if first_run {
        let t = strings::tr(st.lang);
        let body = if sys::is_wayland() { t.wl_text.split(':').next().unwrap_or("").to_string() } else { t.welcome_body.replace("{hk}", &st.cfg.hotkey) };
        notify(t.welcome_title, &body, None);
    }
    if st.cfg.check_updates {
        start_update_check(&mut st);
    }

    let mut ids = Ids::of(&st.items);
    gtk::glib::timeout_add_local(Duration::from_millis(40), move || {
        if poll(&mut st, &mut ids) {
            gtk::glib::ControlFlow::Continue
        } else {
            let _ = std::fs::remove_file(sys::socket_path());
            gtk::main_quit();
            gtk::glib::ControlFlow::Break
        }
    });
    gtk::main();
    0
}

struct Ids {
    take: MenuId,
    settings: MenuId,
    folder: MenuId,
    update_to: MenuId,
    check_now: MenuId,
    check_start: MenuId,
    support: MenuId,
    exit: MenuId,
}

impl Ids {
    fn of(i: &Items) -> Ids {
        Ids {
            take: i.take.id().clone(),
            settings: i.settings.id().clone(),
            folder: i.folder.id().clone(),
            update_to: i.update_to.id().clone(),
            check_now: i.check_now.id().clone(),
            check_start: i.check_start.id().clone(),
            support: i.support.id().clone(),
            exit: i.exit.id().clone(),
        }
    }
}

fn spawn_listener(l: std::os::unix::net::UnixListener, tx: Sender<Msg>) {
    std::thread::spawn(move || {
        for conn in l.incoming().flatten() {
            for c in sys::read_commands(conn) {
                if let Some(m) = parse_msg(&c) {
                    let _ = tx.send(m);
                }
            }
        }
    });
}

/// One timer tick. Returns `false` to quit.
fn poll(s: &mut State, ids: &mut Ids) -> bool {
    while let Ok(m) = s.rx.try_recv() {
        match m {
            Msg::Capture => s.pending = Some(Instant::now() + Duration::from_millis(150)),
            Msg::Settings => open_settings(s),
            Msg::Reload => reload_config(s),
            Msg::Quit => return false,
        }
    }
    while let Ok(ev) = GlobalHotKeyEvent::receiver().try_recv() {
        if ev.state == HotKeyState::Pressed && s.hotkey.is_some_and(|h| h.id() == ev.id) {
            s.pending = Some(Instant::now());
        }
    }
    while let Ok(ev) = MenuEvent::receiver().try_recv() {
        let id = ev.id;
        if id == ids.take {
            s.pending = Some(Instant::now() + Duration::from_millis(350)); // let the menu close first
        } else if id == ids.settings {
            open_settings(s);
        } else if id == ids.folder {
            sys::open_folder(&s.cfg.save_folder());
        } else if id == ids.update_to {
            if s.updater.apply_if(s.cfg.save()) {
                return false;
            }
        } else if id == ids.check_now {
            s.announced_update = false;
            start_update_check(s);
        } else if id == ids.check_start {
            let mut disk = Config::load();
            disk.check_updates = s.items.check_start.is_checked();
            disk.save();
            s.cfg.check_updates = disk.check_updates;
        } else if id == ids.support {
            support::open();
        } else if id == ids.exit {
            return false;
        }
    }
    if s.pending.is_some_and(|t| Instant::now() >= t) {
        s.pending = None;
        start_capture(s);
    }
    poll_job(s);
    refresh_update_items(s);
    if s.update_flag.swap(false, Ordering::SeqCst) {
        if let UpdateState::Available { version, .. } = s.updater.state() {
            if !s.announced_update {
                s.announced_update = true;
                let t = strings::tr(s.lang);
                let u = crate::nimvoro::strings(s.lang);
                notify(t.update_title, &u.available.replace("{v}", &version), None);
            }
        }
    }
    true
}

fn refresh_update_items(s: &mut State) {
    let status = s.updater.status_text(s.lang);
    if s.items.status.text() != status {
        s.items.status.set_text(&status);
    }
    let t = strings::tr(s.lang);
    match s.updater.state() {
        UpdateState::Available { version, .. } => {
            let text = t.tray_update_to.replace("{v}", &version);
            if s.items.update_to.text() != text {
                s.items.update_to.set_text(&text);
            }
            if !s.update_item_visible {
                s.items.update_to.set_enabled(true);
                s.update_item_visible = true;
            }
        }
        _ => {
            if s.update_item_visible {
                s.items.update_to.set_enabled(false);
                s.items.update_to.set_text("");
                s.update_item_visible = false;
            }
        }
    }
}

fn start_update_check(s: &mut State) {
    let flag = s.update_flag.clone();
    s.updater.check(move || flag.store(true, Ordering::SeqCst));
}

fn open_settings(s: &mut State) {
    if let Some(child) = &mut s.settings {
        if matches!(child.try_wait(), Ok(None)) {
            return; // already open: the settings process refuses a second window
        }
        s.settings = None;
    }
    if let Some(exe) = sys::exe_path() {
        if let Ok(child) = Command::new(exe).arg("--settings").spawn() {
            s.settings = Some(child);
        }
    }
}

fn reload_config(s: &mut State) {
    let mut cfg = Config::load();
    let lang = i18n::resolve(cfg.language.as_deref());
    if cfg.language.is_none() {
        cfg.language = Some(lang.code().to_string());
    }
    s.cfg = cfg;
    s.lang = lang;
    let t = strings::tr(lang);
    s.items.take.set_text(t.tray_take);
    s.items.settings.set_text(t.tray_settings);
    s.items.folder.set_text(t.tray_folder);
    s.items.updates.set_text(t.tray_updates);
    s.items.check_now.set_text(t.tray_check_now);
    s.items.check_start.set_text(t.tray_check_start);
    s.items.check_start.set_checked(s.cfg.check_updates);
    s.items.support.set_text(support::tray_label(lang));
    s.items.exit.set_text(t.tray_exit);
    register_hotkey(s);
}

/// (Re)registers the configured hotkey and publishes the result for the settings window.
fn register_hotkey(s: &mut State) {
    let text = s.cfg.hotkey.clone();
    if let (Some(m), Some(h)) = (&s.manager, s.hotkey.take()) {
        let _ = m.unregister(h);
    }
    if sys::is_wayland() {
        // No app-level global hotkeys on Wayland. Try the GlobalShortcuts portal once; Settings always
        // explains how to bind `Snapper --capture` as a fallback.
        sys::write_status(&text, false);
        if !s.portal_started {
            s.portal_started = true;
            if let Some(trigger) = Hotkey::parse(&text).and_then(|h| portal_trigger(&h)) {
                spawn_portal_shortcut(s.tx.clone(), trigger, text);
            }
        }
        return;
    }
    let hk = Hotkey::parse(&text).unwrap_or_default();
    let ok = (|| {
        let g = to_global(&hk)?;
        if s.manager.is_none() {
            s.manager = GlobalHotKeyManager::new().map_err(|e| sys::log(&format!("hotkey manager: {e}"))).ok();
        }
        if let Err(e) = s.manager.as_ref()?.register(g) {
            sys::log(&format!("hotkey {text}: {e}"));
            return None;
        }
        s.hotkey = Some(g);
        Some(())
    })()
    .is_some();
    sys::write_status(&text, ok);
    if !ok {
        let t = strings::tr(s.lang);
        notify(t.hk_failed_title, &t.hk_failed_body.replace("{hk}", &hk.format()), None);
    }
}

// ───────────────────────── capture ─────────────────────────

fn work_dir() -> PathBuf {
    let base = sys::socket_path().parent().map(|p| p.to_path_buf()).unwrap_or_else(std::env::temp_dir);
    base.join(format!("snapper-cap-{}-{}", std::process::id(), JOB_SEQ.fetch_add(1, Ordering::SeqCst)))
}

fn start_capture(s: &mut State) {
    if s.job.is_some() {
        return;
    }
    let t = strings::tr(s.lang);
    let shot = match crate::capture::grab() {
        Ok(shot) => shot,
        Err(e) => {
            sys::log(&format!("capture failed: {e}"));
            notify("Snapper", t.capture_failed, None);
            return;
        }
    };
    let dir = work_dir();
    {
        use std::os::unix::fs::DirBuilderExt;
        if std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir).is_err() {
            notify("Snapper", t.capture_failed, None);
            return;
        }
    }
    if std::fs::write(dir.join("cap.raw"), crate::capture::encode_shot(&shot)).is_err() {
        let _ = std::fs::remove_dir_all(&dir);
        notify("Snapper", t.capture_failed, None);
        return;
    }
    drop(shot);
    let Some(exe) = sys::exe_path() else { return };
    match Command::new(exe).arg("--overlay").arg(&dir).spawn() {
        Ok(child) => s.job = Some(Job { child, dir }),
        Err(e) => {
            sys::log(&format!("cannot start the overlay: {e}"));
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

fn poll_job(s: &mut State) {
    let Some(job) = &mut s.job else { return };
    if !matches!(job.child.try_wait(), Ok(Some(_)) | Err(_)) {
        return;
    }
    let job = s.job.take().unwrap();
    let done = overlay::read_result(&job.dir);
    let _ = std::fs::remove_dir_all(&job.dir);
    capture_done(s, done);
}

fn capture_done(s: &mut State, d: Done) {
    // remember colour and stroke without clobbering settings changed meanwhile
    let mut disk = Config::load();
    let color = config::format_hex_color(d.color);
    if disk.last_color != color || disk.last_stroke as usize != d.stroke {
        disk.last_color = color;
        disk.last_stroke = d.stroke as u8;
        disk.save();
    }
    s.cfg.last_color = disk.last_color;
    s.cfg.last_stroke = disk.last_stroke;
    let Some(img) = &d.image else { return };
    let t = strings::tr(s.lang);
    match d.action {
        Action::Copy => {
            if crate::clipboard::copy_image(img).is_err() {
                notify("Snapper", t.copy_failed, None);
            }
        }
        Action::Save => save_image(s, img),
        Action::Cancel => {}
    }
}

fn local_now() -> (u16, u16, u16, u16, u16, u16) {
    // SAFETY: localtime_r only writes into the struct we pass.
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        ((tm.tm_year + 1900) as u16, (tm.tm_mon + 1) as u16, tm.tm_mday as u16, tm.tm_hour as u16, tm.tm_min as u16, tm.tm_sec as u16)
    }
}

fn save_image(s: &mut State, img: &files::Image) {
    let t = strings::tr(s.lang);
    let cfg = s.cfg.clone();
    let dir = cfg.save_folder();
    let result = (|| -> Result<PathBuf, String> {
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let (y, mo, d, h, mi, sec) = local_now();
        let base = files::base_name(y, mo, d, h, mi, sec);
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
            notify("Snapper", &t.saved.replace("{f}", &name), Some(path));
        }
        Err(_) => notify("Snapper", t.save_failed, None),
    }
}

/// Desktop notification. With a path, clicking it opens the folder of that file.
fn notify(title: &str, body: &str, reveal: Option<PathBuf>) {
    let (title, body) = (title.to_string(), body.to_string());
    std::thread::spawn(move || {
        let mut n = notify_rust::Notification::new();
        n.summary(&title).body(&body).icon("camera-photo").appname("Snapper");
        if reveal.is_some() {
            n.action("default", "Show");
        }
        if let Ok(h) = n.show() {
            if let Some(p) = reveal {
                h.wait_for_action(|a| {
                    if a == "default" {
                        sys::reveal_file(&p);
                    }
                });
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_parse() {
        assert_eq!(parse_msg("capture"), Some(Msg::Capture));
        assert_eq!(parse_msg("settings"), Some(Msg::Settings));
        assert_eq!(parse_msg("rm -rf"), None);
    }

    #[test]
    fn key_names_map_to_dom_codes() {
        assert_eq!(code_name("S").as_deref(), Some("KeyS"));
        assert_eq!(code_name("5").as_deref(), Some("Digit5"));
        assert_eq!(code_name("F10").as_deref(), Some("F10"));
        assert_eq!(code_name("PrintScreen").as_deref(), Some("PrintScreen"));
        assert_eq!(code_name("Left").as_deref(), Some("ArrowLeft"));
        assert_eq!(code_name("Tilde").as_deref(), Some("Backquote"));
        assert_eq!(code_name("Banana"), None);
    }

    #[test]
    fn every_hotkey_the_settings_can_record_maps_to_a_global_key() {
        for s in ["PrintScreen", "Ctrl+Shift+S", "Alt+F4", "Ctrl+Alt+Left", "Win+Space", "Ctrl+5", "Shift+Tilde", "F12", "Pause", "Ctrl+PageDown"] {
            let hk = Hotkey::parse(s).unwrap_or_else(|| panic!("{s}"));
            assert!(to_global(&hk).is_some(), "{s}");
        }
    }

    #[test]
    fn portal_triggers() {
        assert_eq!(portal_trigger(&Hotkey::parse("PrintScreen").unwrap()).as_deref(), Some("Print"));
        assert_eq!(portal_trigger(&Hotkey::parse("Ctrl+Shift+S").unwrap()).as_deref(), Some("CTRL+SHIFT+s"));
        assert_eq!(portal_trigger(&Hotkey::parse("Alt+Win+F10").unwrap()).as_deref(), Some("ALT+LOGO+F10"));
        assert_eq!(portal_trigger(&Hotkey::parse("Ctrl+Space").unwrap()).as_deref(), Some("CTRL+space"));
    }

    #[test]
    fn modifiers_are_translated() {
        let g = to_global(&Hotkey::parse("Ctrl+Shift+S").unwrap()).unwrap();
        let expect = HotKey::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::KeyS);
        assert_eq!(g.id(), expect.id());
    }
}

/// Called from the update thread once the new version is running: asks this process to quit by its normal path.
fn exit_resident() {
    crate::sys::send("quit");
}
