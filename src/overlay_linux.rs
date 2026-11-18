//! The capture overlay on Linux: an egui window (`Snapper --overlay <dir>`) that shows the frozen
//! desktop dimmed, lets the user drag a selection and annotate it, and hands the result back to the
//! resident process through files in `<dir>`.
//!
//! - X11: one borderless, always-on-top window covering the whole virtual desktop, 1 image pixel = 1
//!   physical pixel (egui's scale factor is accounted for).
//! - Wayland: a fullscreen window on the output the compositor picks. The portal gives one picture of
//!   the whole desktop, so the picture is fitted into the window (`View`); with one monitor that is 1:1.
//!
//! The selection lives in picture pixels; everything on screen is in egui points. `View` converts.

use crate::ann::{arrow_points, norm_rect, Ann, Pf, STROKES, TEXT_SIZES};
use crate::capture::{decode_shot, encode_shot, Shot};
use crate::config::{format_hex_color, parse_hex_color, Config};
use crate::files::Image;
use crate::geom::*;
use crate::i18n::{self, Lang};
use crate::raster;
use crate::strings;
use ab_glyph::FontArc;
use eframe::egui::{self, pos2, vec2, Color32, CursorIcon, Event, FontId, Key, Painter, Pos2, Rect as ERect, Stroke, StrokeKind, TextureHandle, TextureOptions};
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Copy,
    Save,
    Cancel,
}

impl Action {
    pub fn name(self) -> &'static str {
        match self {
            Action::Copy => "copy",
            Action::Save => "save",
            Action::Cancel => "cancel",
        }
    }
    pub fn parse(s: &str) -> Action {
        match s {
            "copy" => Action::Copy,
            "save" => Action::Save,
            _ => Action::Cancel,
        }
    }
}

/// What the resident process reads after the overlay closes.
pub struct Done {
    pub action: Action,
    pub image: Option<Image>,
    pub color: u32,
    pub stroke: usize,
}

pub fn out_raw(dir: &Path) -> PathBuf {
    dir.join("out.raw")
}
pub fn out_json(dir: &Path) -> PathBuf {
    dir.join("out.json")
}

/// Reads the files the overlay process left in `dir`. A missing result means "cancelled".
pub fn read_result(dir: &Path) -> Done {
    let cancelled = Done { action: Action::Cancel, image: None, color: 0xff3b30, stroke: 1 };
    let Ok(text) = std::fs::read_to_string(out_json(dir)) else { return cancelled };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { return cancelled };
    let action = Action::parse(v.get("action").and_then(|a| a.as_str()).unwrap_or("cancel"));
    let color = v.get("color").and_then(|c| c.as_u64()).unwrap_or(0xff3b30) as u32;
    let stroke = (v.get("stroke").and_then(|c| c.as_u64()).unwrap_or(1) as usize).min(2);
    let image = if action == Action::Cancel {
        None
    } else {
        std::fs::read(out_raw(dir)).ok().and_then(|d| decode_shot(&d)).map(|s| Image { w: s.rect.w() as u32, h: s.rect.h() as u32, bgra: s.bgra })
    };
    Done { action, image, color, stroke }
}

// ───────────────────────── model ─────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tool {
    Move,
    Pen,
    Arrow,
    Rect,
    Fill,
    Text,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Btn {
    Tool(Tool),
    Color,
    Undo,
    Copy,
    Save,
    Close,
}

const PRESETS: [u32; 10] = [0xff3b30, 0xff9500, 0xffcc00, 0x34c759, 0x32d4e6, 0x0a84ff, 0xaf52de, 0xff2d92, 0xffffff, 0x000000];
const ACCENT: u32 = 0x4f6bff;
const BAR_BG: u32 = 0x1b1e2b;
const BAR_LINE: u32 = 0x3a4064;
const BTN_HOVER: u32 = 0x2b3050;
const ICON: u32 = 0xe4e8f7;
const ICON_OFF: u32 = 0x666c8c;

fn col(rgb: u32) -> Color32 {
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}
fn cola(rgb: u32, a: f32) -> Color32 {
    Color32::from_rgba_unmultiplied((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8, (a * 255.0) as u8)
}

#[derive(Clone, Copy, Debug)]
enum Op {
    Select { start: Pt },
    Move { start: Pt, orig: Rect },
    Resize { handle: Handle, orig: Rect },
    Draw,
}

struct Toolbar {
    rect: Rect,
    buttons: Vec<(Btn, Rect)>,
    sep_x: i32,
}

struct Popover {
    rect: Rect,
    swatches: Vec<Rect>,
    strokes: Vec<Rect>,
    hex: Rect,
}

struct TextEdit {
    /// Top-left corner in picture pixels.
    pos: Pf,
    text: String,
    /// Caret as a byte index into `text`.
    caret: usize,
}

/// Maps picture pixels to egui points: `screen = origin + px * k`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct View {
    pub ox: f32,
    pub oy: f32,
    pub k: f32,
}

impl View {
    /// The picture (`iw` x `ih` px) fitted into a window of `sw` x `sh` points, centred, never distorted.
    /// `ppp` is the window's scale factor: when the window is exactly the picture size in physical
    /// pixels, `k` is `1 / ppp`, i.e. one picture pixel per physical pixel.
    pub fn fit(sw: f32, sh: f32, iw: f32, ih: f32, ppp: f32) -> View {
        let native = 1.0 / ppp.max(0.1);
        let k = if (iw * native - sw).abs() <= 1.5 && (ih * native - sh).abs() <= 1.5 { native } else { (sw / iw).min(sh / ih) };
        View { ox: (sw - iw * k) / 2.0, oy: (sh - ih * k) / 2.0, k }
    }
    pub fn to_screen(&self, x: f32, y: f32) -> Pos2 {
        pos2(self.ox + x * self.k, self.oy + y * self.k)
    }
    pub fn to_image(&self, p: Pos2) -> Pt {
        pt(((p.x - self.ox) / self.k).floor() as i32, ((p.y - self.oy) / self.k).floor() as i32)
    }
    pub fn rect_to_screen(&self, r: &Rect) -> Rect {
        let a = self.to_screen(r.l as f32, r.t as f32);
        let b = self.to_screen(r.r as f32, r.b as f32);
        rect(a.x.round() as i32, a.y.round() as i32, b.x.round() as i32, b.y.round() as i32)
    }
}

const TILE: i32 = 2048;

