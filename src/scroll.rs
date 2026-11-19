//! Scrolling capture on Windows, driven by the user: a worker thread samples the chosen area while
//! the user scrolls and clicks as usual, and feeds the stable frames to `manual::Manual`.
//!
//! The screen stays interactive. Two small windows are the only things Snapper puts on it, and both
//! lie outside the area, so they are never in a frame:
//! - the frame: a topmost, layered, click-through window that draws crop marks around the area
//!   (`WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW`: mouse, wheel and keys
//!   go to the app underneath, inside and outside the area);
//! - the control bar: a non-activating window (`WS_EX_NOACTIVATE`, `MA_NOACTIVATE`) with the live
//!   height, Done and Cancel, so a click on it never takes focus from the page.

use crate::capture;
use crate::files::Image;
use crate::geom::*;
use crate::i18n::Lang;
use crate::manual::{Ev, Manual};
use crate::stitch::Frame;
use crate::strings;
use crate::sys::wide;
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Note {
    /// Finished normally.
    None,
    /// Cut at the height limit.
    Limit,
    /// The area never changed.
    Nothing,
    /// Grabbing the screen failed.
    Failed,
}

pub struct Outcome {
    pub image: Option<Image>,
    pub note: Note,
}

pub struct Shared {
    /// Done: finish and open the result.
    pub done_req: AtomicBool,
    /// Cancel: discard everything.
    pub cancel_req: AtomicBool,
    pub frames: AtomicUsize,
    pub rows: AtomicUsize,
    pub pages: AtomicUsize,
    pub finished: AtomicBool,
    pub outcome: Mutex<Option<Outcome>>,
}

fn grab(r: Rect) -> Option<Frame> {
    let c = capture::capture(r)?;
    let (w, h) = (r.w() as usize, r.h() as usize);
    let bgra = unsafe { std::slice::from_raw_parts(c.ptr(), w * h * 4) }.to_vec();
    Some(Frame { w, h, bgra })
}

/// A cheap hash of a whole frame, to skip identical samples.
fn frame_hash(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for c in b.chunks_exact(8) {
        h = (h ^ u64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]])).wrapping_mul(0x0000_0100_0000_01b3);
        h ^= h >> 29;
    }
    h
}

fn work(area: Rect, max_rows: usize, sh: &Shared) -> Outcome {
    // let the selection overlay disappear from the screen
    std::thread::sleep(Duration::from_millis(260));
    let Some(first) = grab(area) else { return Outcome { image: None, note: Note::Failed } };
    let h = first.h;
    let mut last_hash = frame_hash(&first.bgra);
    let mut committed = last_hash;
    let mut m = Manual::new(first, max_rows);
    sh.frames.store(1, Ordering::Relaxed);
    sh.rows.store(m.rows(), Ordering::Relaxed);
    sh.pages.store(1, Ordering::Relaxed);
    let mut stable = 1usize;
    let mut moving_since: Option<Instant> = None;
    let mut fails = 0;
    let mut note = Note::None;
    loop {
        if sh.cancel_req.load(Ordering::Relaxed) {
            return Outcome { image: None, note: Note::None };
        }
        if sh.done_req.load(Ordering::Relaxed) {
            break;
        }
        std::thread::sleep(Duration::from_millis(70));
        let Some(f) = grab(area) else {
            fails += 1;
            if fails > 30 {
                note = Note::Failed;
                break;
            }
            continue;
        };
        fails = 0;
        let hash = frame_hash(&f.bgra);
        if hash == last_hash {
            stable += 1;
        } else {
            stable = 1;
            last_hash = hash;
            moving_since.get_or_insert_with(Instant::now);
        }
        if hash == committed {
            moving_since = None;
            continue;
        }
        // a long motion without a pause is let through, so fast scrolling does not lose overlap
        let force = moving_since.is_some_and(|t| t.elapsed() > Duration::from_millis(600));
        let ev = m.push(f, stable, force);
        crate::sys::log(&format!("scroll: {ev:?}, stable {stable}, rows {}", m.rows()));
        match ev {
            Ev::Wait => {}
            Ev::Limit => {
                note = Note::Limit;
                break;
            }
            _ => {
                committed = hash;
                moving_since = None;
            }
        }
        sh.frames.store(m.frames(), Ordering::Relaxed);
        sh.rows.store(m.rows(), Ordering::Relaxed);
        sh.pages.store(m.pages(), Ordering::Relaxed);
    }
    let frames = m.frames();
    let out = m.finish();
    if frames <= 1 && out.h <= h {
        return Outcome { image: None, note: if note == Note::None { Note::Nothing } else { note } };
    }
    Outcome { image: Some(Image { w: out.w as u32, h: out.h as u32, bgra: out.bgra }), note }
}

