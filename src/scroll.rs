//! Scrolling capture on Windows: a worker thread grabs the chosen area, sends mouse-wheel input to
//! its centre, waits for the content to settle, grabs again, and feeds every grab to the stitcher.
//! A small topmost "HUD" (frames, height, Stop) sits outside the area, so it is never in a frame.
//! Esc stops too.

use crate::capture;
use crate::geom::*;
use crate::i18n::Lang;
use crate::stitch::Frame;
use crate::strings;
use crate::sys::wide;
use std::cell::RefCell;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

pub use crate::scrollcore::{Note, Shared};
use crate::scrollcore::{self, Io, Outcome};

fn grab(r: Rect) -> Option<Frame> {
    let c = capture::capture(r)?;
    let (w, h) = (r.w() as usize, r.h() as usize);
    let bgra = unsafe { std::slice::from_raw_parts(c.ptr(), w * h * 4) }.to_vec();
    Some(Frame { w, h, bgra })
}

fn esc_down() -> bool {
    unsafe { (GetAsyncKeyState(VK_ESCAPE.0 as i32) as u16 & 0x8000) != 0 }
}

/// Sends `notches` wheel-down notches to the window under `at`.
fn wheel_down(at: Pt, notches: i32) {
    unsafe {
        let _ = SetCursorPos(at.x, at.y);
    }
    std::thread::sleep(Duration::from_millis(20));
    for _ in 0..notches {
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 { mi: MOUSEINPUT { dx: 0, dy: 0, mouseData: (-120i32) as u32, dwFlags: MOUSEEVENTF_WHEEL, time: 0, dwExtraInfo: 0 } },
        };
        unsafe {
            SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
        }
        std::thread::sleep(Duration::from_millis(12));
    }
}

fn work(r: Rect, max_rows: usize, sh: &Shared) -> Outcome {
    // let the overlay window disappear from the screen
    std::thread::sleep(Duration::from_millis(280));
    // Scroll with the pointer near the right edge: that is usually a scrollbar or a margin, not a link
    // whose hover tooltip would end up in the frames.
    let centre = pt(r.r - (r.w() / 20).clamp(4, 10), r.t + r.h() / 2);
    let mut saved = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut saved);
    }
    let mut grab_it = || grab(r);
    let mut wheel = |n: i32| wheel_down(centre, n);
    let esc = esc_down;
    let out = scrollcore::run(max_rows, sh, &mut Io { grab: &mut grab_it, wheel: &mut wheel, esc: &esc });
    unsafe {
        let _ = SetCursorPos(saved.x, saved.y);
    }
    out
}

/// Starts the worker. `notify` receives `msg` when it is finished.
pub fn spawn(r: Rect, max_rows: usize, notify: HWND, msg: u32) -> Arc<Shared> {
    let sh = Arc::new(Shared::new());
    let sh2 = sh.clone();
    let hwnd = notify.0 as isize;
    std::thread::spawn(move || {
        let out = work(r, max_rows, &sh2);
        sh2.finish(out);
        unsafe {
            let _ = PostMessageW(Some(HWND(hwnd as *mut _)), msg, WPARAM(0), LPARAM(0));
        }
    });
    sh
}

// ───────────────────────── the HUD ─────────────────────────

const HUD_CLASS: PCWSTR = w!("SnapperScrollHud");
const HUD_TIMER: usize = 1;

struct Hud {
    shared: Arc<Shared>,
    lang: Lang,
    scale: f32,
    stop_btn: RECT,
}

thread_local! {
    static HUD: RefCell<Option<(HWND, Hud)>> = const { RefCell::new(None) };
}

/// Finds a place for the HUD that does not touch `region` (it must never be in a frame).
fn hud_place(region: Rect, w: i32, h: i32) -> Option<Rect> {
    let mons = capture::monitors();
    let avoid = region.inflate(6);
    let mut cands: Vec<Rect> = Vec::new();
    for m in &mons {
        let mr = m.rect;
        let cx = (region.center().x - w / 2).clamp(mr.l, (mr.r - w).max(mr.l));
        let cy = (region.center().y - h / 2).clamp(mr.t, (mr.b - h).max(mr.t));
        cands.push(rect(cx, region.b + 10, cx + w, region.b + 10 + h));
        cands.push(rect(cx, region.t - 10 - h, cx + w, region.t - 10));
        cands.push(rect(region.r + 10, cy, region.r + 10 + w, cy + h));
        cands.push(rect(region.l - 10 - w, cy, region.l - 10, cy + h));
        cands.push(rect(mr.l + 12, mr.t + 12, mr.l + 12 + w, mr.t + 12 + h));
        cands.push(rect(mr.r - 12 - w, mr.t + 12, mr.r - 12, mr.t + 12 + h));
        cands.push(rect(mr.l + 12, mr.b - 12 - h, mr.l + 12 + w, mr.b - 12));
        cands.push(rect(mr.r - 12 - w, mr.b - 12 - h, mr.r - 12, mr.b - 12));
    }
    cands.into_iter().find(|c| mons.iter().any(|m| m.rect.contains_rect(c)) && c.intersect(&avoid).is_none())
}

