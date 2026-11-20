//! The capture overlay: one borderless topmost window per monitor, all sharing one model
//! (selection, annotations, tool, colour) in virtual-desktop pixel coordinates. Each window draws
//! its own slice with Direct2D. Mouse capture follows a drag across monitor boundaries.

use crate::capture::{self, Capture, Monitor};
use crate::config::{format_hex_color, parse_hex_color};
use crate::files::Image;
use crate::geom::*;
use crate::i18n::Lang;
use crate::render::*;
use crate::strings;
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{InvalidateRect, ValidateRect};
use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Input::Ime::{ImmGetContext, ImmReleaseContext, ImmSetCompositionWindow, CFS_POINT, COMPOSITIONFORM};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Copy,
    /// Copy the text inside the selection (`Done::image` holds its raw pixels, `Done::rect` its place).
    CopyText,
    Save,
    Cancel,
    /// Start a scrolling capture of the selected area (`Done::rect`).
    Scroll,
}

/// What the resident process receives when the overlay closes.
pub struct Done {
    pub action: Action,
    pub image: Option<Image>,
    pub color: u32,
    pub stroke: usize,
    /// The selection, for `Action::Scroll`.
    pub rect: Option<Rect>,
}

pub struct Params {
    pub lang: Lang,
    pub color: u32,
    pub stroke: usize,
    /// Start in scrolling-capture mode: the first selection starts the scroll at once.
    pub scroll: bool,
}

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
    Scroll,
    /// Starts the scrolling capture (the scrolling-capture overlay has only this and Close).
    Start,
    Undo,
    Copy,
    CopyText,
    Save,
    Close,
}

const PRESETS: [u32; 10] = [0xff3b30, 0xff9500, 0xffcc00, 0x34c759, 0x32d4e6, 0x0a84ff, 0xaf52de, 0xff2d92, 0xffffff, 0x000000];
// The suite's graphite ramp, dark only: the overlay floats over a dimmed screenshot.
const ACCENT: u32 = 0x35a7ff;
const BAR_BG: u32 = 0x19191b;
const BAR_LINE: u32 = 0x3a3a3d;
const BTN_HOVER: u32 = 0x232325;
const BTN_ACTIVE: u32 = 0x303033;
const ICON: u32 = 0xededed;
const ICON_OFF: u32 = 0x636368;
/// Deepest surface: the viewer background, the size label and the tooltips.
const DEEP: u32 = 0x0f0f10;
const TEXT2: u32 = 0xa1a1a6;
const MUTED: u32 = 0x85858b;
const SWATCH_LINE: u32 = 0x5a5a5f;

struct Mon {
    hwnd: HWND,
    rect: Rect,
    scale: f32,
    rt: Option<ID2D1HwndRenderTarget>,
    bmp: Option<ID2D1Bitmap>,
    pen: Option<Pen>,
}

#[derive(Clone, Copy, Debug)]
enum Op {
    Select { start: Pt },
    Move { start: Pt, orig: Rect },
    Resize { handle: Handle, orig: Rect },
    Draw,
    /// Dragging the tall image in the scrolling-capture viewer.
    Pan { start: Pt, scroll: f32 },
}

/// Rows per GPU tile of the tall image (Direct2D bitmaps have a size limit).
const TILE: i32 = 2048;

/// The viewer of a stitched scrolling capture: the image is shown 1:1 (or fitted to the width) and
/// scrolled with the wheel. Annotations live in image pixels.
struct View {
    img: Option<Image>,
    iw: i32,
    ih: i32,
    tiles: Vec<Option<ID2D1Bitmap>>,
    /// The part of the monitor below the toolbar band.
    vp: Rect,
    z: f32,
    /// Image row shown at the top of the viewport.
    scroll: f32,
    ox: i32,
}

impl View {
    fn max_scroll(&self) -> f32 {
        (self.ih as f32 - self.vp.h() as f32 / self.z).max(0.0)
    }

    fn display(&self) -> Rect {
        let w = (self.iw as f32 * self.z).round() as i32;
        let h = ((self.ih as f32 - self.scroll) * self.z).round().min(self.vp.h() as f32) as i32;
        rect(self.ox, self.vp.t, self.ox + w, self.vp.t + h.max(1))
    }
}

struct Toolbar {
    rect: Rect,
    mon: usize,
    scale: f32,
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
    pos: Pf,
    text: String,
    /// Caret as a byte index into `text`.
    caret: usize,
}

struct Session {
    gfx: Rc<Gfx>,
    lang: Lang,
    cap: Option<Capture>,
    view: Option<View>,
    scroll_mode: bool,
    vrect: Rect,
    mons: Vec<Mon>,
    sel: Option<Rect>,
    op: Option<Op>,
    tool: Tool,
    color: u32,
    stroke: usize,
    anns: Vec<Ann>,
    redo: Vec<Ann>,
    cur: Option<Ann>,
    text: Option<TextEdit>,
    tb: Option<Toolbar>,
    hover: Option<Btn>,
    popover: Option<Popover>,
    popover_open: bool,
    hex: String,
    hex_focus: bool,
    cursor: Pt,
    draw_start: Option<Pf>,
    started: Instant,
    high_surrogate: Option<u16>,
}

impl Drop for Session {
    fn drop(&mut self) {
        for m in &mut self.mons {
            // release Direct2D objects before their window goes away
            m.pen = None;
            m.bmp = None;
            m.rt = None;
            unsafe {
                let _ = DestroyWindow(m.hwnd);
            }
        }
    }
}

thread_local! {
    static SESSION: RefCell<Option<Session>> = const { RefCell::new(None) };
    static GFX: RefCell<Option<Rc<Gfx>>> = const { RefCell::new(None) };
}

const CLASS: PCWSTR = w!("SnapperOverlay");

pub fn is_active() -> bool {
    SESSION.with(|c| c.try_borrow().map(|s| s.is_some()).unwrap_or(true))
}

fn gfx() -> Option<Rc<Gfx>> {
    GFX.with(|g| {
        let mut g = g.borrow_mut();
        if g.is_none() {
            *g = Gfx::new().ok().map(Rc::new);
        }
        g.clone()
    })
}

/// Releases the cached Direct2D / DirectWrite factories and everything they hold.
pub fn release_graphics() {
    GFX.with(|g| *g.borrow_mut() = None);
}

/// Creates the factories and a throw-away render target once at startup, so the GPU device and
/// driver are already loaded when the first capture starts (otherwise that costs ~150 ms).
pub fn prewarm(hwnd: HWND) {
    let Some(gfx) = gfx() else { return };
    unsafe {
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
            dpiX: 96.0,
            dpiY: 96.0,
            usage: D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        let hp = D2D1_HWND_RENDER_TARGET_PROPERTIES {
            hwnd,
            pixelSize: D2D_SIZE_U { width: 16, height: 16 },
            presentOptions: D2D1_PRESENT_OPTIONS_IMMEDIATELY,
        };
        if let Ok(rt) = gfx.d2d.CreateHwndRenderTarget(&props, &hp) {
            let base: ID2D1RenderTarget = rt.into();
            let _ = gfx.font(14.0, false);
            base.BeginDraw();
            let _ = base.EndDraw(None, None);
        }
    }
}

fn register_class() {
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            lpszClassName: CLASS,
            ..Default::default()
        };
        RegisterClassExW(&wc);
    }
}

/// Grabs every monitor, then opens the overlay. Returns `false` if it could not start.
pub fn start(params: Params) -> bool {
    if is_active() {
        return false;
    }
    let t0 = Instant::now();
    let Some(gfx) = gfx() else {
        crate::sys::log("gfx init failed");
        return false;
    };
    let monitors: Vec<Monitor> = capture::monitors();
    if monitors.is_empty() {
        return false;
    }
    let vrect = capture::virtual_rect(&monitors);
    // Grab first: nothing of ours is on screen yet.
    let Some(cap) = capture::capture(vrect) else {
        crate::sys::log("capture failed");
        return false;
    };
    crate::sys::log(&format!("captured {}x{} in {:?}", vrect.w(), vrect.h(), t0.elapsed()));
    register_class();
    crate::sys::log(&format!("gfx+capture ready {:?}", t0.elapsed()));

    let mut mons = Vec::new();
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        for m in &monitors {
            let hwnd = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                CLASS,
                w!("Snapper"),
                WS_POPUP,
                m.rect.l,
                m.rect.t,
                m.rect.w(),
                m.rect.h(),
                None,
                None,
                Some(hinst.into()),
                None,
            );
            let Ok(hwnd) = hwnd else { continue };
            mons.push(Mon { hwnd, rect: m.rect, scale: m.scale(), rt: None, bmp: None, pen: None });
        }
    }
    if mons.is_empty() {
        return false;
    }
    crate::sys::log(&format!("windows created {:?}", t0.elapsed()));
    let mut cursor = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut cursor);
    }
    let session = Session {
        gfx,
        lang: params.lang,
        cap: Some(cap),
        view: None,
        scroll_mode: params.scroll,
        vrect,
        mons,
        sel: None,
        op: None,
        tool: Tool::Move,
        color: params.color,
        stroke: params.stroke.min(2),
        anns: Vec::new(),
        redo: Vec::new(),
        cur: None,
        text: None,
        tb: None,
        hover: None,
        popover: None,
        popover_open: false,
        hex: format_hex_color(params.color),
        hex_focus: false,
        cursor: pt(cursor.x, cursor.y),
        draw_start: None,
        started: Instant::now(),
        high_surrogate: None,
    };
    SESSION.with(|c| *c.borrow_mut() = Some(session));
    SESSION.with(|c| {
        let mut b = c.borrow_mut();
        let s = b.as_mut().unwrap();
        for i in 0..s.mons.len() {
            s.ensure_target(i);
            crate::sys::log(&format!("target {i} {:?}", t0.elapsed()));
            s.paint(i);
        }
        unsafe {
            let mut front = None;
            for (i, m) in s.mons.iter().enumerate() {
                let _ = SetWindowPos(m.hwnd, Some(HWND_TOPMOST), m.rect.l, m.rect.t, m.rect.w(), m.rect.h(), SWP_NOACTIVATE | SWP_SHOWWINDOW);
                if m.rect.contains(s.cursor) {
                    front = Some(i);
                }
            }
            let target = s.mons[front.unwrap_or(0)].hwnd;
            force_foreground(target);
        }
        crate::sys::log(&format!("overlay shown after {:?}, {} windows", t0.elapsed(), s.mons.len()));
    });
    true
}