/// Starts the worker. `notify` receives `msg` when it is finished.
pub fn spawn(r: Rect, max_rows: usize, notify: HWND, msg: u32) -> Arc<Shared> {
    let sh = Arc::new(Shared {
        done_req: AtomicBool::new(false),
        cancel_req: AtomicBool::new(false),
        frames: AtomicUsize::new(0),
        rows: AtomicUsize::new(0),
        pages: AtomicUsize::new(1),
        finished: AtomicBool::new(false),
        outcome: Mutex::new(None),
    });
    let sh2 = sh.clone();
    let hwnd = notify.0 as isize;
    std::thread::spawn(move || {
        let out = work(r, max_rows, &sh2);
        *sh2.outcome.lock().unwrap() = Some(out);
        sh2.finished.store(true, Ordering::Release);
        unsafe {
            let _ = PostMessageW(Some(HWND(hwnd as *mut _)), msg, WPARAM(0), LPARAM(0));
        }
    });
    sh
}

// ───────────────────────── look ─────────────────────────

const ACCENT: u32 = 0x35a7ff;
const BAR_BG: u32 = 0x19191b;
const BAR_LINE: u32 = 0x3a3a3d;
const DEEP: u32 = 0x0f0f10;
const TEXT: u32 = 0xededed;
const TEXT2: u32 = 0xa1a1a6;
const BTN: u32 = 0x232325;

fn cr(rgb: u32) -> COLORREF {
    COLORREF(((rgb & 0xff) << 16) | (rgb & 0xff00) | (rgb >> 16))
}

// ───────────────────────── the frame (click-through) ─────────────────────────

const FRAME_CLASS: PCWSTR = w!("SnapperScrollFrame");
const BAR_CLASS: PCWSTR = w!("SnapperScrollBar");
const BAR_TIMER: usize = 1;

struct Ui {
    frame: HWND,
    bar: Option<(HWND, Bar)>,
}

struct Bar {
    shared: Arc<Shared>,
    lang: Lang,
    scale: f32,
    done_btn: RECT,
    cancel_btn: RECT,
    done_key: String,
    cancel_key: String,
}

thread_local! {
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

fn monitor_scale(r: Rect) -> f32 {
    let mons = capture::monitors();
    mons.iter().find(|m| m.rect.contains(r.center())).or(mons.first()).map_or(1.0, |m| m.scale())
}

/// Crop marks in the suite's accent, 6 px off each corner of the area, plus a faint outline. Drawn
/// into a premultiplied BGRA bitmap that the layered window shows as it is.
fn frame_pixels(wpx: i32, hpx: i32, area_in_win: Rect, scale: f32) -> Vec<u32> {
    let mut px = vec![0u32; (wpx * hpx) as usize];
    let mut fill = |l: i32, t: i32, r: i32, b: i32, rgb: u32, a: u32| {
        let (rr, gg, bb) = ((rgb >> 16) & 0xff, (rgb >> 8) & 0xff, rgb & 0xff);
        let v = (a << 24) | ((rr * a / 255) << 16) | ((gg * a / 255) << 8) | (bb * a / 255);
        for y in t.max(0)..b.min(hpx) {
            for x in l.max(0)..r.min(wpx) {
                px[(y * wpx + x) as usize] = v;
            }
        }
    };
    let s = scale.max(1.0);
    let (o, arm, wd) = ((6.0 * s).round() as i32, (16.0 * s).round() as i32, (2.0 * s).round().max(2.0) as i32);
    let a = area_in_win;
    // faint outline, 3 px outside the area
    let g = (3.0 * s).round() as i32;
    fill(a.l - g - 1, a.t - g - 1, a.r + g + 1, a.t - g, ACCENT, 90);
    fill(a.l - g - 1, a.b + g, a.r + g + 1, a.b + g + 1, ACCENT, 90);
    fill(a.l - g - 1, a.t - g, a.l - g, a.b + g, ACCENT, 90);
    fill(a.r + g, a.t - g, a.r + g + 1, a.b + g, ACCENT, 90);
    // the corner marks
    let (l, t, r, b) = (a.l - o, a.t - o, a.r + o, a.b + o);
    for (cx, cy, dx, dy) in [(l, t, 1, 1), (r, t, -1, 1), (l, b, 1, -1), (r, b, -1, -1)] {
        let (hx0, hx1) = if dx > 0 { (cx - wd / 2, cx + arm) } else { (cx - arm, cx + wd / 2) };
        fill(hx0, cy - wd / 2, hx1, cy - wd / 2 + wd, ACCENT, 255);
        let (vy0, vy1) = if dy > 0 { (cy - wd / 2, cy + arm) } else { (cy - arm, cy + wd / 2) };
        fill(cx - wd / 2, vy0, cx - wd / 2 + wd, vy1, ACCENT, 255);
    }
    px
}

unsafe extern "system" fn frame_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    DefWindowProcW(hwnd, msg, wp, lp)
}