struct App {
    lang: Lang,
    dir: PathBuf,
    shot: Shot,
    font: Option<FontArc>,
    tiles: Vec<(Rect, TextureHandle)>,
    view: View,
    vrect: Rect,
    monitors: Vec<Rect>,
    sel: Option<Rect>,
    op: Option<Op>,
    tool: Tool,
    color: u32,
    stroke: usize,
    anns: Vec<Ann>,
    redo: Vec<Ann>,
    cur: Option<Ann>,
    draw_start: Option<Pf>,
    text: Option<TextEdit>,
    tb: Option<Toolbar>,
    hover: Option<Btn>,
    popover: Option<Popover>,
    popover_open: bool,
    hex: String,
    hex_focus: bool,
    cursor: Pt,
    started: Instant,
    action: Option<Action>,
    placed: bool,
    want_pos: Option<(f32, f32, f32, f32)>,
}

/// Opens the overlay for the capture stored in `dir` and blocks until it closes.
pub fn run(dir: &Path) -> i32 {
    let Some(shot) = std::fs::read(dir.join("cap.raw")).ok().and_then(|d| decode_shot(&d)) else {
        crate::sys::log("overlay: no capture file");
        return 1;
    };
    let cfg = Config::load();
    let lang = i18n::resolve(cfg.language.as_deref());
    let (w, h) = (shot.rect.w() as f32, shot.rect.h() as f32);
    let single = shot.monitors.len() <= 1;
    let mut vp = egui::ViewportBuilder::default()
        .with_title("Snapper")
        .with_app_id("snapper-overlay")
        .with_decorations(false)
        .with_resizable(false)
        .with_always_on_top()
        .with_taskbar(false)
        .with_position([0.0, 0.0])
        .with_inner_size([w, h]);
    if crate::sys::is_wayland() || single {
        vp = vp.with_fullscreen(true);
    }
    let options = eframe::NativeOptions { viewport: vp, ..Default::default() };
    let color = parse_hex_color(&cfg.last_color).unwrap_or(0xff3b30);
    let stroke = (cfg.last_stroke as usize).min(2);
    let dir = dir.to_path_buf();
    let res = eframe::run_native(
        "Snapper",
        options,
        Box::new(move |cc| {
            Ok(Box::new(App::new(cc, shot, lang, dir, color, stroke)))
        }),
    );
    if res.is_err() {
        1
    } else {
        0
    }
}