/// Opens the stitched scrolling capture in the viewer on the monitor that holds `near`.
pub fn start_view(params: Params, img: Image, near: Rect) -> bool {
    if is_active() || img.w == 0 || img.h == 0 {
        return false;
    }
    let Some(gfx) = gfx() else { return false };
    let monitors: Vec<Monitor> = capture::monitors();
    let c = near.center();
    let Some(m) = monitors.iter().find(|m| m.rect.contains(c)).or(monitors.first()).copied() else { return false };
    register_class();
    let hwnd = unsafe {
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        CreateWindowExW(WS_EX_TOPMOST | WS_EX_TOOLWINDOW, CLASS, w!("Snapper"), WS_POPUP, m.rect.l, m.rect.t, m.rect.w(), m.rect.h(), None, None, Some(hinst.into()), None)
    };
    let Ok(hwnd) = hwnd else { return false };
    let s = m.scale();
    let (_, th) = toolbar_size(s, Kit::Viewer);
    let band = th + 2 * (10.0 * s).round() as i32;
    let vp = rect(m.rect.l, m.rect.t + band, m.rect.r, m.rect.b);
    let (iw, ih) = (img.w as i32, img.h as i32);
    let z = (vp.w() as f32 / iw as f32).min(1.0);
    let ox = vp.l + ((vp.w() as f32 - iw as f32 * z) / 2.0).round() as i32;
    let tiles = (0..(ih + TILE - 1) / TILE).map(|_| None).collect();
    let view = View { img: Some(img), iw, ih, tiles, vp, z, scroll: 0.0, ox };
    let mut cursor = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut cursor);
    }
    let session = Session {
        gfx,
        lang: params.lang,
        cap: None,
        view: Some(view),
        scroll_mode: false,
        vrect: m.rect,
        mons: vec![Mon { hwnd, rect: m.rect, scale: s, rt: None, bmp: None, pen: None }],
        sel: None,
        op: None,
        tool: Tool::Move,
        color: params.color,
        stroke: params.stroke.min(2),
        anns: Vec::new(),
        redo: Vec::new(),
        cur: None,
        text: None,
        tb: None,
        hover: None,
        popover: None,
        popover_open: false,
        hex: format_hex_color(params.color),
        hex_focus: false,
        cursor: pt(cursor.x, cursor.y),
        draw_start: None,
        started: Instant::now(),
        high_surrogate: None,
    };
    SESSION.with(|c| *c.borrow_mut() = Some(session));
    SESSION.with(|c| {
        let mut b = c.borrow_mut();
        let s = b.as_mut().unwrap();
        s.layout();
        s.ensure_target(0);
        s.paint(0);
        unsafe {
            let m = &s.mons[0];
            let _ = SetWindowPos(m.hwnd, Some(HWND_TOPMOST), m.rect.l, m.rect.t, m.rect.w(), m.rect.h(), SWP_NOACTIVATE | SWP_SHOWWINDOW);
            force_foreground(m.hwnd);
        }
    });
    true
}

/// Makes `hwnd` the foreground window even when Windows would refuse a plain `SetForegroundWindow`.
unsafe fn force_foreground(hwnd: HWND) {
    let _ = SetForegroundWindow(hwnd);
    if GetForegroundWindow() != hwnd {
        let fg = GetForegroundWindow();
        let fg_thread = GetWindowThreadProcessId(fg, None);
        let me = GetCurrentThreadId();
        if fg_thread != 0 && fg_thread != me {
            let _ = AttachThreadInput(me, fg_thread, true);
            let _ = SetForegroundWindow(hwnd);
            let _ = AttachThreadInput(me, fg_thread, false);
        }
    }
    let _ = SetFocus(Some(hwnd));
}

/// Closes the overlay without output (used when the resident process exits).
pub fn cancel() {
    finish(Action::Cancel);
}