unsafe fn show_frame(area: Rect, scale: f32) -> Option<HWND> {
    let margin = (14.0 * scale.max(1.0)).round() as i32;
    let (x, y, wpx, hpx) = (area.l - margin, area.t - margin, area.w() + 2 * margin, area.h() + 2 * margin);
    let hinst = GetModuleHandleW(None).unwrap_or_default();
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(frame_proc),
        hInstance: hinst.into(),
        lpszClassName: FRAME_CLASS,
        ..Default::default()
    };
    RegisterClassExW(&wc);
    let hwnd = CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
        FRAME_CLASS,
        w!("Snapper frame"),
        WS_POPUP,
        x,
        y,
        wpx,
        hpx,
        None,
        None,
        Some(hinst.into()),
        None,
    )
    .ok()?;
    let mut bi = BITMAPINFO::default();
    bi.bmiHeader = BITMAPINFOHEADER { biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32, biWidth: wpx, biHeight: -hpx, biPlanes: 1, biBitCount: 32, ..Default::default() };
    let screen = GetDC(None);
    let mem = CreateCompatibleDC(Some(screen));
    let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
    let bmp = CreateDIBSection(Some(mem), &bi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
    let old = SelectObject(mem, bmp.into());
    let px = frame_pixels(wpx, hpx, rect(margin, margin, margin + area.w(), margin + area.h()), scale);
    std::ptr::copy_nonoverlapping(px.as_ptr(), bits as *mut u32, px.len());
    let blend = BLENDFUNCTION { BlendOp: 0, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: 1 };
    let (dst, size, src) = (POINT { x, y }, SIZE { cx: wpx, cy: hpx }, POINT { x: 0, y: 0 });
    let _ = UpdateLayeredWindow(hwnd, Some(screen), Some(&dst), Some(&size), Some(mem), Some(&src), COLORREF(0), Some(&blend), ULW_ALPHA);
    SelectObject(mem, old);
    let _ = DeleteObject(bmp.into());
    let _ = DeleteDC(mem);
    ReleaseDC(None, screen);
    let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, wpx, hpx, SWP_NOACTIVATE | SWP_SHOWWINDOW);
    Some(hwnd)
}

// ───────────────────────── the control bar ─────────────────────────

unsafe extern "system" fn work_proc(hmon: HMONITOR, _hdc: HDC, _rc: *mut RECT, data: LPARAM) -> windows::core::BOOL {
    let list = &mut *(data.0 as *mut Vec<Rect>);
    let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
    if GetMonitorInfoW(hmon, &mut mi).as_bool() {
        let r = mi.rcWork;
        list.push(rect(r.left, r.top, r.right, r.bottom));
    }
    windows::core::BOOL(1)
}

/// Monitor areas without the taskbar.
fn work_areas() -> Vec<Rect> {
    let mut list: Vec<Rect> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(work_proc), LPARAM(&mut list as *mut _ as isize));
    }
    list
}

/// Finds a place for the bar that does not touch `region` (it must never be in a frame).
fn bar_place(region: Rect, w: i32, h: i32) -> Option<Rect> {
    let mons: Vec<capture::Monitor> = work_areas().into_iter().map(|r| capture::Monitor { rect: r, dpi: 96 }).collect();
    let avoid = region.inflate(30);
    let mut cands: Vec<Rect> = Vec::new();
    for m in &mons {
        let mr = m.rect;
        let cx = (region.center().x - w / 2).clamp(mr.l, (mr.r - w).max(mr.l));
        let cy = (region.center().y - h / 2).clamp(mr.t, (mr.b - h).max(mr.t));
        cands.push(rect(cx, region.b + 36, cx + w, region.b + 36 + h));
        cands.push(rect(cx, region.t - 36 - h, cx + w, region.t - 36));
        cands.push(rect(region.r + 36, cy, region.r + 36 + w, cy + h));
        cands.push(rect(region.l - 36 - w, cy, region.l - 36, cy + h));
        cands.push(rect(mr.l + 16, mr.t + 16, mr.l + 16 + w, mr.t + 16 + h));
        cands.push(rect(mr.r - 16 - w, mr.t + 16, mr.r - 16, mr.t + 16 + h));
        cands.push(rect(mr.l + 16, mr.b - 16 - h, mr.l + 16 + w, mr.b - 16));
        cands.push(rect(mr.r - 16 - w, mr.b - 16 - h, mr.r - 16, mr.b - 16));
    }
    cands.into_iter().find(|c| mons.iter().any(|m| m.rect.contains_rect(c)) && c.intersect(&avoid).is_none())
}