fn default_font() -> Option<FontArc> {
    let defs = egui::FontDefinitions::default();
    let d = defs.font_data.get("Ubuntu-Light")?;
    let bytes: &[u8] = (*d.font).as_ref();
    FontArc::try_from_vec(bytes.to_vec()).ok()
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, shot: Shot, lang: Lang, dir: PathBuf, color: u32, stroke: usize) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let vrect = shot.rect;
        let monitors = shot.monitors.clone();
        App {
            lang,
            dir,
            font: default_font(),
            tiles: Vec::new(),
            view: View { ox: 0.0, oy: 0.0, k: 1.0 },
            vrect,
            monitors,
            sel: None,
            op: None,
            tool: Tool::Move,
            color,
            stroke,
            anns: Vec::new(),
            redo: Vec::new(),
            cur: None,
            draw_start: None,
            text: None,
            tb: None,
            hover: None,
            popover: None,
            popover_open: false,
            hex: format_hex_color(color),
            hex_focus: false,
            cursor: pt(0, 0),
            started: Instant::now(),
            action: None,
            placed: false,
            shot,
            want_pos: None,
        }
    }

    fn make_tiles(&mut self, ctx: &egui::Context) {
        if !self.tiles.is_empty() {
            return;
        }
        let opts = if (self.view.k - 1.0).abs() < 1e-3 { TextureOptions::NEAREST } else { TextureOptions::LINEAR };
        let (w, h) = (self.shot.w() as i32, self.shot.h() as i32);
        let mut y = 0;
        while y < h {
            let th = TILE.min(h - y);
            let mut x = 0;
            while x < w {
                let tw = TILE.min(w - x);
                let mut rgba = Vec::with_capacity((tw * th * 4) as usize);
                for row in y..y + th {
                    let s = ((row * w + x) * 4) as usize;
                    for px in self.shot.bgra[s..s + (tw * 4) as usize].chunks_exact(4) {
                        rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
                    }
                }
                let img = egui::ColorImage::from_rgba_unmultiplied([tw as usize, th as usize], &rgba);
                let tex = ctx.load_texture(format!("shot-{x}-{y}"), img, opts);
                self.tiles.push((rect(x, y, x + tw, y + th), tex));
                x += tw;
            }
            y += th;
        }
    }

    // ───────────────────────── geometry ─────────────────────────

    fn sel_screen(&self) -> Option<Rect> {
        self.sel.map(|s| self.view.rect_to_screen(&s))
    }

    fn monitor_places(&self) -> Vec<Place> {
        let size = toolbar_size();
        self.monitors.iter().map(|m| Place { rect: self.view.rect_to_screen(m), toolbar: size }).collect()
    }

    fn layout(&mut self) {
        let Some(sel) = self.sel_screen() else {
            self.tb = None;
            self.popover = None;
            return;
        };
        if matches!(self.op, Some(Op::Select { .. })) {
            self.tb = None;
            self.popover = None;
            return;
        }
        let places = self.monitor_places();
        let places = if places.is_empty() { vec![Place { rect: self.view.rect_to_screen(&self.vrect), toolbar: toolbar_size() }] } else { places };
        if let Some((r, mon)) = place_toolbar(&sel, &places, 8) {
            let tb = build_toolbar(r);
            self.popover = if self.popover_open { Some(build_popover(&tb, places[mon].rect)) } else { None };
            self.tb = Some(tb);
        }
    }

    fn hit_toolbar(&self, p: Pt) -> Option<Btn> {
        self.tb.as_ref()?.buttons.iter().find(|(_, r)| r.contains(p)).map(|(b, _)| *b)
    }

    fn over_ui(&self, p: Pt) -> bool {
        self.tb.as_ref().is_some_and(|t| t.rect.contains(p)) || self.popover.as_ref().is_some_and(|q| q.rect.contains(p))
    }

    fn clamp_to(&self, p: Pt, r: &Rect) -> Pt {
        pt(p.x.clamp(r.l, r.r - 1), p.y.clamp(r.t, r.b - 1))
    }

    // ───────────────────────── input ─────────────────────────

    fn on_down(&mut self, pos: Pos2) {
        let ps = pt(pos.x.round() as i32, pos.y.round() as i32); // screen points
        let ip = self.view.to_image(pos);
        self.cursor = ps;
        if let Some(pop) = &self.popover {
            if pop.rect.contains(ps) {
                let hit = pop.swatches.iter().position(|r| r.contains(ps));
                let stroke = pop.strokes.iter().position(|r| r.contains(ps));
                let hex = pop.hex.contains(ps);
                if let Some(i) = hit {
                    self.color = PRESETS[i];
                    self.hex = format_hex_color(self.color);
                    self.hex_focus = false;
                } else if let Some(i) = stroke {
                    self.stroke = i;
                } else if hex {
                    self.hex_focus = true;
                }
                return;
            }
            self.popover_open = false;
            self.hex_focus = false;
        }
        if let Some(b) = self.hit_toolbar(ps) {
            self.press(b);
            return;
        }
        if self.tb.as_ref().is_some_and(|t| t.rect.contains(ps)) {
            return;
        }
        let sel_s = self.sel_screen();
        if self.text.is_some() && !(self.tool == Tool::Text && sel_s.is_some_and(|s| s.contains(ps))) {
            self.commit_text();
        }
        let Some(sel) = self.sel else {
            let p = self.clamp_to(ip, &self.vrect);
            self.op = Some(Op::Select { start: p });
            self.sel = Some(rect(p.x, p.y, p.x + 1, p.y + 1));
            return;
        };
        let sel_s = sel_s.unwrap();
        if let Some(h) = hit_handle(&sel_s, ps, 7) {
            self.op = Some(Op::Resize { handle: h, orig: sel });
            return;
        }
        if !sel_s.contains(ps) {
            return;
        }
        match self.tool {
            Tool::Move => self.op = Some(Op::Move { start: ip, orig: sel }),
            Tool::Text => {
                self.commit_text();
                self.text = Some(TextEdit { pos: (ip.x as f32, ip.y as f32), text: String::new(), caret: 0 });
            }
            t => {
                let q = (ip.x as f32 + 0.5, ip.y as f32 + 0.5);
                let w = STROKES[self.stroke];
                let c = self.color;
                self.cur = Some(match t {
                    Tool::Pen => Ann::Pen { pts: vec![q], color: c, w },
                    Tool::Arrow => Ann::Arrow { a: q, b: q, color: c, w },
                    Tool::Rect => Ann::Rect { r: (q.0, q.1, q.0, q.1), color: c, w },
                    _ => Ann::Fill { r: (q.0, q.1, q.0, q.1), color: c },
                });
                self.op = Some(Op::Draw);
                self.draw_start = Some(q);
            }
        }
    }

    fn on_move(&mut self, pos: Pos2) {
        let ps = pt(pos.x.round() as i32, pos.y.round() as i32);
        let ip = self.view.to_image(pos);
        self.cursor = ps;
        let Some(op) = self.op else {
            self.hover = self.hit_toolbar(ps);
            return;
        };
        match op {
            Op::Select { start } => {
                let q = self.clamp_to(ip, &self.vrect);
                self.sel = Some(rect(start.x.min(q.x), start.y.min(q.y), start.x.max(q.x) + 1, start.y.max(q.y) + 1));
            }
            Op::Move { start, orig } => self.sel = Some(move_rect(&orig, ip.x - start.x, ip.y - start.y, &self.vrect)),
            Op::Resize { handle, orig } => self.sel = Some(resize(&orig, handle, ip, 2, &self.vrect)),
            Op::Draw => {
                let Some(sel) = self.sel else { return };
                let q = self.clamp_to(ip, &sel);
                let q = (q.x as f32 + 0.5, q.y as f32 + 0.5);
                let start = self.draw_start.unwrap_or(q);
                match &mut self.cur {
                    Some(Ann::Pen { pts, .. }) => {
                        let last = *pts.last().unwrap();
                        if (q.0 - last.0).powi(2) + (q.1 - last.1).powi(2) >= 2.25 {
                            pts.push(q);
                        }
                    }
                    Some(Ann::Arrow { b, .. }) => *b = q,
                    Some(Ann::Rect { r, .. }) | Some(Ann::Fill { r, .. }) => *r = norm_rect(start, q),
                    _ => {}
                }
            }
        }
    }

    fn on_up(&mut self, pos: Pos2) {
        let Some(op) = self.op else { return };
        self.on_move(pos);
        self.op = None;
        match op {
            Op::Select { .. } => {
                if self.sel.is_some_and(|r| r.w() < 2 || r.h() < 2) {
                    self.sel = None;
                } else {
                    self.tool = Tool::Move;
                }
            }
            Op::Draw => {
                self.draw_start = None;
                if let Some(a) = self.cur.take() {
                    let ok = match &a {
                        Ann::Arrow { a, b, .. } => (a.0 - b.0).abs() + (a.1 - b.1).abs() >= 3.0,
                        Ann::Rect { r, .. } | Ann::Fill { r, .. } => r.2 - r.0 >= 1.0 || r.3 - r.1 >= 1.0,
                        _ => true,
                    };
                    if ok {
                        self.anns.push(a);
                        self.redo.clear();
                    }
                }
            }
            _ => {}
        }
    }

    fn press(&mut self, b: Btn) {
        match b {
            Btn::Tool(t) => {
                self.commit_text();
                self.tool = t;
                self.popover_open = false;
            }
            Btn::Color => {
                self.commit_text();
                self.popover_open = !self.popover_open;
                self.hex_focus = false;
            }
            Btn::Undo => self.undo(),
            Btn::Copy => self.action = Some(Action::Copy),
            Btn::Save => self.action = Some(Action::Save),
            Btn::Close => self.action = Some(Action::Cancel),
        }
    }

    fn undo(&mut self) {
        if let Some(a) = self.anns.pop() {
            self.redo.push(a);
        }
    }

    fn redo_one(&mut self) {
        if let Some(a) = self.redo.pop() {
            self.anns.push(a);
        }
    }

    fn commit_text(&mut self) {
        if let Some(t) = self.text.take() {
            if !t.text.trim().is_empty() {
                self.anns.push(Ann::Text { pos: t.pos, text: t.text, color: self.color, size: TEXT_SIZES[self.stroke] });
                self.redo.clear();
            }
        }
    }

    fn on_key(&mut self, key: Key, ctrl: bool, shift: bool) {
        let editing = self.text.is_some();
        match key {
            Key::Escape => {
                if editing {
                    self.text = None;
                } else if self.hex_focus {
                    self.hex_focus = false;
                } else {
                    self.action = Some(Action::Cancel);
                }
            }
            Key::Enter => {
                if editing {
                    if shift {
                        self.insert_text("\n");
                    } else {
                        self.commit_text();
                    }
                } else if self.hex_focus {
                    self.hex_focus = false;
                } else if self.sel.is_some() && self.op.is_none() {
                    self.action = Some(Action::Copy);
                }
            }
            Key::Backspace => {
                if editing {
                    self.text_backspace();
                } else if self.hex_focus {
                    self.hex.pop();
                    self.apply_hex();
                }
            }
            Key::Delete if editing => self.text_delete(),
            Key::ArrowLeft if editing => self.text_move(-1),
            Key::ArrowRight if editing => self.text_move(1),
            Key::Home if editing => self.text_move(-2),
            Key::End if editing => self.text_move(2),
            Key::C if ctrl && self.sel.is_some() && !editing => self.action = Some(Action::Copy),
            Key::S if ctrl && self.sel.is_some() => self.action = Some(Action::Save),
            Key::Z if ctrl && !editing => self.undo(),
            Key::Y if ctrl && !editing => self.redo_one(),
            _ => {}
        }
    }

    fn on_text(&mut self, s: &str) {
        let clean: String = s.chars().filter(|c| !c.is_control()).collect();
        if clean.is_empty() {
            return;
        }
        if self.text.is_some() {
            self.insert_text(&clean);
        } else if self.hex_focus {
            for c in clean.chars() {
                if (c.is_ascii_hexdigit() || c == '#') && self.hex.chars().count() < 7 {
                    self.hex.push(c);
                }
            }
            self.apply_hex();
        }
    }

    fn on_paste(&mut self, s: &str) {
        if self.text.is_some() {
            let one: String = s.chars().filter(|c| !c.is_control() || *c == '\n').collect();
            self.insert_text(&one);
        } else if self.hex_focus {
            self.hex = s.trim().chars().filter(|c| c.is_ascii_hexdigit() || *c == '#').take(7).collect();
            self.apply_hex();
        }
    }

    fn apply_hex(&mut self) {
        if let Some(c) = parse_hex_color(&self.hex) {
            self.color = c;
        }
    }

    fn insert_text(&mut self, s: &str) {
        if let Some(t) = &mut self.text {
            t.text.insert_str(t.caret, s);
            t.caret += s.len();
        }
    }

    fn text_backspace(&mut self) {
        if let Some(t) = &mut self.text {
            if let Some((i, _)) = t.text[..t.caret].char_indices().next_back() {
                t.text.replace_range(i..t.caret, "");
                t.caret = i;
            }
        }
    }

    fn text_delete(&mut self) {
        if let Some(t) = &mut self.text {
            if let Some(c) = t.text[t.caret..].chars().next() {
                t.text.replace_range(t.caret..t.caret + c.len_utf8(), "");
            }
        }
    }

    /// -1 / 1: one character; -2 / 2: start / end of the line.
    fn text_move(&mut self, d: i32) {
        if let Some(t) = &mut self.text {
            match d {
                -1 => {
                    if let Some((i, _)) = t.text[..t.caret].char_indices().next_back() {
                        t.caret = i;
                    }
                }
                1 => {
                    if let Some(c) = t.text[t.caret..].chars().next() {
                        t.caret += c.len_utf8();
                    }
                }
                -2 => t.caret = t.text[..t.caret].rfind('\n').map_or(0, |i| i + 1),
                _ => t.caret += t.text[t.caret..].find('\n').unwrap_or(t.text.len() - t.caret),
            }
        }
    }

    fn cursor_icon(&self, p: Pt) -> CursorIcon {
        if let Some(op) = self.op {
            return match op {
                Op::Select { .. } | Op::Draw => CursorIcon::Crosshair,
                Op::Move { .. } => CursorIcon::Move,
                Op::Resize { handle, .. } => resize_cursor(handle),
            };
        }
        if self.over_ui(p) {
            return if self.hit_toolbar(p).is_some() { CursorIcon::PointingHand } else { CursorIcon::Default };
        }
        let Some(sel) = self.sel_screen() else { return CursorIcon::Crosshair };
        if let Some(h) = hit_handle(&sel, p, 7) {
            return resize_cursor(h);
        }
        if sel.contains(p) {
            return match self.tool {
                Tool::Move => CursorIcon::Move,
                Tool::Text => CursorIcon::Text,
                _ => CursorIcon::Crosshair,
            };
        }
        CursorIcon::Default
    }

    // ───────────────────────── output ─────────────────────────

    fn finish(&mut self, ctx: &egui::Context, action: Action) {
        self.commit_text();
        let mut image: Option<Image> = None;
        if action != Action::Cancel {
            if let Some(sel) = self.sel {
                image = raster::render_output(&self.shot.bgra, self.shot.w(), &sel, &self.anns, self.font.as_ref()).ok();
            }
        }
        if let Some(img) = &image {
            let s = Shot { rect: rect(0, 0, img.w as i32, img.h as i32), monitors: vec![], bgra: img.bgra.clone() };
            let _ = std::fs::write(out_raw(&self.dir), encode_shot(&s));
        }
        let v = serde_json::json!({ "action": if image.is_some() { action.name() } else { "cancel" }, "color": self.color, "stroke": self.stroke });
        let _ = std::fs::write(out_json(&self.dir), v.to_string());
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    // ───────────────────────── painting ─────────────────────────

    fn pt_e(&self, p: Pf) -> Pos2 {
        self.view.to_screen(p.0, p.1)
    }

    fn draw_region(&self, painter: &Painter, region: &Rect) {
        for (tr, tex) in &self.tiles {
            let Some(ir) = tr.intersect(region) else { continue };
            let dst = ERect::from_min_max(self.view.to_screen(ir.l as f32, ir.t as f32), self.view.to_screen(ir.r as f32, ir.b as f32));
            let uv = ERect::from_min_max(
                pos2((ir.l - tr.l) as f32 / tr.w() as f32, (ir.t - tr.t) as f32 / tr.h() as f32),
                pos2((ir.r - tr.l) as f32 / tr.w() as f32, (ir.b - tr.t) as f32 / tr.h() as f32),
            );
            painter.image(tex.id(), dst, uv, Color32::WHITE);
        }
    }

    fn draw_ann(&self, painter: &Painter, a: &Ann) {
        let k = self.view.k;
        match a {
            Ann::Pen { pts, color, w } => {
                let c = col(*color);
                let pp: Vec<Pos2> = pts.iter().map(|p| self.pt_e(*p)).collect();
                if pp.len() >= 2 {
                    painter.add(egui::Shape::line(pp.clone(), Stroke::new(w * k, c)));
                }
                for p in &pp {
                    painter.circle_filled(*p, w * k / 2.0, c);
                }
            }
            Ann::Arrow { a: pa, b: pb, color, w } => {
                if let Some((shaft, [tip, l, r])) = arrow_points(*pa, *pb, *w) {
                    let c = col(*color);
                    let (p0, p1) = (self.pt_e(*pa), self.pt_e(shaft));
                    painter.line_segment([p0, p1], Stroke::new(w * k, c));
                    painter.circle_filled(p0, w * k / 2.0, c);
                    painter.add(egui::Shape::convex_polygon(vec![self.pt_e(tip), self.pt_e(l), self.pt_e(r)], c, Stroke::new((w * 0.35).max(1.0) * k, c)));
                }
            }
            Ann::Rect { r, color, w } => {
                let rr = ERect::from_min_max(self.pt_e((r.0, r.1)), self.pt_e((r.2, r.3)));
                painter.rect_stroke(rr, 0.0, Stroke::new(w * k, col(*color)), StrokeKind::Middle);
            }
            Ann::Fill { r, color } => {
                let rr = ERect::from_min_max(self.pt_e((r.0, r.1)), self.pt_e((r.2, r.3)));
                painter.rect_filled(rr, 0.0, col(*color));
            }
            Ann::Text { pos, text, color, size } => {
                let g = painter.layout_no_wrap(text.clone(), FontId::proportional(size * k), col(*color));
                painter.galley(self.pt_e(*pos), g, col(*color));
            }
        }
    }

    fn paint(&self, ui: &egui::Ui) {
        let painter = ui.painter();
        let full = ui.max_rect();
        painter.rect_filled(full, 0.0, Color32::BLACK);
        let all = self.vrect;
        self.draw_region(painter, &all);
        let pic = ERect::from_min_max(self.view.to_screen(0.0, 0.0), self.view.to_screen(all.r as f32, all.b as f32));
        painter.rect_filled(pic, 0.0, cola(0x000000, 0.45));

        let Some(sel) = self.sel else {
            self.draw_hint(painter, pic);
            return;
        };
        self.draw_region(painter, &sel);
        let ss = self.view.rect_to_screen(&sel);
        let sr = ERect::from_min_max(pos2(ss.l as f32, ss.t as f32), pos2(ss.r as f32, ss.b as f32));
        {
            let clip = painter.with_clip_rect(sr);
            for a in &self.anns {
                self.draw_ann(&clip, a);
            }
            if let Some(a) = &self.cur {
                self.draw_ann(&clip, a);
            }
            if let Some(t) = &self.text {
                self.draw_text_edit(&clip, t);
            }
        }
        painter.rect_stroke(sr.expand(0.5), 0.0, Stroke::new(1.0, col(ACCENT)), StrokeKind::Middle);
        if !matches!(self.op, Some(Op::Select { .. })) {
            for h in Handle::ALL {
                let c = handle_point(&ss, h);
                let r = ERect::from_center_size(pos2(c.x as f32, c.y as f32), vec2(8.0, 8.0));
                painter.rect_filled(r, 0.0, Color32::WHITE);
                painter.rect_stroke(r, 0.0, Stroke::new(1.0, col(ACCENT)), StrokeKind::Inside);
            }
        }
        self.draw_label(painter, &sel, &ss, pic);
        if let Some(tb) = &self.tb {
            self.draw_toolbar(painter, tb);
            if let Some(p) = &self.popover {
                self.draw_popover(painter, p);
            } else if let Some(h) = self.hover {
                self.draw_tooltip(painter, tb, h);
            }
        }
    }

    fn draw_hint(&self, painter: &Painter, pic: ERect) {
        let t = strings::tr(self.lang);
        let text = format!("{}   -   {}", t.t_close, t.t_copy);
        let g = painter.layout_no_wrap(text, FontId::proportional(14.0), cola(0xffffff, 0.85));
        let pos = pos2(pic.center().x - g.size().x / 2.0, pic.top() + 24.0);
        let bg = ERect::from_min_size(pos - vec2(10.0, 6.0), g.size() + vec2(20.0, 12.0));
        painter.rect_filled(bg, 8.0, cola(0x12141f, 0.85));
        painter.galley(pos, g, Color32::WHITE);
    }

    fn draw_text_edit(&self, painter: &Painter, t: &TextEdit) {
        let size = TEXT_SIZES[self.stroke];
        let k = self.view.k;
        let font = FontId::proportional(size * k);
        let g = painter.layout_no_wrap(t.text.clone(), font.clone(), col(self.color));
        let origin = self.pt_e(t.pos);
        painter.galley(origin, g.clone(), col(self.color));
        let row = painter.layout_no_wrap("Ag".into(), font.clone(), Color32::WHITE).size().y;
        let box_r = ERect::from_min_max(origin - vec2(4.0, 2.0), origin + vec2(g.size().x.max(8.0) + 6.0, g.size().y.max(row) + 2.0));
        painter.rect_stroke(box_r, 0.0, Stroke::new(1.0, cola(ACCENT, 0.8)), StrokeKind::Middle);
        let before = &t.text[..t.caret];
        let line_no = before.matches('\n').count();
        let last = before.rsplit('\n').next().unwrap_or("");
        let cx = painter.layout_no_wrap(last.to_string(), font, Color32::WHITE).size().x;
        let cy = origin.y + line_no as f32 * row;
        let on = (self.started.elapsed().as_millis() / 500) % 2 == 0;
        if on {
            let c = if self.color == 0 { Color32::WHITE } else { col(self.color) };
            painter.line_segment([pos2(origin.x + cx, cy), pos2(origin.x + cx, cy + row)], Stroke::new(2.0, c));
        }
    }

    fn draw_label(&self, painter: &Painter, sel: &Rect, ss: &Rect, pic: ERect) {
        let text = format!("{} \u{00d7} {}", sel.w(), sel.h());
        let g = painter.layout_no_wrap(text, FontId::proportional(12.5), Color32::WHITE);
        let (pw, ph) = (7.0, 4.0);
        let (w, h) = (g.size().x + pw * 2.0, g.size().y + ph * 2.0);
        let (x, y) = if matches!(self.op, Some(Op::Select { .. })) {
            ((self.cursor.x + 16) as f32, (self.cursor.y + 20) as f32)
        } else {
            let y = ss.t as f32 - h - 6.0;
            (ss.l as f32, if y < pic.top() { ss.t as f32 + 6.0 } else { y })
        };
        let x = x.clamp(pic.left(), (pic.right() - w).max(pic.left()));
        let y = y.clamp(pic.top(), (pic.bottom() - h).max(pic.top()));
        let r = ERect::from_min_size(pos2(x, y), vec2(w, h));
        painter.rect_filled(r, 6.0, cola(0x12141f, 0.88));
        painter.galley(pos2(x + pw, y + ph), g, Color32::WHITE);
    }

    fn draw_toolbar(&self, painter: &Painter, tb: &Toolbar) {
        let r = e_rect(&tb.rect);
        painter.rect_filled(r, r.height() / 2.0, cola(BAR_BG, 0.97));
        painter.rect_stroke(r, r.height() / 2.0, Stroke::new(1.0, col(BAR_LINE)), StrokeKind::Inside);
        painter.line_segment(
            [pos2(tb.sep_x as f32 + 0.5, r.top() + 12.0), pos2(tb.sep_x as f32 + 0.5, r.bottom() - 12.0)],
            Stroke::new(1.0, col(BAR_LINE)),
        );
        for (btn, br) in &tb.buttons {
            let active = *btn == Btn::Tool(self.tool);
            let hover = self.hover == Some(*btn);
            let er = e_rect(br);
            if active || hover {
                painter.rect_filled(er, 9.0, col(if active { ACCENT } else { BTN_HOVER }));
            }
            let disabled = *btn == Btn::Undo && self.anns.is_empty();
            let c = if disabled { ICON_OFF } else if active { 0xffffff } else { ICON };
            self.draw_icon(painter, *btn, er, c);
        }
    }

    fn draw_icon(&self, painter: &Painter, btn: Btn, r: ERect, c: u32) {
        let k = 22.0 / 24.0;
        let o = r.center() - vec2(11.0, 11.0);
        let p = |x: f32, y: f32| pos2(o.x + x * k, o.y + y * k);
        let lw = 1.8;
        let stroke = Stroke::new(lw, col(c));
        let line = |a: (f32, f32), b: (f32, f32), w: f32| {
            painter.line_segment([p(a.0, a.1), p(b.0, b.1)], Stroke::new(w, col(c)));
            painter.circle_filled(p(a.0, a.1), w / 2.0, col(c));
            painter.circle_filled(p(b.0, b.1), w / 2.0, col(c));
        };
        let wings = |tip: (f32, f32), from: (f32, f32), len: f32| {
            let (dx, dy) = (tip.0 - from.0, tip.1 - from.1);
            let l = (dx * dx + dy * dy).sqrt().max(0.001);
            let (ux, uy) = (dx / l, dy / l);
            for sign in [-1.0f32, 1.0] {
                let ang = 0.62f32 * sign;
                let (cs, sn) = (ang.cos(), ang.sin());
                let (vx, vy) = (-(ux * cs - uy * sn), -(ux * sn + uy * cs));
                line(tip, (tip.0 + vx * len, tip.1 + vy * len), lw);
            }
        };
        match btn {
            Btn::Tool(Tool::Move) => {
                line((12.0, 3.0), (12.0, 21.0), lw);
                line((3.0, 12.0), (21.0, 12.0), lw);
                wings((12.0, 3.0), (12.0, 9.0), 4.0);
                wings((12.0, 21.0), (12.0, 15.0), 4.0);
                wings((3.0, 12.0), (9.0, 12.0), 4.0);
                wings((21.0, 12.0), (15.0, 12.0), 4.0);
            }
            Btn::Tool(Tool::Pen) => {
                line((8.0, 16.0), (17.5, 6.5), 3.6);
                painter.add(egui::Shape::convex_polygon(vec![p(4.0, 20.0), p(5.6, 14.6), p(9.4, 18.4)], col(c), Stroke::NONE));
            }
            Btn::Tool(Tool::Arrow) => {
                line((5.0, 19.0), (16.0, 8.0), lw);
                painter.add(egui::Shape::convex_polygon(vec![p(19.5, 4.5), p(19.5, 12.0), p(12.0, 4.5)], col(c), Stroke::NONE));
            }
            Btn::Tool(Tool::Rect) => {
                painter.rect_stroke(ERect::from_min_max(p(4.5, 6.5), p(19.5, 17.5)), 0.0, stroke, StrokeKind::Middle);
            }
            Btn::Tool(Tool::Fill) => {
                painter.rect_filled(ERect::from_min_max(p(4.0, 6.0), p(20.0, 18.0)), 2.0, col(c));
            }
            Btn::Tool(Tool::Text) => {
                line((6.0, 6.5), (18.0, 6.5), lw * 1.15);
                line((12.0, 6.5), (12.0, 19.0), lw * 1.15);
                line((9.5, 19.0), (14.5, 19.0), lw * 1.15);
            }
            Btn::Color => {
                painter.circle_filled(p(12.0, 12.0), 8.0 * k, col(self.color));
                painter.circle_stroke(p(12.0, 12.0), 8.0 * k, Stroke::new(lw, Color32::WHITE));
            }
            Btn::Undo => {
                let mut prev = None;
                let mut first = None;
                for n in 0..=16 {
                    let th = (150.0f32 - n as f32 * 11.0).to_radians();
                    let q = (12.0 + 6.5 * th.cos(), 14.0 - 6.5 * th.sin());
                    if let Some(a) = prev {
                        line(a, q, lw);
                    } else {
                        first = Some(q);
                    }
                    prev = Some(q);
                }
                if let Some(f) = first {
                    wings(f, (f.0 + 1.6, f.1 - 3.0), 4.8);
                }
            }
            Btn::Copy => {
                painter.rect_stroke(ERect::from_min_max(p(8.5, 4.5), p(19.5, 15.5)), 2.0, stroke, StrokeKind::Middle);
                painter.rect_filled(ERect::from_min_max(p(4.5, 8.5), p(15.5, 19.5)), 2.0, col(BAR_BG));
                painter.rect_stroke(ERect::from_min_max(p(4.5, 8.5), p(15.5, 19.5)), 2.0, stroke, StrokeKind::Middle);
            }
            Btn::Save => {
                line((12.0, 4.0), (12.0, 14.5), lw);
                wings((12.0, 15.0), (12.0, 9.0), 4.6);
                line((5.0, 14.0), (5.0, 19.5), lw);
                line((5.0, 19.5), (19.0, 19.5), lw);
                line((19.0, 19.5), (19.0, 14.0), lw);
            }
            Btn::Close => {
                line((6.0, 6.0), (18.0, 18.0), lw);
                line((18.0, 6.0), (6.0, 18.0), lw);
            }
        }
    }

    fn draw_tooltip(&self, painter: &Painter, tb: &Toolbar, btn: Btn) {
        let t = strings::tr(self.lang);
        let text = match btn {
            Btn::Tool(Tool::Move) => t.t_move,
            Btn::Tool(Tool::Pen) => t.t_pencil,
            Btn::Tool(Tool::Arrow) => t.t_arrow,
            Btn::Tool(Tool::Rect) => t.t_rect,
            Btn::Tool(Tool::Fill) => t.t_fill,
            Btn::Tool(Tool::Text) => t.t_text,
            Btn::Color => t.t_color,
            Btn::Undo => t.t_undo,
            Btn::Copy => t.t_copy,
            Btn::Save => t.t_save,
            Btn::Close => t.t_close,
        };
        let Some((_, br)) = tb.buttons.iter().find(|(b, _)| *b == btn) else { return };
        let g = painter.layout_no_wrap(text.to_string(), FontId::proportional(12.5), col(0xf2f4ff));
        let (pw, ph) = (8.0, 5.0);
        let (w, h) = (g.size().x + pw * 2.0, g.size().y + ph * 2.0);
        let screen = painter.clip_rect();
        let x = (br.center().x as f32 - w / 2.0).clamp(screen.left(), (screen.right() - w).max(screen.left()));
        let below = tb.rect.b as f32 + 6.0;
        let y = if below + h <= screen.bottom() { below } else { tb.rect.t as f32 - 6.0 - h };
        let r = ERect::from_min_size(pos2(x, y), vec2(w, h));
        painter.rect_filled(r, 6.0, cola(0x12141f, 0.96));
        painter.rect_stroke(r, 6.0, Stroke::new(1.0, col(BAR_LINE)), StrokeKind::Inside);
        painter.galley(pos2(x + pw, y + ph), g, col(0xf2f4ff));
    }

    fn draw_popover(&self, painter: &Painter, p: &Popover) {
        let r = e_rect(&p.rect);
        painter.rect_filled(r, 14.0, cola(BAR_BG, 0.98));
        painter.rect_stroke(r, 14.0, Stroke::new(1.0, col(BAR_LINE)), StrokeKind::Inside);
        for (i, sw) in p.swatches.iter().enumerate() {
            let e = e_rect(sw);
            let rad = e.width() / 2.0 - 2.0;
            painter.circle_filled(e.center(), rad, col(PRESETS[i]));
            painter.circle_stroke(e.center(), rad, Stroke::new(1.0, col(0x59607f)));
            if PRESETS[i] == self.color {
                painter.circle_stroke(e.center(), rad + 3.0, Stroke::new(2.0, Color32::WHITE));
            }
        }
        for (i, sr) in p.strokes.iter().enumerate() {
            let e = e_rect(sr);
            painter.rect_filled(e, 8.0, col(if i == self.stroke { ACCENT } else { BTN_HOVER }));
            let cy = e.center().y;
            painter.line_segment([pos2(e.left() + 12.0, cy), pos2(e.right() - 12.0, cy)], Stroke::new(STROKES[i], Color32::WHITE));
        }
        let t = strings::tr(self.lang);
        let hr = e_rect(&p.hex);
        painter.rect_filled(hr, 8.0, col(0x12141f));
        painter.rect_stroke(hr, 8.0, Stroke::new(1.0, col(if self.hex_focus { ACCENT } else { BAR_LINE })), StrokeKind::Inside);
        let f = FontId::proportional(13.0);
        let lg = painter.layout_no_wrap(t.hex.to_string(), f.clone(), col(0x8e94b3));
        painter.galley(pos2(hr.left() + 10.0, hr.center().y - lg.size().y / 2.0), lg, col(0x8e94b3));
        let hg = painter.layout_no_wrap(self.hex.clone(), f, col(0xf2f4ff));
        let tx = hr.left() + 46.0;
        let ty = hr.center().y - hg.size().y / 2.0;
        let tw = hg.size().x;
        painter.galley(pos2(tx, ty), hg.clone(), col(0xf2f4ff));
        if self.hex_focus {
            painter.line_segment([pos2(tx + tw + 1.0, ty), pos2(tx + tw + 1.0, ty + hg.size().y)], Stroke::new(1.0, Color32::WHITE));
        }
        let dot = pos2(hr.right() - 16.0, hr.center().y);
        painter.circle_filled(dot, 7.0, col(self.color));
        painter.circle_stroke(dot, 7.0, Stroke::new(1.0, col(0x59607f)));
    }
}

