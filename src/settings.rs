//! Settings mode (`Snapper.exe --settings`): an egui window in the suite style. Changes apply
//! immediately: the config is saved and the resident process is signalled to reload it.

use crate::config::{Config, Format};
use crate::hotkey::{self, Hotkey, MOD_ALT, MOD_CONTROL, MOD_SHIFT};
use crate::i18n::{self, Lang};
use crate::nimvoro::{self, UpdateState, Updater};
use crate::strings::{self, S};
use crate::support;
use crate::sys;
use crate::theme::{self, Btn, Pal};
use eframe::egui::{self, Align, Color32, Frame, Layout, Margin, RichText, ScrollArea, Ui};
use std::time::{Duration, Instant};
#[cfg(windows)]
use windows::core::{w, PCWSTR};
#[cfg(windows)]
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, HWND, LPARAM, WPARAM};
#[cfg(windows)]
use windows::Win32::System::Threading::CreateMutexW;
#[cfg(windows)]
use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS};
#[cfg(windows)]
use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW, SetForegroundWindow, ShowWindow, SW_RESTORE, WM_CLOSE};

const TITLE: &str = "Snapper Settings";
const REPO_URL: &str = "https://github.com/GrigoryyKach/snapper";
#[cfg(windows)]
const TEST_HOTKEY_ID: i32 = 0x5A5A;