/// Shows the frame and the control bar. `done_key` / `cancel_key` are the temporary hotkeys, shown as keycaps.
pub fn show_ui(shared: Arc<Shared>, lang: Lang, area: Rect, done_key: &str, cancel_key: &str) {
    close_ui();
    let scale = monitor_scale(area);
    let frame = unsafe { show_frame(area, scale) };
    let Some(frame) = frame else { return };
    let (bw, bh) = ((600.0 * scale) as i32, (104.0 * scale) as i32);
    let mut bar = None;
    if let Some(place) = bar_place(area, bw, bh) {
        unsafe {
            let hinst = GetModuleHandleW(None).unwrap_or_default();
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(bar_proc),
                hInstance: hinst.into(),
                lpszClassName: BAR_CLASS,
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                ..Default::default()
            };
            RegisterClassExW(&wc);
            if let Ok(hwnd) = CreateWindowExW(WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE, BAR_CLASS, w!("Snapper"), WS_POPUP, place.l, place.t, bw, bh, None, None, Some(hinst.into()), None) {
                let (m, bwid, bhei) = ((14.0 * scale) as i32, (96.0 * scale) as i32, (32.0 * scale) as i32);
                let by = m;
                let done_btn = RECT { left: bw - m - bwid, top: by, right: bw - m, bottom: by + bhei };
                let cancel_btn = RECT { left: done_btn.left - 8 - bwid, top: by, right: done_btn.left - 8, bottom: by + bhei };
                bar = Some((hwnd, Bar { shared, lang, scale, done_btn, cancel_btn, done_key: done_key.to_string(), cancel_key: cancel_key.to_string() }));
                let rgn = CreateRoundRectRgn(0, 0, bw + 1, bh + 1, (24.0 * scale) as i32, (24.0 * scale) as i32);
                SetWindowRgn(hwnd, Some(rgn), true);
                SetTimer(Some(hwnd), BAR_TIMER, 120, None);
                let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), place.l, place.t, bw, bh, SWP_NOACTIVATE | SWP_SHOWWINDOW);
            }
        }
    }
    UI.with(|c| *c.borrow_mut() = Some(Ui { frame, bar }));
}

pub fn close_ui() {
    if let Some(ui) = UI.with(|c| c.borrow_mut().take()) {
        unsafe {
            if let Some((h, _)) = &ui.bar {
                let _ = KillTimer(Some(*h), BAR_TIMER);
                let _ = DestroyWindow(*h);
            }
            let _ = DestroyWindow(ui.frame);
        }
    }
}

/// "4230" -> "4 230".
fn spaced(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push('\u{a0}');
        }
        out.push(c);
    }
    out
}

unsafe extern "system" fn bar_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        WM_ERASEBKGND => return LRESULT(1),
        WM_TIMER => {
            let _ = InvalidateRect(Some(hwnd), None, false);
            return LRESULT(0);
        }
        WM_LBUTTONUP => {
            let x = (lp.0 & 0xffff) as i16 as i32;
            let y = ((lp.0 >> 16) & 0xffff) as i16 as i32;
            UI.with(|c| {
                if let Some(Ui { bar: Some((_, b)), .. }) = c.borrow().as_ref() {
                    let hit = |r: &RECT| x >= r.left && x < r.right && y >= r.top && y < r.bottom;
                    if hit(&b.done_btn) {
                        b.shared.done_req.store(true, Ordering::Relaxed);
                    } else if hit(&b.cancel_btn) {
                        b.shared.cancel_req.store(true, Ordering::Relaxed);
                    }
                }
            });
            return LRESULT(0);
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let dc = BeginPaint(hwnd, &mut ps);
            UI.with(|c| {
                if let Some(Ui { bar: Some((_, b)), .. }) = c.borrow().as_ref() {
                    paint_bar(dc, hwnd, b);
                }
            });
            let _ = EndPaint(hwnd, &ps);
            return LRESULT(0);
        }
        _ => {}
    }
    DefWindowProcW(hwnd, msg, wp, lp)
}

