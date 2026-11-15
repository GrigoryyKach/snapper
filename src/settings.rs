//! Settings mode (`Snapper.exe --settings`): an egui window in the suite style. Changes apply
//! immediately: the config is saved and the resident process is signalled to reload it.

use crate::config::{Config, Format};
use crate::hotkey::{self, Hotkey, MOD_ALT, MOD_CONTROL, MOD_SHIFT};
use crate::i18n::{self, Lang};
use crate::nimvoro::{self, UpdateState, Updater};
use crate::strings::{self, S};
use crate::sys;
use crate::theme::{self, Btn, Pal, Textures};
use eframe::egui::{self, Align, Color32, Frame, Layout, Margin, RichText, ScrollArea, Ui};
use std::time::{Duration, Instant};
use windows::core::w;
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS};
use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW, SetForegroundWindow, ShowWindow, SW_RESTORE, WM_CLOSE};

const TITLE: &str = "Snapper Settings";
const REPO_URL: &str = "https://github.com/GrigoryyKach/snapper";
const TEST_HOTKEY_ID: i32 = 0x5A5A;

pub fn run() -> eframe::Result<()> {
    // Only one settings window: a second launch focuses the first.
    unsafe {
        let m = CreateMutexW(None, true, w!("Local\\Snapper.Settings"));
        if m.is_ok() && GetLastError() == ERROR_ALREADY_EXISTS {
            if let Ok(h) = FindWindowW(None, w!("Snapper Settings")) {
                let _ = ShowWindow(h, SW_RESTORE);
                let _ = SetForegroundWindow(h);
            }
            return Ok(());
        }
        std::mem::forget(m);
    }
    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png")).unwrap_or_default();
    let viewport = egui::ViewportBuilder::default()
        .with_title(TITLE)
        .with_app_id("local.snapper.settings")
        .with_inner_size([500.0, 720.0])
        .with_min_inner_size([440.0, 560.0])
        .with_icon(std::sync::Arc::new(icon));
    let options = eframe::NativeOptions { viewport, centered: true, ..Default::default() };
    let cfg = Config::load();
    eframe::run_native(TITLE, options, Box::new(move |cc| Ok(Box::new(App::new(cc, cfg)))))
}

#[derive(Clone, Copy, PartialEq)]
enum HkErr {
    Taken,
    Unsupported,
}