fn finish(action: Action) {
    crate::sys::log(&format!("finish {action:?}"));
    let Some(mut s) = SESSION.with(|c| c.try_borrow_mut().ok().and_then(|mut b| b.take())) else { return };
    unsafe {
        let _ = ReleaseCapture();
    }
    s.hide();
    let image = match action {
        Action::Copy | Action::Save => {
            s.commit_text();
            s.render_image()
        }
        Action::CopyText => s.raw_region(),
        _ => None,
    };
    let rect = if matches!(action, Action::Scroll | Action::CopyText) { s.sel } else { None };
    let (color, stroke) = (s.color, s.stroke);
    drop(s);
    crate::resident::capture_done(Done { action, image, color, stroke, rect });
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_ERASEBKGND {
        return LRESULT(1);
    }
    let mut action: Option<Action> = None;
    let res = SESSION.with(|cell| {
        let Ok(mut guard) = cell.try_borrow_mut() else { return None };
        let s = guard.as_mut()?;
        s.handle(hwnd, msg, wp, lp, &mut action)
    });
    if let Some(a) = action {
        finish(a);
    }
    match res {
        Some(r) => r,
        None => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

fn loword(v: isize) -> i32 {
    (v & 0xffff) as i16 as i32
}
fn hiword(v: isize) -> i32 {
    ((v >> 16) & 0xffff) as i16 as i32
}
fn key_down(vk: VIRTUAL_KEY) -> bool {
    unsafe { (GetKeyState(vk.0 as i32) as u16 & 0x8000) != 0 }
}

impl Session {
    fn mon_index(&self, hwnd: HWND) -> Option<usize> {
        self.mons.iter().position(|m| m.hwnd == hwnd)
    }

    fn mon_at(&self, p: Pt) -> usize {
        self.mons
            .iter()
            .position(|m| m.rect.contains(p))
            .unwrap_or_else(|| {
                // nearest monitor for points in gaps
                let mut best = (0, i64::MAX);
                for (i, m) in self.mons.iter().enumerate() {
                    let dx = (m.rect.l - p.x).max(0).max(p.x - (m.rect.r - 1)) as i64;
                    let dy = (m.rect.t - p.y).max(0).max(p.y - (m.rect.b - 1)) as i64;
                    let d = dx * dx + dy * dy;
                    if d < best.1 {
                        best = (i, d);
                    }
                }
                best.0
            })
    }

    fn scale_at(&self, p: Pt) -> f32 {
        self.mons[self.mon_at(p)].scale
    }

    fn hide(&mut self) {
        for m in &self.mons {
            unsafe {
                let _ = ShowWindow(m.hwnd, SW_HIDE);
            }
        }
    }

    fn ensure_target(&mut self, i: usize) {
        if self.mons[i].rt.is_some() {
            return;
        }
        let (hwnd, mr) = (self.mons[i].hwnd, self.mons[i].rect);
        let (stride, offset, base_ptr) = match &self.cap {
            Some(c) => (c.stride(), (mr.t - self.vrect.t) as usize * c.stride() + (mr.l - self.vrect.l) as usize * 4, c.ptr()),
            None => (0, 0, std::ptr::null()),
        };
        unsafe {
            let props = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
                pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
                dpiX: 96.0,
                dpiY: 96.0,
                usage: D2D1_RENDER_TARGET_USAGE_NONE,
                minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
            };
            let hp = D2D1_HWND_RENDER_TARGET_PROPERTIES {
                hwnd,
                pixelSize: D2D_SIZE_U { width: mr.w() as u32, height: mr.h() as u32 },
                presentOptions: D2D1_PRESENT_OPTIONS_IMMEDIATELY,
            };
            let rt = match self.gfx.d2d.CreateHwndRenderTarget(&props, &hp) {
                Ok(rt) => rt,
                Err(e) => {
                    crate::sys::log(&format!("CreateHwndRenderTarget failed: {e}"));
                    return;
                }
            };
            let base: ID2D1RenderTarget = rt.clone().into();
            if let Some(v) = self.view.as_mut() {
                // the viewer makes its tile bitmaps lazily while painting
                for t in v.tiles.iter_mut() {
                    *t = None;
                }
                if let Ok(pen) = Pen::new(&self.gfx, &base) {
                    self.mons[i].rt = Some(rt);
                    self.mons[i].pen = Some(pen);
                }
                return;
            }
            let bmp = base.CreateBitmap(
                D2D_SIZE_U { width: mr.w() as u32, height: mr.h() as u32 },
                Some(base_ptr.add(offset) as *const _),
                stride as u32,
                &bitmap_props(),
            );
            let pen = Pen::new(&self.gfx, &base);
            if let (Err(e), _) | (_, Err(e)) = (&bmp, &pen) {
                crate::sys::log(&format!("bitmap/pen failed: {e}"));
            }
            if let (Ok(bmp), Ok(pen)) = (bmp, pen) {
                self.mons[i].rt = Some(rt);
                self.mons[i].bmp = Some(bmp);
                self.mons[i].pen = Some(pen);
            }
        }
    }

    // ───────────────────────── dirty regions ─────────────────────────

    fn region(&self) -> Option<Rect> {
        let mut r: Option<Rect> = None;
        let mut add = |x: Rect| r = Some(r.map_or(x, |o| o.union(&x)));
        if let Some(s) = self.sel {
            add(s.inflate(70));
        }
        if let Some(Op::Select { .. }) = self.op {
            add(rect(self.cursor.x - 30, self.cursor.y - 30, self.cursor.x + 300, self.cursor.y + 90));
        }
        if let Some(tb) = &self.tb {
            add(tb.rect.inflate(140));
        }
        if let Some(p) = &self.popover {
            add(p.rect.inflate(10));
        }
        r
    }

    fn invalidate(&self, r: Option<Rect>) {
        if self.view.is_some() {
            for m in &self.mons {
                unsafe {
                    let _ = InvalidateRect(Some(m.hwnd), None, false);
                }
            }
            return;
        }
        let Some(r) = r else { return };
        for m in &self.mons {
            if m.rect.intersect(&r).is_some() {
                unsafe {
                    let _ = InvalidateRect(Some(m.hwnd), None, false);
                }
            }
        }
    }

    /// Runs `f`, then repaints everything either the old or the new state touched.
    fn mutate(&mut self, f: impl FnOnce(&mut Self)) {
        let before = self.region();
        f(self);
        self.layout();
        let after = self.region();
        let both = match (before, after) {
            (Some(a), Some(b)) => Some(a.union(&b)),
            (a, b) => a.or(b),
        };
        self.invalidate(both);
    }

    // ───────────────────────── toolbar layout ─────────────────────────

    fn layout(&mut self) {
        if self.view.is_some() {
            self.layout_view();
            return;
        }
        let Some(sel) = self.sel else {
            self.tb = None;
            self.popover = None;
            return;
        };
        if self.op.is_some_and(|o| matches!(o, Op::Select { .. })) {
            self.tb = None;
            self.popover = None;
            return;
        }
        let kit = if self.scroll_mode { Kit::Start } else { Kit::Full };
        let places: Vec<Place> = self.mons.iter().map(|m| Place { rect: m.rect, toolbar: toolbar_size(m.scale, kit) }).collect();
        let gap = (12.0 * self.scale_at(pt(sel.r - 1, sel.b - 1))) as i32;
        if let Some((r, mon)) = place_toolbar(&sel, &places, gap) {
            self.tb = Some(build_toolbar(r, mon, self.mons[mon].scale, kit));
        }
        self.popover = if self.popover_open { self.tb.as_ref().map(|tb| build_popover(tb, self.mons[tb.mon].rect)) } else { None };
    }

    fn handle_half(&self, p: Pt) -> i32 {
        (4.0 * self.scale_at(p)).round() as i32
    }

    fn hit_toolbar(&self, p: Pt) -> Option<Btn> {
        self.tb.as_ref()?.buttons.iter().find(|(_, r)| r.contains(p)).map(|(b, _)| *b)
    }

    fn over_ui(&self, p: Pt) -> bool {
        self.tb.as_ref().is_some_and(|t| t.rect.contains(p)) || self.popover.as_ref().is_some_and(|q| q.rect.contains(p))
    }

    // ───────────────────────── messages ─────────────────────────

    fn handle(&mut self, hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, action: &mut Option<Action>) -> Option<LRESULT> {
        match msg {
            WM_PAINT => {
                if let Some(i) = self.mon_index(hwnd) {
                    self.ensure_target(i);
                    self.paint(i);
                    unsafe {
                        let _ = ValidateRect(Some(hwnd), None);
                    }
                }
                Some(LRESULT(0))
            }
            WM_SETCURSOR => {
                if loword(lp.0) as u32 & 0xffff == HTCLIENT {
                    let mut c = POINT::default();
                    unsafe {
                        let _ = GetCursorPos(&mut c);
                    }
                    let id = self.cursor_for(pt(c.x, c.y));
                    unsafe {
                        if let Ok(h) = LoadCursorW(None, id) {
                            SetCursor(Some(h));
                        }
                    }
                    return Some(LRESULT(1));
                }
                None
            }
            WM_LBUTTONDOWN => {
                let p = self.point_of(hwnd, lp);
                self.on_down(hwnd, p, action);
                Some(LRESULT(0))
            }
            WM_MOUSEMOVE => {
                let p = self.point_of(hwnd, lp);
                self.on_move(p);
                Some(LRESULT(0))
            }
            WM_LBUTTONUP => {
                let p = self.point_of(hwnd, lp);
                self.on_up(p);
                Some(LRESULT(0))
            }
            WM_MOUSEWHEEL => {
                if self.view.is_some() {
                    let delta = ((wp.0 >> 16) & 0xffff) as u16 as i16 as f32;
                    let z = self.view.as_ref().map_or(1.0, |v| v.z);
                    self.mutate(|s| s.scroll_by(-delta / 120.0 * 110.0 / z));
                }
                Some(LRESULT(0))
            }
            WM_RBUTTONDOWN => {
                if self.sel.is_none() {
                    *action = Some(Action::Cancel);
                }
                Some(LRESULT(0))
            }
            WM_KEYDOWN | WM_SYSKEYDOWN => {
                if self.on_key(VIRTUAL_KEY(wp.0 as u16), action) {
                    Some(LRESULT(0))
                } else {
                    None
                }
            }
            WM_CHAR => {
                self.on_char(wp.0 as u16);
                Some(LRESULT(0))
            }
            WM_IME_STARTCOMPOSITION | WM_IME_COMPOSITION => {
                self.place_ime(hwnd);
                None
            }
            WM_ACTIVATE => {
                let inactive = (wp.0 & 0xffff) as u32 == 0; // WA_INACTIVE
                if inactive && self.started.elapsed().as_millis() > 700 {
                    let other = HWND(lp.0 as *mut _);
                    if !self.mons.iter().any(|m| m.hwnd == other) {
                        crate::sys::log("deactivated");
                        *action = Some(Action::Cancel);
                    }
                }
                None
            }
            WM_DISPLAYCHANGE => {
                *action = Some(Action::Cancel);
                None
            }
            _ => None,
        }
    }

    fn point_of(&self, hwnd: HWND, lp: LPARAM) -> Pt {
        let origin = self.mon_index(hwnd).map(|i| self.mons[i].rect).unwrap_or_default();
        pt(origin.l + loword(lp.0), origin.t + hiword(lp.0))
    }

    fn cursor_for(&self, p: Pt) -> PCWSTR {
        if let Some(op) = self.op {
            return match op {
                Op::Select { .. } | Op::Draw => IDC_CROSS,
                Op::Move { .. } | Op::Pan { .. } => IDC_SIZEALL,
                Op::Resize { handle, .. } => resize_cursor(handle),
            };
        }
        if self.over_ui(p) {
            return if self.hit_toolbar(p).is_some() { IDC_HAND } else { IDC_ARROW };
        }
        let Some(sel) = self.sel else { return IDC_CROSS };
        if self.view.is_none() {
            if let Some(h) = hit_handle(&sel, p, self.handle_half(p) + 3) {
                return resize_cursor(h);
            }
        }
        if sel.contains(p) {
            return match self.tool {
                Tool::Move => IDC_SIZEALL,
                Tool::Text => IDC_IBEAM,
                _ => IDC_CROSS,
            };
        }
        IDC_ARROW
    }

    fn clamp_to(&self, p: Pt, r: &Rect) -> Pt {
        pt(p.x.clamp(r.l, r.r - 1), p.y.clamp(r.t, r.b - 1))
    }

    fn on_down(&mut self, hwnd: HWND, p: Pt, action: &mut Option<Action>) {
        // colour popover
        if let Some(pop) = &self.popover {
            if pop.rect.contains(p) {
                let hit = pop.swatches.iter().position(|r| r.contains(p));
                let stroke = pop.strokes.iter().position(|r| r.contains(p));
                let hex = pop.hex.contains(p);
                self.mutate(|s| {
                    if let Some(i) = hit {
                        s.color = PRESETS[i];
                        s.hex = format_hex_color(s.color);
                        s.hex_focus = false;
                    } else if let Some(i) = stroke {
                        s.stroke = i;
                    } else if hex {
                        s.hex_focus = true;
                    }
                });
                return;
            }
            self.mutate(|s| {
                s.popover_open = false;
                s.hex_focus = false;
            });
        }
        // toolbar
        if let Some(b) = self.hit_toolbar(p) {
            self.press(b, action);
            return;
        }
        if self.tb.as_ref().is_some_and(|t| t.rect.contains(p)) {
            return;
        }
        // clicking elsewhere ends text editing
        if self.text.is_some() && !(self.tool == Tool::Text && self.sel.is_some_and(|s| s.contains(p))) {
            self.mutate(|s| s.commit_text());
        }
        unsafe {
            SetCapture(hwnd);
        }
        let Some(sel) = self.sel else {
            let p = self.clamp_to(p, &self.vrect);
            self.mutate(|s| {
                s.op = Some(Op::Select { start: p });
                s.sel = Some(rect(p.x, p.y, p.x + 1, p.y + 1));
            });
            return;
        };
        if self.view.is_none() {
            if let Some(h) = hit_handle(&sel, p, self.handle_half(p) + 3) {
                self.op = Some(Op::Resize { handle: h, orig: sel });
                return;
            }
        }
        if !sel.contains(p) {
            unsafe {
                let _ = ReleaseCapture();
            }
            return;
        }
        match self.tool {
            Tool::Move => {
                self.op = Some(match &self.view {
                    Some(v) => Op::Pan { start: p, scroll: v.scroll },
                    None => Op::Move { start: p, orig: sel },
                })
            }
            Tool::Text => {
                if self.text.is_some() {
                    self.mutate(|s| s.commit_text());
                }
                let pos = self.ann_pt(p, 0.0);
                self.mutate(|s| {
                    s.text = Some(TextEdit { pos, text: String::new(), caret: 0 });
                });
                unsafe {
                    let _ = ReleaseCapture();
                }
                self.place_ime(hwnd);
            }
            t => {
                let q = self.ann_pt(p, 0.5);
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
                self.invalidate(Some(sel));
            }
        }
    }

    fn on_move(&mut self, p: Pt) {
        let prev = self.cursor;
        self.cursor = p;
        let Some(op) = self.op else {
            // hover over toolbar buttons (tooltips)
            let h = self.hit_toolbar(p);
            if h != self.hover {
                self.hover = h;
                if let Some(tb) = &self.tb {
                    self.invalidate(Some(tb.rect.inflate(140)));
                }
            }
            return;
        };
        let _ = prev;
        match op {
            Op::Select { start } => {
                let q = self.clamp_to(p, &self.vrect);
                self.mutate(|s| s.sel = Some(rect(start.x.min(q.x), start.y.min(q.y), start.x.max(q.x) + 1, start.y.max(q.y) + 1)));
            }
            Op::Move { start, orig } => {
                let vr = self.vrect;
                self.mutate(|s| s.sel = Some(move_rect(&orig, p.x - start.x, p.y - start.y, &vr)));
            }
            Op::Resize { handle, orig } => {
                let vr = self.vrect;
                self.mutate(|s| s.sel = Some(resize(&orig, handle, p, 2, &vr)));
            }
            Op::Pan { start, scroll } => {
                let z = self.view.as_ref().map_or(1.0, |v| v.z);
                self.mutate(|s| {
                    if let Some(v) = s.view.as_mut() {
                        v.scroll = scroll - (p.y - start.y) as f32 / z;
                    }
                });
            }
            Op::Draw => {
                let Some(sel) = self.sel else { return };
                let q = self.clamp_to(p, &sel);
                let q = self.ann_pt(q, 0.5);
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
                self.invalidate(Some(sel));
            }
        }
    }

    fn on_up(&mut self, p: Pt) {
        let Some(op) = self.op else { return };
        self.on_move(p);
        unsafe {
            let _ = ReleaseCapture();
        }
        self.mutate(|s| {
            s.op = None;
            match op {
                Op::Select { .. } => {
                    if s.sel.is_some_and(|r| r.w() < 2 || r.h() < 2) {
                        s.sel = None;
                    } else {
                        s.tool = Tool::Move;
                    }
                }
                Op::Draw => {
                    s.draw_start = None;
                    if let Some(a) = s.cur.take() {
                        let ok = match &a {
                            Ann::Arrow { a, b, .. } => (a.0 - b.0).abs() + (a.1 - b.1).abs() >= 3.0,
                            Ann::Rect { r, .. } | Ann::Fill { r, .. } => r.2 - r.0 >= 1.0 || r.3 - r.1 >= 1.0,
                            _ => true,
                        };
                        if ok {
                            s.anns.push(a);
                            s.redo.clear();
                        }
                    }
                }
                _ => {}
            }
        });
        // pointer may now sit on a different handle/tool: refresh the cursor
        let _ = self.hover;
    }

    fn press(&mut self, b: Btn, action: &mut Option<Action>) {
        match b {
            Btn::Tool(t) => self.mutate(|s| {
                s.commit_text();
                s.tool = t;
                s.popover_open = false;
            }),
            Btn::Color => self.mutate(|s| {
                s.commit_text();
                s.popover_open = !s.popover_open;
                s.hex_focus = false;
            }),
            Btn::Scroll | Btn::Start => *action = Some(Action::Scroll),
            Btn::Undo => self.mutate(|s| s.undo()),
            Btn::Copy => *action = Some(Action::Copy),
            Btn::CopyText => *action = Some(Action::CopyText),
            Btn::Save => *action = Some(Action::Save),
            Btn::Close => *action = Some(Action::Cancel),
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

    /// Finishes the text being edited (empty text is dropped).
    fn commit_text(&mut self) {
        if let Some(t) = self.text.take() {
            if !t.text.trim().is_empty() {
                self.anns.push(Ann::Text { pos: t.pos, text: t.text, color: self.color, size: TEXT_SIZES[self.stroke] });
                self.redo.clear();
            }
        }
    }

    fn on_key(&mut self, vk: VIRTUAL_KEY, action: &mut Option<Action>) -> bool {
        let ctrl = key_down(VK_CONTROL);
        let shift = key_down(VK_SHIFT);
        let editing = self.text.is_some();
        match vk {
            VK_ESCAPE => {
                if editing {
                    self.mutate(|s| s.text = None);
                } else if self.hex_focus {
                    self.mutate(|s| s.hex_focus = false);
                } else {
                    *action = Some(Action::Cancel);
                }
            }
            VK_RETURN => {
                if editing {
                    if shift {
                        self.mutate(|s| s.insert_text("\n"));
                    } else {
                        self.mutate(|s| s.commit_text());
                    }
                } else if self.hex_focus {
                    self.mutate(|s| s.hex_focus = false);
                } else if self.sel.is_some() && self.op.is_none() {
                    *action = Some(if self.scroll_mode { Action::Scroll } else { Action::Copy });
                }
            }
            VK_BACK => {
                if editing {
                    self.mutate(|s| s.text_backspace());
                } else if self.hex_focus {
                    self.mutate(|s| {
                        s.hex.pop();
                        s.apply_hex();
                    });
                }
            }
            VK_DELETE if editing => self.mutate(|s| s.text_delete()),
            VK_LEFT if editing => self.mutate(|s| s.text_move(-1)),
            VK_RIGHT if editing => self.mutate(|s| s.text_move(1)),
            VK_HOME if editing => self.mutate(|s| s.text_move(-2)),
            VK_END if editing => self.mutate(|s| s.text_move(2)),
            VK_PRIOR | VK_NEXT | VK_UP | VK_DOWN | VK_HOME | VK_END if self.view.is_some() => {
                let (page, z) = self.view.as_ref().map_or((0.0, 1.0), |v| (v.vp.h() as f32 * 0.9, v.z));
                let d = match vk {
                    VK_PRIOR => -page / z,
                    VK_NEXT => page / z,
                    VK_UP => -60.0 / z,
                    VK_DOWN => 60.0 / z,
                    VK_HOME => -1e9,
                    _ => 1e9,
                };
                self.mutate(|s| s.scroll_by(d));
            }
            _ if ctrl => match vk.0 {
                0x43 => {
                    if self.sel.is_some() && self.view.is_none() {
                        *action = Some(if shift { Action::CopyText } else { Action::Copy });
                    } else if self.sel.is_some() {
                        *action = Some(Action::Copy);
                    }
                }
                0x53 => {
                    if self.sel.is_some() {
                        *action = Some(Action::Save);
                    }
                }
                0x5A => {
                    if !editing {
                        self.mutate(|s| s.undo());
                    }
                }
                0x59 => {
                    if !editing {
                        self.mutate(|s| s.redo_one());
                    }
                }
                0x56 => {
                    if let Some(t) = clipboard_text() {
                        if editing {
                            self.mutate(|s| s.insert_text(&t));
                        } else if self.hex_focus {
                            self.mutate(|s| {
                                s.hex = t.trim().chars().filter(|c| c.is_ascii_hexdigit() || *c == '#').take(7).collect();
                                s.apply_hex();
                            });
                        }
                    }
                }
                _ => return false,
            },
            _ => return false,
        }
        true
    }

    fn on_char(&mut self, ch: u16) {
        if self.text.is_some() {
            if ch < 0x20 || ch == 0x7f {
                return;
            }
            let s = if (0xD800..0xDC00).contains(&ch) {
                self.high_surrogate = Some(ch);
                return;
            } else if (0xDC00..0xE000).contains(&ch) {
                let Some(hi) = self.high_surrogate.take() else { return };
                String::from_utf16_lossy(&[hi, ch])
            } else {
                String::from_utf16_lossy(&[ch])
            };
            self.mutate(|x| x.insert_text(&s));
        } else if self.hex_focus {
            let c = char::from_u32(ch as u32).unwrap_or(' ');
            if (c.is_ascii_hexdigit() || c == '#') && self.hex.chars().count() < 7 {
                self.mutate(|s| {
                    s.hex.push(c);
                    s.apply_hex();
                });
            }
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

    fn place_ime(&self, hwnd: HWND) {
        let (Some(t), Some(i)) = (&self.text, self.mon_index(hwnd)) else { return };
        let (cx, cy) = self.to_screen(self.caret_point(t));
        let m = &self.mons[i];
        unsafe {
            let himc = ImmGetContext(hwnd);
            if !himc.is_invalid() {
                let cf = COMPOSITIONFORM {
                    dwStyle: CFS_POINT,
                    ptCurrentPos: POINT { x: cx as i32 - m.rect.l, y: cy as i32 - m.rect.t },
                    rcArea: Default::default(),
                };
                let _ = ImmSetCompositionWindow(himc, &cf);
                let _ = ImmReleaseContext(hwnd, himc);
            }
        }
    }

    /// Top-left corner of the caret in virtual-desktop pixels.
    fn caret_point(&self, t: &TextEdit) -> (f32, f32) {
        let size = TEXT_SIZES[self.stroke];
        let Some(l) = self.gfx.layout(&t.text, size, false) else { return t.pos };
        let idx = t.text[..t.caret].encode_utf16().count() as u32;
        let (mut x, mut y) = (0f32, 0f32);
        let mut m = windows::Win32::Graphics::DirectWrite::DWRITE_HIT_TEST_METRICS::default();
        unsafe {
            let _ = l.HitTestTextPosition(idx, false, &mut x, &mut y, &mut m);
        }
        (t.pos.0 + x, t.pos.1 + y)
    }

    // ───────────────────────── output ─────────────────────────

    /// The selected pixels exactly as they were on screen: no dimming, no annotations (for the text reader).
    fn raw_region(&self) -> Option<Image> {
        let sel = self.sel?.intersect(&self.vrect)?;
        let cap = self.cap.as_ref()?;
        let stride = cap.stride();
        let (w, h) = (sel.w() as usize, sel.h() as usize);
        let mut bgra = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            let off = (sel.t - self.vrect.t) as usize * stride + y * stride + (sel.l - self.vrect.l) as usize * 4;
            // SAFETY: the selection lies inside the captured virtual desktop (intersected above)
            bgra.extend_from_slice(unsafe { std::slice::from_raw_parts(cap.ptr().add(off), w * 4) });
        }
        Some(Image { w: w as u32, h: h as u32, bgra })
    }

    fn render_image(&mut self) -> Option<Image> {
        if let Some(v) = self.view.as_mut() {
            let img = v.img.take()?;
            return render_tall(&self.gfx, img, &self.anns).ok();
        }
        let sel = self.sel?;
        render_output(&self.gfx, self.cap.as_ref()?.ptr(), &self.vrect, &sel, &self.anns).ok()
    }

    // ───────────────────────── scrolling-capture viewer ─────────────────────────

    /// Screen pixel -> annotation coordinates (`bias` 0.5 centres on the pixel).
    fn ann_pt(&self, p: Pt, bias: f32) -> Pf {
        match &self.view {
            Some(v) => ((p.x - v.ox) as f32 / v.z, v.scroll + (p.y - v.vp.t) as f32 / v.z),
            None => (p.x as f32 + bias, p.y as f32 + bias),
        }
    }

    /// Annotation coordinates -> screen pixels.
    fn to_screen(&self, a: Pf) -> Pf {
        match &self.view {
            Some(v) => (v.ox as f32 + a.0 * v.z, v.vp.t as f32 + (a.1 - v.scroll) * v.z),
            None => a,
        }
    }

    fn scroll_by(&mut self, d: f32) {
        if let Some(v) = self.view.as_mut() {
            v.scroll += d;
        }
    }

    /// Keeps the view in range, refreshes the visible image area and puts the toolbar in its band.
    fn layout_view(&mut self) {
        let Some(v) = self.view.as_mut() else { return };
        v.scroll = v.scroll.clamp(0.0, v.max_scroll());
        let disp = v.display();
        self.sel = Some(disp);
        let m = &self.mons[0];
        let s = m.scale;
        let (tw, th) = toolbar_size(s, Kit::Viewer);
        let pad = (10.0 * s).round() as i32;
        let x = m.rect.l + (m.rect.w() - tw) / 2;
        let y = m.rect.t + pad;
        let mr = m.rect;
        self.tb = Some(build_toolbar(rect(x, y, x + tw, y + th), 0, s, Kit::Viewer));
        self.popover = if self.popover_open { self.tb.as_ref().map(|tb| build_popover(tb, mr)) } else { None };
    }

    /// Creates the GPU tiles that the current scroll position needs.
    fn ensure_tiles(&mut self) {
        let Some(rt) = self.mons[0].rt.clone() else { return };
        let Some(v) = self.view.as_mut() else { return };
        let Some(img) = v.img.as_ref() else { return };
        let top = v.scroll.floor() as i32;
        let bottom = (v.scroll + v.vp.h() as f32 / v.z).ceil() as i32;
        let first = (top / TILE).max(0) as usize;
        let last = ((bottom / TILE).max(0) as usize).min(v.tiles.len().saturating_sub(1));
        let base: ID2D1RenderTarget = rt.into();
        for k in first..=last {
            if v.tiles[k].is_some() {
                continue;
            }
            let y0 = k as i32 * TILE;
            let rows = (v.ih - y0).min(TILE);
            let stride = v.iw as usize * 4;
            let ptr = unsafe { img.bgra.as_ptr().add(y0 as usize * stride) };
            let bmp = unsafe { base.CreateBitmap(D2D_SIZE_U { width: v.iw as u32, height: rows as u32 }, Some(ptr as *const _), stride as u32, &bitmap_props()) };
            match bmp {
                Ok(b) => v.tiles[k] = Some(b),
                Err(e) => crate::sys::log(&format!("tile {k} failed: {e}")),
            }
        }
    }

    fn paint_view(&mut self, i: usize) {
        self.ensure_tiles();
        let gfx = self.gfx.clone();
        let m = &self.mons[i];
        let (Some(hrt), Some(pen), Some(v)) = (&m.rt, &m.pen, &self.view) else { return };
        let rt: &ID2D1RenderTarget = hrt;
        let mr = m.rect;
        let scale = m.scale;
        let sel = self.sel.unwrap_or(v.vp);
        unsafe {
            rt.BeginDraw();
            rt.SetTransform(&windows_numerics_translate(-(mr.l as f32), -(mr.t as f32)));
            rt.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
            rt.Clear(Some(&colorf(0x141415, 1.0)));
            let b = &pen.brush;
            // the image, tile by tile
            rt.PushAxisAlignedClip(&rect_f(&v.vp), D2D1_ANTIALIAS_MODE_ALIASED);
            let tx = (v.ox - mr.l) as f32;
            let ty = (v.vp.t - mr.t) as f32 - v.scroll * v.z;
            let image_space = windows_numerics::Matrix3x2 { M11: v.z, M12: 0.0, M21: 0.0, M22: v.z, M31: tx, M32: ty };
            rt.SetTransform(&image_space);
            let mode = if (v.z - 1.0).abs() < 1e-3 { D2D1_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR } else { D2D1_BITMAP_INTERPOLATION_MODE_LINEAR };
            for (k, t) in v.tiles.iter().enumerate() {
                let Some(t) = t else { continue };
                let y0 = (k as i32 * TILE) as f32;
                let rows = (v.ih - k as i32 * TILE).min(TILE) as f32;
                rt.DrawBitmap(t, Some(&rf(0.0, y0, v.iw as f32, y0 + rows)), 1.0, mode, None);
            }
            // annotations, clipped to the visible image
            rt.SetTransform(&windows_numerics_translate(-(mr.l as f32), -(mr.t as f32)));
            rt.PushAxisAlignedClip(&rect_f(&sel), D2D1_ANTIALIAS_MODE_ALIASED);
            rt.SetTransform(&image_space);
            for a in &self.anns {
                draw_ann(&gfx, rt, pen, a);
            }
            if let Some(a) = &self.cur {
                draw_ann(&gfx, rt, pen, a);
            }
            if let Some(t) = &self.text {
                self.draw_text_edit(rt, pen, t);
            }
            rt.SetTransform(&windows_numerics_translate(-(mr.l as f32), -(mr.t as f32)));
            rt.PopAxisAlignedClip();
            rt.PopAxisAlignedClip();
            // frame and scroll thumb
            b.SetColor(&colorf(ACCENT, 1.0));
            rt.DrawRectangle(&rf(sel.l as f32 - 0.5, sel.t as f32 - 0.5, sel.r as f32 + 0.5, sel.b as f32 + 0.5), b, 1.0, None);
            draw_crop_marks(rt, b, &sel, scale);
            let total = v.ih as f32 * v.z;
            if total > v.vp.h() as f32 {
                let track = v.vp.h() as f32 - 8.0 * scale;
                let th = (track * v.vp.h() as f32 / total).max(24.0 * scale);
                let top = v.vp.t as f32 + 4.0 * scale + (track - th) * (v.scroll / v.max_scroll().max(1.0));
                let x = v.vp.r as f32 - 8.0 * scale;
                b.SetColor(&colorf(0xffffff, 0.45));
                let rr = D2D1_ROUNDED_RECT { rect: rf(x, top, x + 4.0 * scale, top + th), radiusX: 2.0 * scale, radiusY: 2.0 * scale };
                rt.FillRoundedRectangle(&rr, b);
            }
            // size label at the left of the toolbar band
            let text = if (v.z - 1.0).abs() < 1e-3 { format!("{} \u{00d7} {}", v.iw, v.ih) } else { format!("{} \u{00d7} {}  ({:.0}%)", v.iw, v.ih, v.z * 100.0) };
            let fs = 12.5 * scale;
            let fmt = gfx.font_mono(fs);
            let (_, th) = gfx.measure_mono(&text, fs);
            b.SetColor(&colorf(TEXT2, 1.0));
            let wide: Vec<u16> = text.encode_utf16().collect();
            let ly = mr.t as f32 + ((v.vp.t - mr.t) as f32 - th) / 2.0;
            rt.DrawText(&wide, &fmt, &rf(mr.l as f32 + 18.0 * scale, ly, mr.l as f32 + 400.0 * scale, ly + th + 4.0), b, D2D1_DRAW_TEXT_OPTIONS_NONE, windows::Win32::Graphics::DirectWrite::DWRITE_MEASURING_MODE_NATURAL);
            if let Some(tb) = &self.tb {
                self.draw_toolbar(&gfx, rt, pen, tb);
                if let Some(p) = &self.popover {
                    self.draw_popover(&gfx, rt, pen, p, tb.scale);
                } else if let Some(h) = self.hover {
                    self.draw_tooltip(&gfx, rt, pen, tb, h);
                }
            }
            rt.SetTransform(&windows_numerics_translate(0.0, 0.0));
            if let Err(e) = rt.EndDraw(None, None) {
                crate::sys::log(&format!("EndDraw (view) failed: {e}"));
                if e.code().0 as u32 == 0x8899000C {
                    let m = &mut self.mons[i];
                    m.rt = None;
                    m.pen = None;
                    let _ = InvalidateRect(Some(m.hwnd), None, false);
                }
            }
        }
    }

    // ───────────────────────── painting ─────────────────────────

    fn paint(&mut self, i: usize) {
        if self.view.is_some() {
            self.paint_view(i);
            return;
        }
        let gfx = self.gfx.clone();
        let m = &self.mons[i];
        let (Some(hrt), Some(bmp), Some(pen)) = (&m.rt, &m.bmp, &m.pen) else {
            return;
        };
        let rt: &ID2D1RenderTarget = hrt;
        let mr = m.rect;
        let scale = m.scale;
        unsafe {
            rt.BeginDraw();
            rt.SetTransform(&windows_numerics_translate(-(mr.l as f32), -(mr.t as f32)));
            rt.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
            let b = &pen.brush;
            rt.DrawBitmap(bmp, Some(&rect_f(&mr)), 1.0, D2D1_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR, None);
            b.SetColor(&colorf(0x000000, 0.45));
            rt.FillRectangle(&rect_f(&mr), b);

            if let Some(sel) = self.sel {
                if let Some(ir) = sel.intersect(&mr) {
                    let src = ir.offset(-mr.l, -mr.t);
                    rt.DrawBitmap(bmp, Some(&rect_f(&ir)), 1.0, D2D1_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR, Some(&rect_f(&src)));
                }
                // annotations, clipped to the selection
                rt.PushAxisAlignedClip(&rect_f(&sel), D2D1_ANTIALIAS_MODE_ALIASED);
                for a in &self.anns {
                    draw_ann(&gfx, rt, pen, a);
                }
                if let Some(a) = &self.cur {
                    draw_ann(&gfx, rt, pen, a);
                }
                if let Some(t) = &self.text {
                    self.draw_text_edit(rt, pen, t);
                }
                rt.PopAxisAlignedClip();

                // border
                b.SetColor(&colorf(ACCENT, 1.0));
                rt.DrawRectangle(&rf(sel.l as f32 - 0.5, sel.t as f32 - 0.5, sel.r as f32 + 0.5, sel.b as f32 + 0.5), b, 1.0, None);
                draw_crop_marks(rt, b, &sel, scale);

                if !matches!(self.op, Some(Op::Select { .. })) {
                    let half = 4.0 * scale;
                    for h in Handle::ALL {
                        let c = handle_point(&sel, h);
                        let r = rf(c.x as f32 - half, c.y as f32 - half, c.x as f32 + half, c.y as f32 + half);
                        b.SetColor(&colorf(0xffffff, 1.0));
                        rt.FillRectangle(&r, b);
                        b.SetColor(&colorf(ACCENT, 1.0));
                        rt.DrawRectangle(&r, b, 1.0, None);
                    }
                }
                self.draw_label(&gfx, rt, pen, &sel, scale);
            }
            if let Some(tb) = &self.tb {
                self.draw_toolbar(&gfx, rt, pen, tb);
                if let Some(p) = &self.popover {
                    self.draw_popover(&gfx, rt, pen, p, tb.scale);
                } else if let Some(h) = self.hover {
                    self.draw_tooltip(&gfx, rt, pen, tb, h);
                }
            }
            rt.SetTransform(&windows_numerics_translate(0.0, 0.0));
            if let Err(e) = rt.EndDraw(None, None) {
                crate::sys::log(&format!("EndDraw failed: {e}"));
                if e.code().0 as u32 == 0x8899000C {
                    // render target lost: rebuild on the next paint
                    let m = &mut self.mons[i];
                    m.rt = None;
                    m.bmp = None;
                    m.pen = None;
                    let _ = InvalidateRect(Some(m.hwnd), None, false);
                }
            }
        }
    }

    unsafe fn draw_text_edit(&self, rt: &ID2D1RenderTarget, pen: &Pen, t: &TextEdit) {
        let size = TEXT_SIZES[self.stroke];
        let shown = Ann::Text { pos: t.pos, text: t.text.clone(), color: self.color, size };
        draw_ann(&self.gfx, rt, pen, &shown);
        let (tw, th) = self.gfx.measure(&t.text, size, false);
        pen.brush.SetColor(&colorf(ACCENT, 0.8));
        rt.DrawRectangle(&rf(t.pos.0 - 4.0, t.pos.1 - 2.0, t.pos.0 + tw.max(8.0) + 6.0, t.pos.1 + th.max(size) + 2.0), &pen.brush, 1.0, None);
        let (cx, cy) = self.caret_point(t);
        pen.brush.SetColor(&colorf(if self.color == 0 { 0xffffff } else { self.color }, 1.0));
        rt.DrawLine(p2((cx, cy)), p2((cx, cy + size * 1.25)), &pen.brush, 2.0, None);
    }

    unsafe fn draw_label(&self, gfx: &Gfx, rt: &ID2D1RenderTarget, pen: &Pen, sel: &Rect, scale: f32) {
        let size = format!("{} \u{00d7} {}", sel.w(), sel.h());
        let text = if self.scroll_mode {
            let t = strings::tr(self.lang);
            format!("{}  \u{00b7}  {}  \u{00b7}  {}", t.t_scroll, size, t.scroll_hint_start)
        } else {
            size
        };
        let fs = 12.5 * scale;
        let (tw, th) = gfx.measure_mono(&text, fs);
        let (pw, ph) = (8.0 * scale, 4.0 * scale);
        let (w, h) = (tw + pw * 2.0, th + ph * 2.0);
        let (x, y) = if matches!(self.op, Some(Op::Select { .. })) {
            ((self.cursor.x + 16) as f32, (self.cursor.y + 20) as f32)
        } else {
            let y = sel.t as f32 - h - 12.0 * scale;
            (sel.l as f32, if y < self.vrect.t as f32 { sel.t as f32 + 6.0 * scale } else { y })
        };
        let x = x.clamp(self.vrect.l as f32, (self.vrect.r as f32 - w).max(self.vrect.l as f32));
        let y = y.clamp(self.vrect.t as f32, (self.vrect.b as f32 - h).max(self.vrect.t as f32));
        let rr = D2D1_ROUNDED_RECT { rect: rf(x, y, x + w, y + h), radiusX: 6.0 * scale, radiusY: 6.0 * scale };
        pen.brush.SetColor(&colorf(DEEP, 0.88));
        rt.FillRoundedRectangle(&rr, &pen.brush);
        pen.brush.SetColor(&colorf(ICON, 1.0));
        let fmt = gfx.font_mono(fs);
        let wide: Vec<u16> = text.encode_utf16().collect();
        rt.DrawText(&wide, &fmt, &rf(x + pw, y + ph, x + w, y + h), &pen.brush, D2D1_DRAW_TEXT_OPTIONS_NONE, windows::Win32::Graphics::DirectWrite::DWRITE_MEASURING_MODE_NATURAL);
    }

    unsafe fn draw_toolbar(&self, gfx: &Gfx, rt: &ID2D1RenderTarget, pen: &Pen, tb: &Toolbar) {
        let s = tb.scale;
        let b = &pen.brush;
        let rr = D2D1_ROUNDED_RECT { rect: rect_f(&tb.rect), radiusX: 16.0 * s, radiusY: 16.0 * s };
        b.SetColor(&colorf(BAR_BG, 0.97));
        rt.FillRoundedRectangle(&rr, b);
        b.SetColor(&colorf(BAR_LINE, 1.0));
        let inset = D2D1_ROUNDED_RECT { rect: rf(rr.rect.left + 0.5, rr.rect.top + 0.5, rr.rect.right - 0.5, rr.rect.bottom - 0.5), radiusX: rr.radiusX, radiusY: rr.radiusY };
        rt.DrawRoundedRectangle(&inset, b, 1.0, None);
        // separator
        b.SetColor(&colorf(BAR_LINE, 1.0));
        if tb.sep_x > tb.rect.l {
            rt.DrawLine(p2((tb.sep_x as f32 + 0.5, tb.rect.t as f32 + 12.0 * s)), p2((tb.sep_x as f32 + 0.5, tb.rect.b as f32 - 12.0 * s)), b, 1.0, None);
        }
        for (btn, r) in &tb.buttons {
            let active = *btn == Btn::Tool(self.tool);
            let hover = self.hover == Some(*btn);
            if active || hover {
                let rr = D2D1_ROUNDED_RECT { rect: rect_f(r), radiusX: 8.0 * s, radiusY: 8.0 * s };
                b.SetColor(&colorf(if active { BTN_ACTIVE } else { BTN_HOVER }, 1.0));
                rt.FillRoundedRectangle(&rr, b);
            }
            let disabled = *btn == Btn::Undo && self.anns.is_empty();
            let col = if disabled { ICON_OFF } else if active { ACCENT } else { ICON };
            self.draw_icon(gfx, rt, pen, *btn, r, s, col);
        }
    }

    unsafe fn draw_icon(&self, gfx: &Gfx, rt: &ID2D1RenderTarget, pen: &Pen, btn: Btn, r: &Rect, s: f32, col: u32) {
        let b = &pen.brush;
        let k = 22.0 * s / 24.0;
        let (ox, oy) = (r.center().x as f32 - 11.0 * s, r.center().y as f32 - 11.0 * s);
        let p = |x: f32, y: f32| v2(ox + x * k, oy + y * k);
        let lw = 1.8 * s.max(1.0);
        b.SetColor(&colorf(col, 1.0));
        let line = |a: (f32, f32), c: (f32, f32), w: f32| rt.DrawLine(p(a.0, a.1), p(c.0, c.1), b, w, &pen.round);
        let wings = |tip: (f32, f32), from: (f32, f32), len: f32| {
            let (dx, dy) = (tip.0 - from.0, tip.1 - from.1);
            let l = (dx * dx + dy * dy).sqrt().max(0.001);
            let (ux, uy) = (dx / l, dy / l);
            for sign in [-1.0f32, 1.0] {
                let ang = 0.62f32 * sign;
                let (c, sn) = (ang.cos(), ang.sin());
                let (vx, vy) = (-(ux * c - uy * sn), -(ux * sn + uy * c));
                rt.DrawLine(p(tip.0, tip.1), p(tip.0 + vx * len, tip.1 + vy * len), b, lw, &pen.round);
            }
        };
        let tri = |a: (f32, f32), c: (f32, f32), d: (f32, f32)| {
            if let Ok(g) = gfx.d2d.CreatePathGeometry() {
                if let Ok(sink) = g.Open() {
                    sink.BeginFigure(p(a.0, a.1), D2D1_FIGURE_BEGIN_FILLED);
                    sink.AddLine(p(c.0, c.1));
                    sink.AddLine(p(d.0, d.1));
                    sink.EndFigure(D2D1_FIGURE_END_CLOSED);
                    let _ = sink.Close();
                    rt.FillGeometry(&g, b, None);
                }
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
                line((8.0, 16.0), (17.5, 6.5), 3.6 * s.max(1.0));
                tri((4.0, 20.0), (5.6, 14.6), (9.4, 18.4));
            }
            Btn::Tool(Tool::Arrow) => {
                line((5.0, 19.0), (16.0, 8.0), lw);
                tri((19.5, 4.5), (19.5, 12.0), (12.0, 4.5));
            }
            Btn::Tool(Tool::Rect) => {
                rt.DrawRectangle(&rf(ox + 4.5 * k, oy + 6.5 * k, ox + 19.5 * k, oy + 17.5 * k), b, lw, None);
            }
            Btn::Tool(Tool::Fill) => {
                let rr = D2D1_ROUNDED_RECT { rect: rf(ox + 4.0 * k, oy + 6.0 * k, ox + 20.0 * k, oy + 18.0 * k), radiusX: 2.0 * k, radiusY: 2.0 * k };
                rt.FillRoundedRectangle(&rr, b);
            }
            Btn::Tool(Tool::Text) => {
                line((6.0, 6.5), (18.0, 6.5), lw * 1.15);
                line((12.0, 6.5), (12.0, 19.0), lw * 1.15);
                line((9.5, 19.0), (14.5, 19.0), lw * 1.15);
            }
            Btn::Color => {
                b.SetColor(&colorf(self.color, 1.0));
                let c = p(12.0, 12.0);
                rt.FillEllipse(&D2D1_ELLIPSE { point: c, radiusX: 8.0 * k, radiusY: 8.0 * k }, b);
                b.SetColor(&colorf(0xffffff, 1.0));
                rt.DrawEllipse(&D2D1_ELLIPSE { point: c, radiusX: 8.0 * k, radiusY: 8.0 * k }, b, lw, None);
            }
            Btn::Start => {
                b.SetColor(&colorf(ACCENT, 1.0));
                tri((8.0, 5.0), (8.0, 19.0), (19.5, 12.0));
            }
            Btn::Scroll => {
                line((7.0, 4.0), (17.0, 4.0), lw);
                line((12.0, 4.5), (12.0, 18.5), lw);
                wings((12.0, 20.0), (12.0, 13.0), 5.2);
            }
            Btn::Undo => {
                let mut prev = None;
                let mut first = None;
                for n in 0..=16 {
                    let th = (150.0f32 - n as f32 * 11.0).to_radians();
                    let pt = (12.0 + 6.5 * th.cos(), 14.0 - 6.5 * th.sin());
                    if let Some(q) = prev {
                        line(q, pt, lw);
                    } else {
                        first = Some(pt);
                    }
                    prev = Some(pt);
                }
                if let Some(f) = first {
                    // arrowhead at the start of the arc pointing back along it
                    wings(f, (f.0 + 1.6, f.1 - 3.0), 4.8);
                }
            }
            Btn::Copy => {
                let rr = |l: f32, t: f32, r2: f32, b2: f32| D2D1_ROUNDED_RECT { rect: rf(ox + l * k, oy + t * k, ox + r2 * k, oy + b2 * k), radiusX: 2.0 * k, radiusY: 2.0 * k };
                rt.DrawRoundedRectangle(&rr(8.5, 4.5, 19.5, 15.5), b, lw, None);
                b.SetColor(&colorf(BAR_BG, 1.0));
                rt.FillRoundedRectangle(&rr(4.5, 8.5, 15.5, 19.5), b);
                b.SetColor(&colorf(col, 1.0));
                rt.DrawRoundedRectangle(&rr(4.5, 8.5, 15.5, 19.5), b, lw, None);
            }
            Btn::CopyText => {
                // crop corners around two lines of text
                for (cx, cy, dx, dy) in [(4.0f32, 4.0f32, 1.0f32, 1.0f32), (20.0, 4.0, -1.0, 1.0), (4.0, 20.0, 1.0, -1.0), (20.0, 20.0, -1.0, -1.0)] {
                    line((cx, cy), (cx + 4.5 * dx, cy), lw);
                    line((cx, cy), (cx, cy + 4.5 * dy), lw);
                }
                line((8.5, 10.0), (15.5, 10.0), lw);
                line((8.5, 14.0), (13.0, 14.0), lw);
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

    unsafe fn draw_tooltip(&self, gfx: &Gfx, rt: &ID2D1RenderTarget, pen: &Pen, tb: &Toolbar, btn: Btn) {
        let t = strings::tr(self.lang);
        let text = match btn {
            Btn::Tool(Tool::Move) => t.t_move,
            Btn::Tool(Tool::Pen) => t.t_pencil,
            Btn::Tool(Tool::Arrow) => t.t_arrow,
            Btn::Tool(Tool::Rect) => t.t_rect,
            Btn::Tool(Tool::Fill) => t.t_fill,
            Btn::Tool(Tool::Text) => t.t_text,
            Btn::Color => t.t_color,
            Btn::Scroll => t.t_scroll,
            Btn::Start => t.scroll_start,
            Btn::Undo => t.t_undo,
            Btn::Copy => t.t_copy,
            Btn::CopyText => t.t_copy_text,
            Btn::Save => t.t_save,
            Btn::Close => t.t_close,
        };
        let Some((_, br)) = tb.buttons.iter().find(|(b, _)| *b == btn) else { return };
        let s = tb.scale;
        let fs = 12.5 * s;
        let (tw, th) = gfx.measure(text, fs, false);
        let (pw, ph) = (8.0 * s, 5.0 * s);
        let (w, h) = (tw + pw * 2.0, th + ph * 2.0);
        let mr = self.mons[tb.mon].rect;
        let mut x = br.center().x as f32 - w / 2.0;
        x = x.clamp(mr.l as f32, (mr.r as f32 - w).max(mr.l as f32));
        let below = tb.rect.b as f32 + 6.0 * s;
        let y = if below + h <= mr.b as f32 { below } else { tb.rect.t as f32 - 6.0 * s - h };
        let rr = D2D1_ROUNDED_RECT { rect: rf(x, y, x + w, y + h), radiusX: 6.0 * s, radiusY: 6.0 * s };
        pen.brush.SetColor(&colorf(DEEP, 0.96));
        rt.FillRoundedRectangle(&rr, &pen.brush);
        pen.brush.SetColor(&colorf(BAR_LINE, 1.0));
        rt.DrawRoundedRectangle(&rr, &pen.brush, 1.0, None);
        pen.brush.SetColor(&colorf(ICON, 1.0));
        let fmt = gfx.font(fs, false);
        let wide: Vec<u16> = text.encode_utf16().collect();
        rt.DrawText(&wide, &fmt, &rf(x + pw, y + ph, x + w, y + h), &pen.brush, D2D1_DRAW_TEXT_OPTIONS_NONE, windows::Win32::Graphics::DirectWrite::DWRITE_MEASURING_MODE_NATURAL);
    }

    unsafe fn draw_popover(&self, gfx: &Gfx, rt: &ID2D1RenderTarget, pen: &Pen, p: &Popover, s: f32) {
        let b = &pen.brush;
        let rr = D2D1_ROUNDED_RECT { rect: rect_f(&p.rect), radiusX: 16.0 * s, radiusY: 16.0 * s };
        b.SetColor(&colorf(BAR_BG, 0.98));
        rt.FillRoundedRectangle(&rr, b);
        b.SetColor(&colorf(BAR_LINE, 1.0));
        rt.DrawRoundedRectangle(&D2D1_ROUNDED_RECT { rect: rf(rr.rect.left + 0.5, rr.rect.top + 0.5, rr.rect.right - 0.5, rr.rect.bottom - 0.5), ..rr }, b, 1.0, None);
        for (i, r) in p.swatches.iter().enumerate() {
            let c = r.center();
            let rad = r.w() as f32 / 2.0 - 2.0 * s;
            b.SetColor(&colorf(PRESETS[i], 1.0));
            rt.FillEllipse(&D2D1_ELLIPSE { point: v2(c.x as f32, c.y as f32), radiusX: rad, radiusY: rad }, b);
            b.SetColor(&colorf(SWATCH_LINE, 1.0));
            rt.DrawEllipse(&D2D1_ELLIPSE { point: v2(c.x as f32, c.y as f32), radiusX: rad, radiusY: rad }, b, 1.0, None);
            if PRESETS[i] == self.color {
                b.SetColor(&colorf(0xffffff, 1.0));
                rt.DrawEllipse(&D2D1_ELLIPSE { point: v2(c.x as f32, c.y as f32), radiusX: rad + 3.0 * s, radiusY: rad + 3.0 * s }, b, 2.0 * s.max(1.0), None);
            }
        }
        let t = strings::tr(self.lang);
        for (i, r) in p.strokes.iter().enumerate() {
            let sel = i == self.stroke;
            let rr = D2D1_ROUNDED_RECT { rect: rect_f(r), radiusX: 8.0 * s, radiusY: 8.0 * s };
            b.SetColor(&colorf(if sel { BTN_ACTIVE } else { BTN_HOVER }, 1.0));
            rt.FillRoundedRectangle(&rr, b);
            b.SetColor(&colorf(if sel { ACCENT } else { ICON }, 1.0));
            let cy = r.center().y as f32;
            rt.DrawLine(
                v2(r.l as f32 + 12.0 * s, cy),
                v2(r.r as f32 - 12.0 * s, cy),
                b,
                STROKES[i],
                &pen.round,
            );
            let _ = t.stroke[i];
        }
        // hex field
        let fs = 13.0 * s;
        let hr = p.hex;
        let rr = D2D1_ROUNDED_RECT { rect: rect_f(&hr), radiusX: 8.0 * s, radiusY: 8.0 * s };
        b.SetColor(&colorf(DEEP, 1.0));
        rt.FillRoundedRectangle(&rr, b);
        b.SetColor(&colorf(if self.hex_focus { ACCENT } else { BAR_LINE }, 1.0));
        rt.DrawRoundedRectangle(&rr, b, 1.0, None);
        let fmt = gfx.font(fs, false);
        let label: Vec<u16> = t.hex.encode_utf16().collect();
        b.SetColor(&colorf(MUTED, 1.0));
        let lh = hr.h() as f32;
        let (_, th) = gfx.measure("HEX", fs, false);
        let ty = hr.t as f32 + (lh - th) / 2.0;
        rt.DrawText(&label, &fmt, &rf(hr.l as f32 + 10.0 * s, ty, hr.r as f32, hr.b as f32), b, D2D1_DRAW_TEXT_OPTIONS_NONE, windows::Win32::Graphics::DirectWrite::DWRITE_MEASURING_MODE_NATURAL);
        let shown = if self.hex.is_empty() { String::new() } else { self.hex.clone() };
        let tx = hr.l as f32 + 46.0 * s;
        b.SetColor(&colorf(ICON, 1.0));
        let wide: Vec<u16> = shown.encode_utf16().collect();
        rt.DrawText(&wide, &fmt, &rf(tx, ty, hr.r as f32, hr.b as f32), b, D2D1_DRAW_TEXT_OPTIONS_NONE, windows::Win32::Graphics::DirectWrite::DWRITE_MEASURING_MODE_NATURAL);
        if self.hex_focus {
            let (tw, _) = gfx.measure(&shown, fs, false);
            b.SetColor(&colorf(0xffffff, 1.0));
            rt.DrawLine(v2(tx + tw + 1.0, ty), v2(tx + tw + 1.0, ty + th), b, 1.0, None);
        }
        // colour preview dot
        let dot = D2D1_ELLIPSE { point: v2(hr.r as f32 - 16.0 * s, hr.center().y as f32), radiusX: 7.0 * s, radiusY: 7.0 * s };
        b.SetColor(&colorf(self.color, 1.0));
        rt.FillEllipse(&dot, b);
        b.SetColor(&colorf(SWATCH_LINE, 1.0));
        rt.DrawEllipse(&dot, b, 1.0, None);
    }
}

/// The signature: an L-shaped crop mark standing 6px off each corner of the frame, like the corner
/// brackets of a viewfinder. Accent, 2px, 16px arms, static. They sit outside the selection and never
/// cover the captured pixels.
unsafe fn draw_crop_marks(rt: &ID2D1RenderTarget, b: &ID2D1SolidColorBrush, sel: &Rect, scale: f32) {
    let (arm, w, o) = (16.0 * scale, 2.0 * scale.max(1.0), 6.0 * scale);
    let (l, t, r, bt) = (sel.l as f32 - o, sel.t as f32 - o, sel.r as f32 + o, sel.b as f32 + o);
    b.SetColor(&colorf(ACCENT, 1.0));
    let line = |a: (f32, f32), c: (f32, f32)| rt.DrawLine(v2(a.0, a.1), v2(c.0, c.1), b, w, None);
    for (cx, cy, dx, dy) in [(l, t, 1.0, 1.0), (r, t, -1.0, 1.0), (l, bt, 1.0, -1.0), (r, bt, -1.0, -1.0)] {
        // the arms overshoot the corner by half a stroke so the corner is filled
        line((cx - dx * w / 2.0, cy), (cx + dx * arm, cy));
        line((cx, cy - dy * w / 2.0), (cx, cy + dy * arm));
    }
}

fn resize_cursor(h: Handle) -> PCWSTR {
    match handle_cursor(h) {
        ResizeCursor::NwSe => IDC_SIZENWSE,
        ResizeCursor::NeSw => IDC_SIZENESW,
        ResizeCursor::Ns => IDC_SIZENS,
        ResizeCursor::We => IDC_SIZEWE,
    }
}

// ───────────────────────── toolbar geometry ─────────────────────────

/// Which toolbar: the full one, the viewer's, or the Start / Close pair of a scrolling capture.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kit {
    Full,
    Viewer,
    Start,
}

/// Toolbar layout; `None` is the separator. The scrolling-capture button is left out in the viewer.
fn items(kit: Kit) -> Vec<Option<Btn>> {
    if kit == Kit::Start {
        return vec![Some(Btn::Start), Some(Btn::Close)];
    }
    let viewer = kit == Kit::Viewer;
    let mut v = vec![
        Some(Btn::Tool(Tool::Move)),
        Some(Btn::Tool(Tool::Pen)),
        Some(Btn::Tool(Tool::Arrow)),
        Some(Btn::Tool(Tool::Rect)),
        Some(Btn::Tool(Tool::Fill)),
        Some(Btn::Tool(Tool::Text)),
        Some(Btn::Color),
        None,
    ];
    if !viewer {
        v.push(Some(Btn::Scroll));
    }
    v.extend([Some(Btn::Undo), Some(Btn::Copy)]);
    if !viewer {
        v.push(Some(Btn::CopyText));
    }
    v.extend([Some(Btn::Save), Some(Btn::Close)]);
    v
}

struct Metrics {
    pad: i32,
    btn: i32,
    gap: i32,
    sep: i32,
}

fn metrics(s: f32) -> Metrics {
    let f = |v: f32| (v * s).round() as i32;
    Metrics { pad: f(8.0), btn: f(32.0), gap: f(4.0), sep: f(12.0) }
}

pub fn toolbar_size(s: f32, kit: Kit) -> (i32, i32) {
    let m = metrics(s);
    let mut x = m.pad;
    for it in items(kit) {
        x += if it.is_some() { m.btn + m.gap } else { m.sep };
    }
    (x - m.gap + m.pad, m.btn + 2 * m.pad)
}

fn build_toolbar(r: Rect, mon: usize, s: f32, kit: Kit) -> Toolbar {
    let m = metrics(s);
    let mut x = r.l + m.pad;
    let mut buttons = Vec::new();
    let mut sep_x = 0;
    for it in items(kit) {
        match it {
            Some(b) => {
                buttons.push((b, rect(x, r.t + m.pad, x + m.btn, r.t + m.pad + m.btn)));
                x += m.btn + m.gap;
            }
            None => {
                sep_x = x + m.sep / 2 - m.gap / 2;
                x += m.sep;
            }
        }
    }
    Toolbar { rect: r, mon, scale: s, buttons, sep_x }
}

fn build_popover(tb: &Toolbar, mon: Rect) -> Popover {
    let s = tb.scale;
    let f = |v: f32| (v * s).round() as i32;
    let (pad, sw, g) = (f(12.0), f(28.0), f(8.0));
    let w = pad * 2 + 5 * sw + 4 * g;
    let stroke_h = f(32.0);
    let hex_h = f(32.0);
    let h = pad + 2 * sw + g + f(12.0) + stroke_h + f(12.0) + hex_h + pad;
    let color_btn = tb.buttons.iter().find(|(b, _)| *b == Btn::Color).map(|(_, r)| *r).unwrap_or(tb.rect);
    let x = (color_btn.center().x - w / 2).clamp(mon.l, (mon.r - w).max(mon.l));
    let below = tb.rect.b + f(8.0);
    let y = if below + h <= mon.b { below } else { tb.rect.t - f(8.0) - h };
    let r = rect(x, y, x + w, y + h);
    let mut swatches = Vec::new();
    for i in 0..10 {
        let (cx, cy) = (i % 5, i / 5);
        let l = r.l + pad + cx * (sw + g);
        let t = r.t + pad + cy * (sw + g);
        swatches.push(rect(l, t, l + sw, t + sw));
    }
    let sy = r.t + pad + 2 * sw + g + f(12.0);
    let inner = w - 2 * pad;
    let bw = (inner - 2 * g) / 3;
    let strokes = (0..3).map(|i| rect(r.l + pad + i * (bw + g), sy, r.l + pad + i * (bw + g) + bw, sy + stroke_h)).collect();
    let hy = sy + stroke_h + f(12.0);
    let hex = rect(r.l + pad, hy, r.r - pad, hy + hex_h);
    Popover { rect: r, swatches, strokes, hex }
}

fn clipboard_text() -> Option<String> {
    unsafe {
        OpenClipboard(None).ok()?;
        let result = (|| {
            let h = GetClipboardData(13).ok()?; // CF_UNICODETEXT
            let hg = windows::Win32::Foundation::HGLOBAL(h.0);
            let p = GlobalLock(hg) as *const u16;
            if p.is_null() {
                return None;
            }
            let mut len = 0;
            while *p.add(len) != 0 && len < 10_000 {
                len += 1;
            }
            let text = String::from_utf16_lossy(std::slice::from_raw_parts(p, len));
            let _ = GlobalUnlock(hg);
            Some(text)
        })();
        let _ = CloseClipboard();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toolbar_size_scales_with_dpi() {
        let (w1, h1) = toolbar_size(1.0, Kit::Full);
        let (w2, h2) = toolbar_size(2.0, Kit::Full);
        assert!(w1 > 300 && w1 < 560, "{w1}");
        assert_eq!(h1, 34 + 14);
        assert!((w2 - 2 * w1).abs() <= 6 && (h2 - 2 * h1).abs() <= 2);
    }

    #[test]
    fn toolbar_buttons_fit_inside() {
        let (w, h) = toolbar_size(1.25, Kit::Full);
        let tb = build_toolbar(rect(100, 100, 100 + w, 100 + h), 0, 1.25, Kit::Full);
        assert_eq!(tb.buttons.len(), 13);
        assert_eq!(build_toolbar(rect(0, 0, 900, 90), 0, 1.0, Kit::Viewer).buttons.len(), 11);
        assert_eq!(build_toolbar(rect(0, 0, 900, 90), 0, 1.0, Kit::Start).buttons.len(), 2);
        for (_, r) in &tb.buttons {
            assert!(tb.rect.contains_rect(r));
        }
        // buttons do not overlap
        for pair in tb.buttons.windows(2) {
            assert!(pair[0].1.r <= pair[1].1.l);
        }
    }

    #[test]
    fn popover_stays_inside_the_monitor() {
        let mon = rect(0, 0, 1920, 1080);
        let (w, h) = toolbar_size(1.0, Kit::Full);
        let tb = build_toolbar(rect(1500, 1000, 1500 + w, 1000 + h), 0, 1.0, Kit::Full);
        let p = build_popover(&tb, mon);
        assert!(mon.contains_rect(&p.rect), "{:?}", p.rect);
        assert_eq!(p.swatches.len(), 10);
        for s in &p.swatches {
            assert!(p.rect.contains_rect(s));
        }
    }

    fn pump(ms: u64) {
        let end = Instant::now() + std::time::Duration::from_millis(ms);
        while Instant::now() < end {
            unsafe {
                let mut msg = windows::Win32::UI::WindowsAndMessaging::MSG::default();
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    fn post(hwnd: HWND, msg: u32, mon: Rect, p: Pt) {
        let (x, y) = (p.x - mon.l, p.y - mon.t);
        unsafe {
            let _ = PostMessageW(Some(hwnd), msg, WPARAM(0), LPARAM(((y as isize) << 16) | (x as isize & 0xffff)));
        }
    }

    /// Screenshot of the toolbar over a demo page: `SNAPPER_SHOT_RECT=l,t,r,b` (screen pixels) is the
    /// selection, `SNAPPER_SHOT_OUT` the PNG, `SNAPPER_SHOT_LANG` en, ru or uk. The messages go to the
    /// overlay's own window, and only after it is the foreground window.
    #[test]
    #[ignore]
    fn toolbar_shot() {
        use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
        use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
        let Ok(r) = std::env::var("SNAPPER_SHOT_RECT") else { return };
        let out = std::env::var("SNAPPER_SHOT_OUT").unwrap();
        let lang = Lang::ALL.into_iter().find(|l| l.code() == std::env::var("SNAPPER_SHOT_LANG").unwrap_or("en".into())).unwrap_or(Lang::En);
        let v: Vec<i32> = r.split(',').map(|x| x.trim().parse().unwrap()).collect();
        let sel = rect(v[0], v[1], v[2], v[3]);
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }
        assert!(start(Params { lang, color: 0xff3b30, stroke: 1, scroll: false }));
        pump(900);
        let (hwnd, mon) = SESSION.with(|c| {
            let b = c.borrow();
            let s = b.as_ref().unwrap();
            let m = s.mons.iter().find(|m| m.rect.contains(pt(sel.l, sel.t))).unwrap();
            (m.hwnd, m.rect)
        });
        let mut fg = false;
        for _ in 0..20 {
            if unsafe { GetForegroundWindow() } == hwnd {
                fg = true;
                break;
            }
            pump(100);
        }
        if !fg {
            cancel();
            panic!("the overlay window is not in the foreground: no input is sent");
        }
        post(hwnd, WM_LBUTTONDOWN, mon, pt(sel.l, sel.t));
        pump(80);
        post(hwnd, WM_MOUSEMOVE, mon, pt((sel.l + sel.r) / 2, (sel.t + sel.b) / 2));
        pump(80);
        post(hwnd, WM_MOUSEMOVE, mon, pt(sel.r, sel.b));
        pump(80);
        post(hwnd, WM_LBUTTONUP, mon, pt(sel.r, sel.b));
        pump(400);
        let (tbr, copy_text) = SESSION.with(|c| {
            let b = c.borrow();
            let tb = b.as_ref().unwrap().tb.as_ref().unwrap();
            (tb.rect, tb.buttons.iter().find(|(b, _)| *b == Btn::CopyText).map(|(_, r)| *r).unwrap())
        });
        post(hwnd, WM_MOUSEMOVE, mon, copy_text.center());
        pump(500);
        let vr = capture::virtual_rect(&capture::monitors());
        let cap = capture::capture(vr).expect("capture");
        let area = sel.union(&tbr).inflate(90).intersect(&mon).unwrap();
        let (w, h) = (area.w() as usize, area.h() as usize);
        let mut bgra = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            let off = (area.t - vr.t) as usize * cap.stride() + y * cap.stride() + (area.l - vr.l) as usize * 4;
            bgra.extend_from_slice(unsafe { std::slice::from_raw_parts(cap.ptr().add(off), w * 4) });
        }
        let img = Image { w: w as u32, h: h as u32, bgra };
        std::fs::write(&out, crate::files::encode_png(&img, false).unwrap()).unwrap();
        drop(cap);
        cancel();
    }

    /// Prints where the toolbar buttons land for a selection (`SNAPPER_SHOT_RECT`), for the
    /// end-to-end script that clicks them in a separate Snapper process.
    #[test]
    #[ignore]
    fn toolbar_geometry() {
        use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
        let Ok(r) = std::env::var("SNAPPER_SHOT_RECT") else { return };
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
        let v: Vec<i32> = r.split(',').map(|x| x.trim().parse().unwrap()).collect();
        let sel = rect(v[0], v[1], v[2], v[3]);
        let mons = capture::monitors();
        let places: Vec<Place> = mons.iter().map(|m| Place { rect: m.rect, toolbar: toolbar_size(m.scale(), Kit::Full) }).collect();
        let mon = mons.iter().find(|m| m.rect.contains(pt(sel.r - 1, sel.b - 1))).unwrap();
        let gap = (12.0 * mon.scale()) as i32;
        let (r, i) = place_toolbar(&sel, &places, gap).unwrap();
        let tb = build_toolbar(r, i, mons[i].scale(), Kit::Full);
        for (b, br) in &tb.buttons {
            println!("BTN {:?} {} {}", b, br.center().x, br.center().y);
        }
        println!("MON {} {} {} {}", mons[i].rect.l, mons[i].rect.t, mons[i].rect.r, mons[i].rect.b);
    }
}