fn resize_cursor(h: Handle) -> CursorIcon {
    match handle_cursor(h) {
        ResizeCursor::NwSe => CursorIcon::ResizeNwSe,
        ResizeCursor::NeSw => CursorIcon::ResizeNeSw,
        ResizeCursor::Ns => CursorIcon::ResizeVertical,
        ResizeCursor::We => CursorIcon::ResizeHorizontal,
    }
}

fn e_rect(r: &Rect) -> ERect {
    ERect::from_min_max(pos2(r.l as f32, r.t as f32), pos2(r.r as f32, r.b as f32))
}

// ───────────────────────── toolbar geometry (points) ─────────────────────────

const ITEMS: [Option<Btn>; 12] = [
    Some(Btn::Tool(Tool::Move)),
    Some(Btn::Tool(Tool::Pen)),
    Some(Btn::Tool(Tool::Arrow)),
    Some(Btn::Tool(Tool::Rect)),
    Some(Btn::Tool(Tool::Fill)),
    Some(Btn::Tool(Tool::Text)),
    Some(Btn::Color),
    None,
    Some(Btn::Undo),
    Some(Btn::Copy),
    Some(Btn::Save),
    Some(Btn::Close),
];

const PAD: i32 = 7;
const BTN: i32 = 34;
const GAP: i32 = 3;
const SEP: i32 = 12;