struct App {
    cfg: Config,
    lang: Lang,
    tex: Textures,
    updater: Updater,
    autostart: bool,
    recording: bool,
    hk_err: Option<HkErr>,
    status: Option<sys::HotkeyStatus>,
    resident: bool,
    last_poll: Instant,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, mut cfg: Config) -> Self {
        theme::install(&cc.egui_ctx, cfg.theme);
        let lang = i18n::resolve(cfg.language.as_deref());
        if cfg.language.as_deref() != Some(lang.code()) {
            cfg.language = Some(lang.code().to_string());
            cfg.save();
        }
        let updater = Updater::new("snapper", "GrigoryyKach/snapper", env!("CARGO_PKG_VERSION"));
        Self {
            cfg,
            lang,
            tex: Textures::new(&cc.egui_ctx),
            updater,
            autostart: sys::autostart_enabled(),
            recording: false,
            hk_err: None,
            status: sys::read_status(),
            resident: sys::resident_window().is_some(),
            last_poll: Instant::now(),
        }
    }

    fn s(&self) -> &'static S {
        strings::tr(self.lang)
    }

    fn apply(&mut self) {
        self.cfg.save();
        sys::signal_reload();
    }

    fn poll(&mut self) {
        if self.last_poll.elapsed() > Duration::from_millis(400) {
            self.last_poll = Instant::now();
            self.status = sys::read_status();
            self.resident = sys::resident_window().is_some();
            self.autostart = sys::autostart_enabled();
        }
    }

    /// While recording: the first key press in the window becomes the hotkey. (Print Screen only
    /// reports a key release on Windows, so releases count for that key.)
    fn record_step(&mut self, ctx: &egui::Context) {
        let events = ctx.input(|i| i.events.clone());
        for ev in events {
            let egui::Event::Key { key, pressed, modifiers, repeat: false, .. } = ev else { continue };
            if key == egui::Key::Escape {
                self.recording = false;
                return;
            }
            let Some(vk) = key_to_vk(key) else { continue };
            if !pressed && vk != hotkey::VK_SNAPSHOT {
                continue;
            }
            let mut mods = 0;
            if modifiers.ctrl {
                mods |= MOD_CONTROL;
            }
            if modifiers.alt {
                mods |= MOD_ALT;
            }
            if modifiers.shift {
                mods |= MOD_SHIFT;
            }
            let hk = Hotkey { mods, vk };
            self.recording = false;
            if mods == 0 && !hotkey::allowed_without_modifier(vk) {
                self.hk_err = Some(HkErr::Unsupported);
                return;
            }
            let text = hk.format();
            if text == self.cfg.hotkey {
                self.hk_err = None;
                return;
            }
            if try_register(&hk) {
                self.hk_err = None;
                self.cfg.hotkey = text;
                self.status = None;
                self.apply();
            } else {
                self.hk_err = Some(HkErr::Taken);
            }
            return;
        }
    }

    fn header(&self, ui: &mut Ui, p: &Pal) {
        let t = self.s();
        ui.horizontal(|ui| {
            theme::logo(ui, 36.0, "\u{2702}", &self.tex);
            ui.add_space(4.0);
            ui.vertical(|ui| {
                ui.add_space(1.0);
                ui.label(RichText::new("Snapper").font(theme::semi(16.0)).color(p.text));
                ui.add_space(-8.0);
                ui.label(RichText::new(t.subtitle).font(theme::reg(12.0)).color(p.muted));
            });
        });
        ui.add_space(14.0);
    }

    fn update_banner(&mut self, ui: &mut Ui, p: &Pal) {
        let UpdateState::Available { version, .. } = self.updater.state() else { return };
        let u = nimvoro::strings(self.lang);
        Frame::new().fill(p.accent_soft).corner_radius(10.0).inner_margin(Margin::symmetric(12, 10)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(RichText::new(u.available.replace("{v}", &version)).font(theme::semi(13.5)).color(p.text));
                    ui.add_space(-6.0);
                    theme::hint(ui, &u.you_have.replace("{v}", self.updater.current), p);
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if theme::button(ui, u.update_btn, Btn::Soft, p).clicked() {
                        close_resident();
                        if self.updater.apply() {
                            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    }
                });
            });
        });
        ui.add_space(12.0);
    }

    fn hotkey_section(&mut self, ui: &mut Ui, p: &Pal) {
        let t = self.s();
        theme::caps(ui, t.sec_capture, p);
        ui.add_space(2.0);
        let w = ui.available_width();
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.set_width(w - 150.0);
                ui.label(RichText::new(t.hotkey).font(theme::reg(14.0)).color(p.text));
                ui.add_space(-6.0);
                theme::hint(ui, t.hotkey_hint, p);
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if theme::button(ui, t.record, Btn::Soft, p).clicked() {
                    self.recording = true;
                    self.hk_err = None;
                }
                ui.add_space(4.0);
                let label = if self.recording { "\u{2026}".to_string() } else { self.cfg.hotkey.clone() };
                key_chip(ui, &label, self.recording, p);
            });
        });
        ui.add_space(4.0);

        // status line
        let (text, color): (String, Color32) = if self.recording {
            (t.recording.to_string(), p.accent)
        } else if let Some(e) = self.hk_err {
            (
                match e {
                    HkErr::Taken => t.hk_taken.to_string(),
                    HkErr::Unsupported => t.hk_unsupported.to_string(),
                },
                Color32::from_rgb(0xe0, 0x4f, 0x4f),
            )
        } else if !self.resident {
            (t.resident_off.to_string(), p.muted)
        } else {
            match &self.status {
                Some(st) if st.hotkey == self.cfg.hotkey && st.ok => (t.hk_ok.to_string(), Color32::from_rgb(0x2f, 0xa8, 0x5f)),
                Some(st) if st.hotkey == self.cfg.hotkey => {
                    let mut msg = t.hk_taken.to_string();
                    if hotkey_is_prtsc(&self.cfg.hotkey) && sys::prtsc_snipping_enabled() {
                        msg = t.hk_prtsc_warn.to_string();
                    }
                    (msg, Color32::from_rgb(0xe0, 0x4f, 0x4f))
                }
                _ => (t.hk_pending.to_string(), p.muted),
            }
        };
        ui.label(RichText::new(text).font(theme::reg(12.5)).color(color));
        if self.cfg.hotkey != "PrintScreen" {
            ui.add_space(2.0);
            if theme::button(ui, t.reset, Btn::Link, p).clicked() {
                self.cfg.hotkey = "PrintScreen".into();
                self.hk_err = None;
                self.status = None;
                self.apply();
            }
        }
        ui.add_space(10.0);

        // format
        ui.horizontal(|ui| {
            ui.label(RichText::new(t.format).font(theme::reg(14.0)).color(p.text));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                for f in [Format::Jpg, Format::Png] {
                    let kind = if self.cfg.format == f { Btn::Soft } else { Btn::Ghost };
                    let label = if f == Format::Png { "PNG" } else { "JPG" };
                    if theme::button(ui, label, kind, p).clicked() && self.cfg.format != f {
                        self.cfg.format = f;
                        self.apply();
                    }
                    ui.add_space(4.0);
                }
            });
        });
        ui.add_space(8.0);
        let mut v = self.cfg.copy_on_save;
        if switch_row(ui, p, t.copy_on_save, t.copy_on_save_hint, &mut v) {
            self.cfg.copy_on_save = v;
            self.apply();
        }
    }

    fn saving_section(&mut self, ui: &mut Ui, p: &Pal) {
        let t = self.s();
        theme::divider(ui, p);
        theme::caps(ui, t.sec_saving, p);
        ui.add_space(2.0);
        ui.label(RichText::new(t.folder).font(theme::reg(14.0)).color(p.text));
        ui.add_space(2.0);
        let folder = self.cfg.save_folder();
        ui.horizontal(|ui| {
            Frame::new().fill(p.bg).stroke(egui::Stroke::new(1.0, p.line)).corner_radius(9.0).inner_margin(Margin::symmetric(10, 7)).show(ui, |ui| {
                ui.set_width((ui.available_width() - 168.0).max(80.0));
                ui.add(egui::Label::new(RichText::new(folder.display().to_string()).font(theme::reg(13.0)).color(p.text)).truncate());
            });
            if theme::button(ui, t.browse, Btn::Soft, p).clicked() {
                if let Some(dir) = rfd::FileDialog::new().set_directory(&folder).pick_folder() {
                    self.cfg.save_dir = dir.display().to_string();
                    self.apply();
                }
            }
            if theme::button(ui, t.open, Btn::Ghost, p).clicked() {
                sys::open_folder(&folder);
            }
        });
        ui.add_space(8.0);
    }

    fn system_section(&mut self, ui: &mut Ui, p: &Pal) {
        let t = self.s();
        let u = nimvoro::strings(self.lang);
        theme::divider(ui, p);
        theme::caps(ui, t.sec_system, p);
        ui.add_space(2.0);
        let mut auto = self.autostart;
        if switch_row(ui, p, t.autostart, t.autostart_hint, &mut auto) && auto != self.autostart {
            if sys::set_autostart(auto) {
                self.autostart = auto;
                self.cfg.autostart = auto;
                self.apply();
            }
        }
        ui.add_space(4.0);
        let mut check = self.cfg.check_updates;
        if switch_row(ui, p, u.check_on_start, u.check_on_start_hint, &mut check) {
            self.cfg.check_updates = check;
            self.apply();
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            theme::hint(ui, &self.updater.status_text(self.lang), p);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if !self.updater.is_checking() && theme::button(ui, u.check_now, Btn::Link, p).clicked() {
                    let c = ui.ctx().clone();
                    self.updater.check(move || c.request_repaint());
                }
            });
        });
        ui.add_space(8.0);
    }

    fn interface_section(&mut self, ui: &mut Ui, p: &Pal) {
        let t = self.s();
        theme::divider(ui, p);
        theme::caps(ui, t.sec_interface, p);
        ui.add_space(2.0);
        let mut lang = self.lang;
        if i18n::language_row(ui, &mut lang, p) {
            self.lang = lang;
            self.cfg.language = Some(lang.code().to_string());
            self.apply();
        }
        ui.add_space(4.0);
        let mut mode = self.cfg.theme;
        if i18n::theme_row(ui, &mut mode, self.lang, p) {
            self.cfg.theme = mode;
            theme::apply(ui.ctx(), mode);
            self.apply();
        }
        ui.add_space(8.0);
    }

    fn about_section(&mut self, ui: &mut Ui, p: &Pal) {
        let t = self.s();
        theme::divider(ui, p);
        theme::caps(ui, t.sec_about, p);
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{} {}", t.version, env!("CARGO_PKG_VERSION"))).font(theme::reg(13.5)).color(p.text));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if theme::button(ui, t.project_page, Btn::Link, p).clicked() {
                    sys::open_url(REPO_URL);
                }
            });
        });
        ui.add_space(10.0);
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll();
        if self.recording {
            self.record_step(ctx);
            ctx.request_repaint_after(Duration::from_millis(100));
        } else {
            ctx.request_repaint_after(Duration::from_millis(450));
        }
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let p = Pal::of(ui.visuals().dark_mode);
        egui::CentralPanel::default().frame(Frame::new().fill(p.bg).inner_margin(Margin::symmetric(26, 22))).show(ui, |ui| {
            ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
                ui.set_width(ui.available_width() - 8.0);
                self.header(ui, &p);
                self.update_banner(ui, &p);
                self.hotkey_section(ui, &p);
                self.saving_section(ui, &p);
                self.system_section(ui, &p);
                self.interface_section(ui, &p);
                self.about_section(ui, &p);
            });
        });
    }
}