pub fn show_hud(shared: Arc<Shared>, lang: Lang, region: Rect) {
    close_hud();
    let mons = capture::monitors();
    let scale = mons.iter().find(|m| m.rect.contains(region.center())).or(mons.first()).map_or(1.0, |m| m.scale());
    let (w, h) = ((330.0 * scale) as i32, (44.0 * scale) as i32);
    let Some(place) = hud_place(region, w, h) else { return };
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(hud_proc),
            hInstance: hinst.into(),
            lpszClassName: HUD_CLASS,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            HUD_CLASS,
            w!("Snapper"),
            WS_POPUP,
            place.l,
            place.t,
            w,
            h,
            None,
            None,
            Some(hinst.into()),
            None,
        ) else {
            return;
        };
        let bw = (96.0 * scale) as i32;
        let m = (7.0 * scale) as i32;
        let stop_btn = RECT { left: w - bw - m, top: m, right: w - m, bottom: h - m };
        HUD.with(|c| *c.borrow_mut() = Some((hwnd, Hud { shared, lang, scale, stop_btn })));
        SetTimer(Some(hwnd), HUD_TIMER, 120, None);
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), place.l, place.t, w, h, SWP_NOACTIVATE | SWP_SHOWWINDOW);
    }
}

pub fn close_hud() {
    if let Some((hwnd, _)) = HUD.with(|c| c.borrow_mut().take()) {
        unsafe {
            let _ = KillTimer(Some(hwnd), HUD_TIMER);
            let _ = DestroyWindow(hwnd);
        }
    }
}

unsafe extern "system" fn hud_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
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
            HUD.with(|c| {
                if let Some((_, h)) = c.borrow().as_ref() {
                    let b = h.stop_btn;
                    if x >= b.left && x < b.right && y >= b.top && y < b.bottom {
                        h.shared.stop.store(true, Ordering::Relaxed);
                    }
                }
            });
            return LRESULT(0);
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let dc = BeginPaint(hwnd, &mut ps);
            HUD.with(|c| {
                if let Some((_, h)) = c.borrow().as_ref() {
                    paint_hud(dc, hwnd, h);
                }
            });
            let _ = EndPaint(hwnd, &ps);
            return LRESULT(0);
        }
        _ => {}
    }
    DefWindowProcW(hwnd, msg, wp, lp)
}

unsafe fn paint_hud(dc: HDC, hwnd: HWND, h: &Hud) {
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let bg = CreateSolidBrush(COLORREF(0x002b_1e1b));
    FillRect(dc, &rc, bg);
    let _ = DeleteObject(bg.into());
    let accent = CreateSolidBrush(COLORREF(0x00ff_6b4f));
    FillRect(dc, &h.stop_btn, accent);
    let _ = DeleteObject(accent.into());
    let font = CreateFontW(-((14.0 * h.scale) as i32), 0, 0, 0, 600, 0, 0, 0, FONT_CHARSET(1), FONT_OUTPUT_PRECISION(0), FONT_CLIP_PRECISION(0), FONT_QUALITY(5), 0, w!("Segoe UI"));
    let old = SelectObject(dc, font.into());
    SetBkMode(dc, TRANSPARENT);
    let t = strings::tr(h.lang);
    let text = t.scroll_progress.replace("{n}", &h.shared.frames.load(Ordering::Relaxed).to_string()).replace("{h}", &h.shared.rows.load(Ordering::Relaxed).to_string());
    let mut w = wide(&text);
    w.pop();
    SetTextColor(dc, COLORREF(0x00f7_e8e4));
    let mut tr = RECT { left: (14.0 * h.scale) as i32, top: 0, right: h.stop_btn.left - 6, bottom: rc.bottom };
    DrawTextW(dc, &mut w, &mut tr, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
    let mut sw = wide(t.scroll_stop);
    sw.pop();
    SetTextColor(dc, COLORREF(0x00ff_ffff));
    let mut br = h.stop_btn;
    DrawTextW(dc, &mut sw, &mut br, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    SelectObject(dc, old);
    let _ = DeleteObject(font.into());
}