pub fn run() -> eframe::Result<()> {
    // Only one settings window: a second launch focuses the first.
    #[cfg(unix)]
    if !single_instance() {
        return Ok(());
    }
    #[cfg(windows)]
    unsafe {
        let m = CreateMutexW(None, true, PCWSTR(sys::named("Local\\Snapper.Settings").as_ptr()));
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
        .with_inner_size([500.0, 900.0])
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
    logo: egui::TextureHandle,
    updater: Updater,
    autostart: bool,
    recording: bool,
    /// Recording the scrolling-capture hotkey instead of the main one.
    recording_scroll: bool,
    scroll_err: Option<HkErr>,
    hk_err: Option<HkErr>,
    /// Hotkey text of the last rejected attempt (to show the Print Screen hint).
    hk_err_for: String,
    status: Option<sys::HotkeyStatus>,
    resident: bool,
    last_poll: Instant,
    #[cfg(unix)]
    copied: Option<Instant>,
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
            logo: load_logo(&cc.egui_ctx),
            updater,
            autostart: sys::autostart_enabled(),
            recording: false,
            recording_scroll: false,
            scroll_err: None,
            hk_err: None,
            hk_err_for: String::new(),
            status: sys::read_status(),
            resident: sys::resident_window().is_some(),
            last_poll: Instant::now(),
            #[cfg(unix)]
            copied: None,
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
                self.recording_scroll = false;
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
            let scroll = self.recording_scroll;
            self.recording = false;
            self.recording_scroll = false;
            if scroll {
                self.set_scroll_hotkey(Hotkey { mods, vk });
            } else {
                self.set_hotkey(Hotkey { mods, vk });
            }
            return;
        }
    }

    /// Validates `hk` by registering it, then saves and applies it, or shows a clear error.
    fn set_hotkey(&mut self, hk: Hotkey) {
        let text = hk.format();
        self.hk_err_for = text.clone();
        if hk.mods == 0 && !hotkey::allowed_without_modifier(hk.vk) {
            self.hk_err = Some(HkErr::Unsupported);
            return;
        }
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
    }

    /// Validates and saves the optional scrolling-capture hotkey.
    fn set_scroll_hotkey(&mut self, hk: Hotkey) {
        let text = hk.format();
        if hk.mods == 0 && !hotkey::allowed_without_modifier(hk.vk) {
            self.scroll_err = Some(HkErr::Unsupported);
            return;
        }
        if text == self.cfg.scroll_hotkey {
            self.scroll_err = None;
            return;
        }
        if text == self.cfg.hotkey || !try_register(&hk) {
            self.scroll_err = Some(HkErr::Taken);
            return;
        }
        self.scroll_err = None;
        self.cfg.scroll_hotkey = text;
        self.apply();
    }

    /// The optional hotkey that starts a scrolling capture directly.
    fn scroll_row(&mut self, ui: &mut Ui, p: &Pal) {
        let t = self.s();
        ui.add_space(6.0);
        let w = ui.available_width();
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.set_width(w - 190.0);
                ui.label(RichText::new(t.scroll_hotkey).font(theme::reg(14.0)).color(p.text));
                ui.add_space(-6.0);
                theme::hint(ui, t.scroll_hotkey_hint, p);
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if theme::button(ui, t.record, Btn::Soft, p).clicked() {
                    self.recording_scroll = true;
                    self.recording = false;
                    self.scroll_err = None;
                }
                ui.add_space(4.0);
                let label = if self.recording_scroll {
                    "\u{2026}".to_string()
                } else if self.cfg.scroll_hotkey.is_empty() {
                    t.scroll_none.to_string()
                } else {
                    self.cfg.scroll_hotkey.clone()
                };
                key_chip(ui, &label, self.recording_scroll, p);
            });
        });
        if self.recording_scroll {
            ui.label(RichText::new(t.recording).font(theme::reg(12.5)).color(p.accent));
        } else if let Some(e) = self.scroll_err {
            let msg = if e == HkErr::Taken { t.hk_taken } else { t.hk_unsupported };
            ui.label(RichText::new(msg).font(theme::reg(12.5)).color(Color32::from_rgb(0xe0, 0x4f, 0x4f)));
        }
        if !self.cfg.scroll_hotkey.is_empty() && theme::button(ui, t.scroll_clear, Btn::Link, p).clicked() {
            self.cfg.scroll_hotkey.clear();
            self.scroll_err = None;
            self.apply();
        }
        ui.add_space(6.0);
    }

    fn header(&self, ui: &mut Ui, p: &Pal) {
        let t = self.s();
        ui.horizontal(|ui| {
            ui.add(egui::Image::new((self.logo.id(), egui::vec2(36.0, 36.0))));
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
        #[cfg(unix)]
        if sys::is_wayland() {
            self.wayland_block(ui, p);
        } else {
            self.hotkey_rows(ui, p);
            self.scroll_row(ui, p);
        }
        #[cfg(windows)]
        {
            self.hotkey_rows(ui, p);
            self.scroll_row(ui, p);
        }
        self.format_rows(ui, p);
    }

    /// Wayland: apps cannot grab keys, so show the command to bind to a system shortcut.
    #[cfg(unix)]
    fn wayland_block(&mut self, ui: &mut Ui, p: &Pal) {
        let t = self.s();
        ui.label(RichText::new(t.wl_title).font(theme::reg(14.0)).color(p.text));
        ui.add_space(-4.0);
        theme::hint(ui, t.wl_text, p);
        if self.resident && self.status.as_ref().is_some_and(|st| st.ok && st.hotkey == self.cfg.hotkey) {
            ui.add_space(2.0);
            ui.label(RichText::new(t.hk_ok).font(theme::reg(12.5)).color(Color32::from_rgb(0x2f, 0xa8, 0x5f)));
        }
        ui.add_space(4.0);
        let cmd = sys::capture_command();
        ui.horizontal(|ui| {
            Frame::new().fill(p.bg).stroke(egui::Stroke::new(1.0, p.line)).corner_radius(9.0).inner_margin(Margin::symmetric(10, 7)).show(ui, |ui| {
                ui.set_width((ui.available_width() - 150.0).max(80.0));
                ui.add(egui::Label::new(RichText::new(&cmd).font(egui::FontId::monospace(12.5)).color(p.text)).selectable(true).wrap());
            });
            let copied = self.copied.is_some_and(|c| c.elapsed() < Duration::from_secs(2));
            let label = if copied { t.wl_copied } else { t.wl_copy };
            if theme::button(ui, label, Btn::Soft, p).clicked() {
                ui.ctx().copy_text(cmd.clone());
                self.copied = Some(Instant::now());
            }
        });
        ui.add_space(10.0);
    }

    fn hotkey_rows(&mut self, ui: &mut Ui, p: &Pal) {
        let t = self.s();
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
                    self.recording_scroll = false;
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
                    HkErr::Taken if hotkey_is_prtsc(&self.hk_err_for) => t.hk_prtsc_warn.to_string(),
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
        ui.add_space(4.0);
        ui.label(RichText::new(t.quick_pick).font(theme::reg(12.0)).color(p.muted));
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
            for (label, mods) in [("Print Screen", 0), ("Ctrl+Print Screen", MOD_CONTROL), ("Shift+Print Screen", MOD_SHIFT), ("Alt+Print Screen", MOD_ALT)] {
                if theme::button(ui, label, Btn::Ghost, p).clicked() {
                    self.recording = false;
                    self.set_hotkey(Hotkey { mods, vk: hotkey::VK_SNAPSHOT });
                }
            }
        });
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

    }

    fn format_rows(&mut self, ui: &mut Ui, p: &Pal) {
        let t = self.s();
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
        #[cfg(windows)]
        let (a_title, a_hint) = (t.autostart, t.autostart_hint);
        #[cfg(unix)]
        let (a_title, a_hint) = (t.autostart_lnx, t.autostart_hint_lnx);
        if switch_row(ui, p, a_title, a_hint, &mut auto) && auto != self.autostart {
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

    fn support_block(&mut self, ui: &mut Ui, p: &Pal) {
        theme::divider(ui, p);
        support::support_section(ui, self.lang);
        ui.add_space(4.0);
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
        if self.recording || self.recording_scroll {
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
                self.support_block(ui, &p);
                self.about_section(ui, &p);
            });
        });
    }
}