// ───────────────────────── widgets ─────────────────────────

fn key_chip(ui: &mut Ui, text: &str, active: bool, p: &Pal) {
    let font = theme::semi(13.0);
    let g = ui.painter().layout_no_wrap(text.to_string(), font, p.text);
    let w = g.size().x + 24.0;
    let (r, _) = ui.allocate_exact_size(egui::vec2(w, 30.0), egui::Sense::hover());
    let stroke = egui::Stroke::new(1.0, if active { p.accent } else { p.line });
    ui.painter().rect(r, 9.0, p.card, stroke, egui::StrokeKind::Inside);
    ui.painter().galley(egui::pos2(r.left() + 12.0, r.center().y - g.size().y / 2.0), g, p.text);
}

fn switch_row(ui: &mut Ui, p: &Pal, title: &str, hint: &str, value: &mut bool) -> bool {
    let mut changed = false;
    let w = ui.available_width();
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.set_width(w - 58.0);
            ui.label(RichText::new(title).font(theme::reg(14.0)).color(p.text));
            ui.add_space(-6.0);
            theme::hint(ui, hint, p);
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            changed = theme::switch(ui, value, p).changed();
        });
    });
    changed
}

// ───────────────────────── hotkeys ─────────────────────────

/// Maps an egui key to a Windows virtual-key code through the shared hotkey names.
fn key_to_vk(k: egui::Key) -> Option<u32> {
    let n = format!("{k:?}");
    let name = if let Some(d) = n.strip_prefix("Num").filter(|d| d.chars().all(|c| c.is_ascii_digit())) {
        d.to_string()
    } else if let Some(a) = n.strip_prefix("Arrow") {
        a.to_string()
    } else if n == "Backtick" {
        "Tilde".to_string()
    } else {
        n
    };
    hotkey::key_from_name(&name)
}

fn hotkey_is_prtsc(s: &str) -> bool {
    Hotkey::parse(s).is_some_and(|h| h.vk == hotkey::VK_SNAPSHOT)
}

/// Validates a combination by trying to register it for this thread, then releasing it.
fn try_register(hk: &Hotkey) -> bool {
    unsafe {
        let ok = RegisterHotKey(None, TEST_HOTKEY_ID, HOT_KEY_MODIFIERS(hk.mods | 0x4000), hk.vk).is_ok();
        if ok {
            let _ = UnregisterHotKey(None, TEST_HOTKEY_ID);
        }
        ok
    }
}

fn close_resident() {
    if let Some(h) = sys::resident_window() {
        unsafe {
            let _ = PostMessageW(Some(h), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
        std::thread::sleep(Duration::from_millis(400));
    }
    let _: Option<HWND> = None;
}