unsafe fn text_at(dc: HDC, font_face: PCWSTR, px: i32, weight: i32, rgb: u32, text: &str, rc: &mut RECT, flags: DRAW_TEXT_FORMAT) {
    let font = CreateFontW(-px, 0, 0, 0, weight, 0, 0, 0, FONT_CHARSET(1), FONT_OUTPUT_PRECISION(0), FONT_CLIP_PRECISION(0), FONT_QUALITY(5), 0, font_face);
    let old = SelectObject(dc, font.into());
    SetTextColor(dc, cr(rgb));
    let mut w = wide(text);
    w.pop();
    DrawTextW(dc, &mut w, rc, flags);
    SelectObject(dc, old);
    let _ = DeleteObject(font.into());
}

unsafe fn paint_bar(dc: HDC, hwnd: HWND, b: &Bar) {
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let s = b.scale;
    let px = |v: f32| (v * s).round() as i32;
    // body and 1 px edge
    let bg = CreateSolidBrush(cr(BAR_BG));
    FillRect(dc, &rc, bg);
    let _ = DeleteObject(bg.into());
    let edge = CreateSolidBrush(cr(BAR_LINE));
    FrameRect(dc, &rc, edge);
    let _ = DeleteObject(edge.into());
    SetBkMode(dc, TRANSPARENT);
    let t = strings::tr(b.lang);
    let rows = b.shared.rows.load(Ordering::Relaxed);
    let frames = b.shared.frames.load(Ordering::Relaxed);
    let pages = b.shared.pages.load(Ordering::Relaxed);
    let stats = if pages > 1 {
        t.scroll_bar_pages.replace("{h}", &spaced(rows)).replace("{n}", &frames.to_string()).replace("{p}", &pages.to_string())
    } else {
        t.scroll_bar.replace("{h}", &spaced(rows)).replace("{n}", &frames.to_string())
    };
    let mut r1 = RECT { left: px(16.0), top: px(12.0), right: b.cancel_btn.left - px(10.0), bottom: px(34.0) };
    text_at(dc, w!("Consolas"), px(15.0), 400, TEXT, &stats, &mut r1, DT_LEFT | DT_SINGLELINE | DT_NOPREFIX);
    let mut r2 = RECT { left: px(16.0), top: px(36.0), right: b.cancel_btn.left - px(10.0), bottom: px(56.0) };
    text_at(dc, w!("Segoe UI"), px(12.0), 400, TEXT2, t.scroll_hint, &mut r2, DT_LEFT | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS);
    // keycaps for the temporary hotkeys, left of the buttons
    let mut parts: Vec<String> = Vec::new();
    if !b.done_key.is_empty() {
        parts.push(format!("{}  {}", b.done_key, t.scroll_done));
    }
    if !b.cancel_key.is_empty() {
        parts.push(format!("{}  {}", b.cancel_key, t.scroll_cancel));
    }
    let caps = parts.join("    ");
    // a hairline, then the keycaps
    let line = RECT { left: px(14.0), top: px(66.0), right: rc.right - px(14.0), bottom: px(67.0) };
    let lb = CreateSolidBrush(cr(BAR_LINE));
    FillRect(dc, &line, lb);
    let _ = DeleteObject(lb.into());
    let mut r3 = RECT { left: px(16.0), top: px(72.0), right: rc.right - px(14.0), bottom: px(94.0) };
    text_at(dc, w!("Consolas"), px(12.0), 400, TEXT2, &caps, &mut r3, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS);
    // buttons
    let null_pen = CreatePen(PS_NULL, 0, COLORREF(0));
    let old_pen = SelectObject(dc, null_pen.into());
    let accent = CreateSolidBrush(cr(ACCENT));
    let old_b = SelectObject(dc, accent.into());
    let rad = px(16.0);
    let d = b.done_btn;
    let _ = RoundRect(dc, d.left, d.top, d.right, d.bottom, rad, rad);
    let ghost = CreateSolidBrush(cr(BTN));
    SelectObject(dc, ghost.into());
    let c = b.cancel_btn;
    let _ = RoundRect(dc, c.left, c.top, c.right, c.bottom, rad, rad);
    SelectObject(dc, old_b);
    SelectObject(dc, old_pen);
    let _ = DeleteObject(accent.into());
    let _ = DeleteObject(ghost.into());
    let _ = DeleteObject(null_pen.into());
    let mut dr = d;
    text_at(dc, w!("Segoe UI"), px(13.0), 600, DEEP, t.scroll_done, &mut dr, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
    let mut cr2 = c;
    text_at(dc, w!("Segoe UI"), px(13.0), 400, TEXT, t.scroll_cancel, &mut cr2, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
}