// в”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђ widgets в”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђ

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

// в”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђ hotkeys в”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђв”Ђ

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
#[cfg(windows)]
fn try_register(hk: &Hotkey) -> bool {
    unsafe {
        let ok = RegisterHotKey(None, TEST_HOTKEY_ID, HOT_KEY_MODIFIERS(hk.mods | 0x4000), hk.vk).is_ok();
        if ok {
            let _ = UnregisterHotKey(None, TEST_HOTKEY_ID);
        }
        ok
    }
}

#[cfg(windows)]
fn close_resident() {
    if let Some(h) = sys::resident_window() {
        unsafe {
            let _ = PostMessageW(Some(h), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
        std::thread::sleep(Duration::from_millis(400));
    }
    let _: Option<HWND> = None;
}

#[cfg(unix)]
fn try_register(hk: &Hotkey) -> bool {
    crate::resident::can_register(hk)
}

#[cfg(unix)]
fn close_resident() {
    if sys::send("quit") {
        std::thread::sleep(Duration::from_millis(400));
    }
}

/// One settings window per session: an exclusive lock on a file in the data folder.
#[cfg(unix)]
fn single_instance() -> bool {
    let dir = crate::config::data_dir();
    let _ = std::fs::create_dir_all(&dir);
    let Ok(f) = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join("settings.lock")) else { return true };
    if f.try_lock().is_ok() {
        std::mem::forget(f); // held until the process exits
        true
    } else {
        false
    }
}

/// The real app icon (assets/icon.png) for the header, pre-scaled so it stays crisp at 36 px.
fn load_logo(ctx: &egui::Context) -> egui::TextureHandle {
    let img = image::load_from_memory(include_bytes!("../assets/icon.png")).expect("icon.png");
    let img = img.resize_exact(144, 144, image::imageops::FilterType::Lanczos3).to_rgba8();
    let color = egui::ColorImage::from_rgba_unmultiplied([144, 144], img.as_raw());
    ctx.load_texture("snapper-logo", color, egui::TextureOptions::LINEAR)
}