fn toolbar_size() -> (i32, i32) {
    let mut x = PAD;
    for it in ITEMS {
        x += if it.is_some() { BTN + GAP } else { SEP };
    }
    (x - GAP + PAD, BTN + 2 * PAD)
}

fn build_toolbar(r: Rect) -> Toolbar {
    let mut x = r.l + PAD;
    let mut buttons = Vec::new();
    let mut sep_x = 0;
    for it in ITEMS {
        match it {
            Some(b) => {
                buttons.push((b, rect(x, r.t + PAD, x + BTN, r.t + PAD + BTN)));
                x += BTN + GAP;
            }
            None => {
                sep_x = x + SEP / 2 - GAP / 2;
                x += SEP;
            }
        }
    }
    Toolbar { rect: r, buttons, sep_x }
}

fn build_popover(tb: &Toolbar, mon: Rect) -> Popover {
    let (pad, sw, g) = (12, 28, 8);
    let w = pad * 2 + 5 * sw + 4 * g;
    let (stroke_h, hex_h) = (30, 32);
    let h = pad + 2 * sw + g + 12 + stroke_h + 12 + hex_h + pad;
    let color_btn = tb.buttons.iter().find(|(b, _)| *b == Btn::Color).map(|(_, r)| *r).unwrap_or(tb.rect);
    let x = (color_btn.center().x - w / 2).clamp(mon.l, (mon.r - w).max(mon.l));
    let below = tb.rect.b + 8;
    let y = if below + h <= mon.b { below } else { tb.rect.t - 8 - h };
    let r = rect(x, y, x + w, y + h);
    let mut swatches = Vec::new();
    for i in 0..10 {
        let (cx, cy) = (i % 5, i / 5);
        let l = r.l + pad + cx * (sw + g);
        let t = r.t + pad + cy * (sw + g);
        swatches.push(rect(l, t, l + sw, t + sw));
    }
    let sy = r.t + pad + 2 * sw + g + 12;
    let inner = w - 2 * pad;
    let bw = (inner - 2 * g) / 3;
    let strokes = (0..3).map(|i| rect(r.l + pad + i * (bw + g), sy, r.l + pad + i * (bw + g) + bw, sy + stroke_h)).collect();
    let hy = sy + stroke_h + 12;
    let hex = rect(r.l + pad, hy, r.r - pad, hy + hex_h);
    Popover { rect: r, swatches, strokes, hex }
}

