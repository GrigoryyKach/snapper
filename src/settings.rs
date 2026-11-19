//! Settings mode (`Snapper.exe --settings`): an egui window in the suite style. Changes apply
//! immediately: the config is saved and the resident process is signalled to reload it.

use crate::config::{Config, Format};
use crate::hotkey::{self, Hotkey, MOD_ALT, MOD_CONTROL, MOD_SHIFT};
use crate::i18n::{self, Lang};
use crate::nimvoro::{self, UpdateState, Updater};
use crate::strings::{self, S};
use crate::support;
use crate::sys;
use crate::theme::{self, Btn, Pal, Seg};
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

/// Snapper blue: the crop frame of the app icon. Light is the darker shade, so it reads as text on the page.
const ACCENT: theme::Accent = theme::Accent::new(0x35a7ff, 0x0b6bcb, 0x141415, 0xffffff);

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
        .with_inner_size([520.0, 900.0])
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
        theme::set_accent(ACCENT);
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
        let label = if self.recording_scroll {
            "\u{2026}".to_string()
        } else if self.cfg.scroll_hotkey.is_empty() {
            t.scroll_none.to_string()
        } else {
            self.cfg.scroll_hotkey.clone()
        };
        if hotkey_line(ui, p, t.scroll_hotkey, t.scroll_hotkey_hint, t.record, &label, self.recording_scroll) {
            self.recording_scroll = true;
            self.recording = false;
            self.scroll_err = None;
        }
        if self.recording_scroll {
            ui.label(RichText::new(t.recording).font(theme::reg(12.0)).color(p.accent));
        } else if let Some(e) = self.scroll_err {
            let msg = if e == HkErr::Taken { t.hk_taken } else { t.hk_unsupported };
            ui.label(RichText::new(msg).font(theme::reg(12.0)).color(p.negative));
        }
        if !self.cfg.scroll_hotkey.is_empty() && theme::button(ui, t.scroll_clear, Btn::Link, p).clicked() {
            self.cfg.scroll_hotkey.clear();
            self.scroll_err = None;
            self.apply();
        }
    }

    fn header(&self, ui: &mut Ui, p: &Pal) {
        let t = self.s();
        ui.horizontal(|ui| {
            theme::mark(ui, 24.0, icon_crop, p);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.label(RichText::new("Snapper").font(theme::medium(14.0)).color(p.text));
                ui.label(RichText::new(t.subtitle).font(theme::reg(12.0)).color(p.muted));
            });
        });
        ui.add_space(8.0);
    }

    fn update_banner(&mut self, ui: &mut Ui, p: &Pal) {
        let UpdateState::Available { version, .. } = self.updater.state() else { return };
        let u = nimvoro::strings(self.lang);
        theme::card(p).inner_margin(Margin::same(12)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.label(RichText::new(u.available.replace("{v}", &version)).font(theme::medium(14.0)).color(p.text));
                    theme::hint(ui, &u.you_have.replace("{v}", self.updater.current), p);
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if theme::button(ui, u.update_btn, Btn::Primary, p).clicked() {
                        close_resident();
                        if self.updater.apply() {
                            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    }
                });
            });
        });
        ui.add_space(8.0);
    }

    fn hotkey_section(&mut self, ui: &mut Ui, p: &Pal) {
        let t = self.s();
        theme::section(ui, t.sec_capture, p);
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
        ui.spacing_mut().item_spacing.y = 4.0;
        ui.label(RichText::new(t.wl_title).font(theme::reg(14.0)).color(p.text));
        theme::hint(ui, t.wl_text, p);
        if self.resident && self.status.as_ref().is_some_and(|st| st.ok && st.hotkey == self.cfg.hotkey) {
            status_line(ui, Some(p.positive), t.hk_ok, p.text2);
        }
        ui.add_space(4.0);
        let cmd = sys::capture_command();
        ui.horizontal(|ui| {
            theme::field_frame(p).inner_margin(Margin::symmetric(12, 8)).show(ui, |ui| {
                ui.set_width((ui.available_width() - 136.0).max(80.0));
                ui.add(egui::Label::new(RichText::new(&cmd).font(theme::mono(12.0)).color(p.text)).selectable(true).wrap());
            });
            let copied = self.copied.is_some_and(|c| c.elapsed() < Duration::from_secs(2));
            let label = if copied { t.wl_copied } else { t.wl_copy };
            if theme::button(ui, label, Btn::Secondary, p).clicked() {
                ui.ctx().copy_text(cmd.clone());
                self.copied = Some(Instant::now());
            }
        });
    }

    fn hotkey_rows(&mut self, ui: &mut Ui, p: &Pal) {
        let t = self.s();
        let label = if self.recording { "\u{2026}".to_string() } else { self.cfg.hotkey.clone() };
        if hotkey_line(ui, p, t.hotkey, t.hotkey_hint, t.record, &label, self.recording) {
            self.recording = true;
            self.recording_scroll = false;
            self.hk_err = None;
        }

        // status line
        if self.recording {
            ui.label(RichText::new(t.recording).font(theme::reg(12.0)).color(p.accent));
        } else if let Some(e) = self.hk_err {
            let msg = match e {
                HkErr::Taken if hotkey_is_prtsc(&self.hk_err_for) => t.hk_prtsc_warn,
                HkErr::Taken => t.hk_taken,
                HkErr::Unsupported => t.hk_unsupported,
            };
            status_line(ui, Some(p.negative), msg, p.negative);
        } else if !self.resident {
            status_line(ui, None, t.resident_off, p.muted);
        } else {
            match &self.status {
                Some(st) if st.hotkey == self.cfg.hotkey && st.ok => status_line(ui, Some(p.positive), t.hk_ok, p.text2),
                Some(st) if st.hotkey == self.cfg.hotkey => {
                    let mut msg = t.hk_taken;
                    if hotkey_is_prtsc(&self.cfg.hotkey) && sys::prtsc_snipping_enabled() {
                        msg = t.hk_prtsc_warn;
                    }
                    status_line(ui, Some(p.negative), msg, p.negative);
                }
                _ => status_line(ui, None, t.hk_pending, p.muted),
            }
        }
        theme::hint(ui, t.quick_pick, p);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
            for (label, mods) in [("Print Screen", 0), ("Ctrl+Print Screen", MOD_CONTROL), ("Shift+Print Screen", MOD_SHIFT), ("Alt+Print Screen", MOD_ALT)] {
                if theme::button(ui, label, Btn::Small, p).clicked() {
                    self.recording = false;
                    self.set_hotkey(Hotkey { mods, vk: hotkey::VK_SNAPSHOT });
                }
            }
        });
        if self.cfg.hotkey != "PrintScreen" && theme::button(ui, t.reset, Btn::Link, p).clicked() {
            self.cfg.hotkey = "PrintScreen".into();
            self.hk_err = None;
            self.status = None;
            self.apply();
        }
        theme::divider(ui, p);
    }

    fn format_rows(&mut self, ui: &mut Ui, p: &Pal) {
        let t = self.s();
        ui.horizontal(|ui| {
            ui.label(RichText::new(t.format).font(theme::reg(14.0)).color(p.text));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let items = [Seg { label: "PNG", icon: None }, Seg { label: "JPG", icon: None }];
                let selected = if self.cfg.format == Format::Png { 0 } else { 1 };
                if let Some(i) = theme::segmented(ui, &items, selected, p) {
                    self.cfg.format = if i == 0 { Format::Png } else { Format::Jpg };
                    self.apply();
                }
            });
        });
        let mut v = self.cfg.copy_on_save;
        if switch_row(ui, p, t.copy_on_save, t.copy_on_save_hint, &mut v) {
            self.cfg.copy_on_save = v;
            self.apply();
        }
    }

    fn saving_section(&mut self, ui: &mut Ui, p: &Pal) {
        let t = self.s();
        theme::divider(ui, p);
        theme::section(ui, t.sec_saving, p);
        ui.label(RichText::new(t.folder).font(theme::reg(14.0)).color(p.text));
        let folder = self.cfg.save_folder();
        ui.horizontal(|ui| {
            theme::field_frame(p).inner_margin(Margin::symmetric(12, 6)).show(ui, |ui| {
                ui.set_width((ui.available_width() - 168.0).max(80.0));
                ui.add(egui::Label::new(RichText::new(folder.display().to_string()).font(theme::mono(12.0)).color(p.text)).truncate());
            });
            if theme::button(ui, t.browse, Btn::Secondary, p).clicked() {
                if let Some(dir) = rfd::FileDialog::new().set_directory(&folder).pick_folder() {
                    self.cfg.save_dir = dir.display().to_string();
                    self.apply();
                }
            }
            if theme::button(ui, t.open, Btn::Quiet, p).clicked() {
                sys::open_folder(&folder);
            }
        });
    }

    fn system_section(&mut self, ui: &mut Ui, p: &Pal) {
        let t = self.s();
        let u = nimvoro::strings(self.lang);
        theme::divider(ui, p);
        theme::section(ui, t.sec_system, p);
        let mut auto = self.autostart;
        #[cfg(windows)]
        let (a_title, a_hint) = (t.autostart, t.autostart_hint);
        #[cfg(unix)]
        let (a_title, a_hint) = (t.autostart_lnx, t.autostart_hint_lnx);
        if switch_row(ui, p, a_title, a_hint, &mut auto) && auto != self.autostart && sys::set_autostart(auto) {
            self.autostart = auto;
            self.cfg.autostart = auto;
            self.apply();
        }
        let mut check = self.cfg.check_updates;
        if switch_row(ui, p, u.check_on_start, u.check_on_start_hint, &mut check) {
            self.cfg.check_updates = check;
            self.apply();
        }
        ui.horizontal(|ui| {
            theme::hint(ui, &self.updater.status_text(self.lang), p);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if !self.updater.is_checking() && theme::button(ui, u.check_now, Btn::Link, p).clicked() {
                    let c = ui.ctx().clone();
                    self.updater.check(move || c.request_repaint());
                }
            });
        });
    }

    fn interface_section(&mut self, ui: &mut Ui, p: &Pal) {
        let t = self.s();
        theme::divider(ui, p);
        theme::section(ui, t.sec_interface, p);
        let mut lang = self.lang;
        if i18n::language_row(ui, &mut lang, p) {
            self.lang = lang;
            self.cfg.language = Some(lang.code().to_string());
            self.apply();
        }
        let mut mode = self.cfg.theme;
        if i18n::theme_row(ui, &mut mode, self.lang, p) {
            self.cfg.theme = mode;
            theme::apply(ui.ctx(), mode);
            self.apply();
        }
    }

    fn support_block(&mut self, ui: &mut Ui, p: &Pal) {
        theme::divider(ui, p);
        support::support_section(ui, self.lang);
    }

    fn about_section(&mut self, ui: &mut Ui, p: &Pal) {
        let t = self.s();
        theme::divider(ui, p);
        theme::section(ui, t.sec_about, p);
        ui.horizontal(|ui| {
            ui.label(RichText::new(t.version).font(theme::reg(14.0)).color(p.text));
            theme::meta(ui, env!("CARGO_PKG_VERSION"), p);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if theme::button(ui, t.project_page, Btn::Link, p).clicked() {
                    sys::open_url(REPO_URL);
                }
            });
        });
        ui.add_space(8.0);
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
        egui::CentralPanel::default().frame(Frame::new().fill(p.bg).inner_margin(Margin::symmetric(24, 20))).show(ui, |ui| {
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

// ───────────────────────── widgets ─────────────────────────

/// The settings mark: four crop-mark corners, the same shape the capture frame wears.
fn icon_crop(painter: &egui::Painter, c: egui::Pos2, col: Color32) {
    let s = egui::Stroke::new(1.5, col);
    let (h, a) = (6.5, 4.0);
    for (sx, sy) in [(-1.0f32, -1.0f32), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
        let corner = c + egui::vec2(sx * h, sy * h);
        painter.line_segment([corner, corner - egui::vec2(sx * a, 0.0)], s);
        painter.line_segment([corner, corner - egui::vec2(0.0, sy * a)], s);
    }
}

/// A hotkey chip: mono text on a raised key, an accent outline while recording.
fn key_chip(ui: &mut Ui, text: &str, active: bool, p: &Pal) {
    let g = ui.painter().layout_no_wrap(text.to_string(), theme::mono(13.0), p.text);
    let w = (g.size().x + 24.0).max(40.0);
    let (r, _) = ui.allocate_exact_size(egui::vec2(w, 32.0), egui::Sense::hover());
    let stroke = egui::Stroke::new(1.0, if active { p.accent } else { p.line_hi });
    ui.painter().rect(r, 8.0, p.raised, stroke, egui::StrokeKind::Inside);
    ui.painter().galley(egui::pos2(r.center().x - g.size().x / 2.0, r.center().y - g.size().y / 2.0), g, p.text);
}

/// Title and hint on the left, the chip and the Change button on the right. True when Change was clicked.
fn hotkey_line(ui: &mut Ui, p: &Pal, title: &str, hint: &str, record: &str, chip: &str, active: bool) -> bool {
    let mut clicked = false;
    let w = ui.available_width();
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.set_width(w - 208.0);
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.label(RichText::new(title).font(theme::reg(14.0)).color(p.text));
            theme::hint(ui, hint, p);
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            clicked = theme::button(ui, record, Btn::Secondary, p).clicked();
            key_chip(ui, chip, active, p);
        });
    });
    clicked
}