// ───────────────────────── the eframe app ─────────────────────────

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let screen = ctx.content_rect();
        let ppp = ctx.pixels_per_point();
        let (iw, ih) = (self.vrect.w() as f32, self.vrect.h() as f32);

        // X11: the window was created assuming a scale factor of 1; correct it once.
        if !self.placed {
            self.placed = true;
            if !crate::sys::is_wayland() && (ppp - 1.0).abs() > 0.01 && self.monitors.len() > 1 {
                self.want_pos = Some((0.0, 0.0, iw / ppp, ih / ppp));
            }
        }
        if let Some((x, y, w, h)) = self.want_pos.take() {
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(pos2(x, y)));
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(vec2(w, h)));
        }
        self.view = View::fit(screen.width(), screen.height(), iw, ih, ppp);
        self.make_tiles(&ctx);

        // ----- input -----
        let (events, mods) = ctx.input(|i| (i.events.clone(), i.modifiers));
        let mut copy_event = false;
        for ev in events {
            match ev {
                Event::PointerMoved(p) => self.on_move(p),
                Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, .. } => {
                    if pressed {
                        self.layout();
                        self.on_down(pos);
                    } else {
                        self.on_up(pos);
                    }
                }
                Event::PointerButton { button: egui::PointerButton::Secondary, pressed: true, .. } => {
                    if self.sel.is_none() {
                        self.action = Some(Action::Cancel);
                    }
                }
                Event::Key { key, pressed: true, modifiers, .. } => self.on_key(key, modifiers.ctrl || modifiers.command, modifiers.shift),
                Event::Text(s) => self.on_text(&s),
                Event::Paste(s) => self.on_paste(&s),
                Event::Ime(egui::ImeEvent::Commit(s)) => self.on_text(&s),
                Event::Copy => copy_event = true,
                _ => {}
            }
            // keep the toolbar hit boxes in step with the selection while the pointer moves
            self.layout();
        }
        if copy_event && self.text.is_none() && !self.hex_focus && self.sel.is_some() {
            self.action = Some(Action::Copy);
        }
        let _ = mods;
        self.layout();

        if let Some(a) = self.action.take() {
            self.finish(&ctx, a);
            return;
        }

        egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| {
            self.paint(ui);
        });
        let hover = ctx.input(|i| i.pointer.hover_pos());
        if let Some(h) = hover {
            ctx.set_cursor_icon(self.cursor_icon(pt(h.x.round() as i32, h.y.round() as i32)));
        }
        if self.text.is_some() || self.hex_focus {
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_is_one_to_one_at_native_size() {
        let v = View::fit(1920.0, 1080.0, 1920.0, 1080.0, 1.0);
        assert_eq!(v, View { ox: 0.0, oy: 0.0, k: 1.0 });
        // HiDPI X11 window: 3840x2160 px shown in 1920x1080 points
        let v = View::fit(1920.0, 1080.0, 3840.0, 2160.0, 2.0);
        assert!((v.k - 0.5).abs() < 1e-6 && v.ox == 0.0 && v.oy == 0.0);
    }

    #[test]
    fn view_fits_a_bigger_desktop_into_a_smaller_window() {
        // two monitors stitched by the portal, shown on one fullscreen output
        let v = View::fit(1920.0, 1080.0, 3840.0, 1080.0, 1.0);
        assert!((v.k - 0.5).abs() < 1e-6);
        assert!((v.oy - 270.0).abs() < 1e-3 && v.ox.abs() < 1e-3);
    }

    #[test]
    fn view_roundtrip() {
        let v = View { ox: 10.0, oy: 20.0, k: 0.5 };
        let p = v.to_screen(100.0, 200.0);
        assert_eq!(v.to_image(p), pt(100, 200));
        assert_eq!(v.rect_to_screen(&rect(0, 0, 100, 100)), rect(10, 20, 60, 70));
    }

    #[test]
    fn toolbar_fits_its_buttons() {
        let (w, h) = toolbar_size();
        let tb = build_toolbar(rect(100, 100, 100 + w, 100 + h));
        assert_eq!(tb.buttons.len(), 11);
        assert!(tb.buttons.iter().all(|(_, r)| tb.rect.contains_rect(r)));
        let pop = build_popover(&tb, rect(0, 0, 1920, 1080));
        assert_eq!((pop.swatches.len(), pop.strokes.len()), (10, 3));
        assert!(pop.rect.contains_rect(&pop.hex));
    }

    #[test]
    fn action_names_roundtrip() {
        for a in [Action::Copy, Action::Save, Action::Cancel] {
            assert_eq!(Action::parse(a.name()), a);
        }
        assert_eq!(Action::parse("garbage"), Action::Cancel);
    }

    #[test]
    fn default_font_covers_cyrillic() {
        use ab_glyph::Font;
        let f = default_font().expect("egui default font");
        assert_ne!(f.glyph_id('Ж').0, 0);
        assert!(raster::text_width(&f, 28.0, "Привіт") > 40.0);
    }
}