/// One line of status: a small dot in a meaning colour (or none) and the text.
fn status_line(ui: &mut Ui, dot: Option<Color32>, text: &str, col: Color32) {
    ui.horizontal(|ui| {
        if let Some(d) = dot {
            let (r, _) = ui.allocate_exact_size(egui::vec2(8.0, 16.0), egui::Sense::hover());
            ui.painter().circle_filled(r.center(), 3.0, d);
        }
        ui.add(egui::Label::new(RichText::new(text).font(theme::reg(12.0)).color(col)).wrap());
    });
}

fn switch_row(ui: &mut Ui, p: &Pal, title: &str, hint: &str, value: &mut bool) -> bool {
    let mut changed = false;
    let w = ui.available_width();
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.set_width(w - 56.0);
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.label(RichText::new(title).font(theme::reg(14.0)).color(p.text));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accent_is_readable_in_both_themes() {
        let a = ACCENT;
        for (name, fill, on, pal) in [("dark", a.dark, a.on_dark, Pal::dark()), ("light", a.light, a.on_light, Pal::light())] {
            assert!(theme::contrast(on, fill) >= 4.5, "{name}: on_accent on accent");
            for (surface, c) in [("sidebar", pal.sidebar), ("bg", pal.bg), ("card", pal.card)] {
                assert!(theme::contrast(fill, c) >= 4.5, "{name}: accent text on {surface}");
            }
            // The text roles that settings draw, on the page and on the card (the support block).
            for (role, col) in [("text", pal.text), ("text2", pal.text2), ("muted", pal.muted)] {
                for (surface, c) in [("bg", pal.bg), ("card", pal.card)] {
                    assert!(theme::contrast(col, c) >= 4.5, "{name}: {role} on {surface}");
                }
            }
            // Error text sits on the page only.
            assert!(theme::contrast(pal.negative, pal.bg) >= 4.5, "{name}: negative on bg");
        }
    }
}
