//! Exact text from the screen through UI Automation (no recognition errors).
//!
//! Sample points inside the region find the elements under it. An element that offers a text
//! pattern (browser pages, editors, documents) is read line by line: lines inside the region whole,
//! lines cut by the region word by word. The names and values of other elements (labels, buttons,
//! list items, fields) whose boxes lie in the region are laid out by their positions.
//! Password fields are never read.

use crate::geom::Rect;
use crate::textlayout::{self as tl, Word};
use std::time::{Duration, Instant};
use windows::core::Interface;
use windows::Win32::Foundation::POINT;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, SAFEARRAY};
use windows::Win32::System::Ole::{SafeArrayAccessData, SafeArrayDestroy, SafeArrayGetLBound, SafeArrayGetUBound, SafeArrayUnaccessData};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Accessibility::*;
use windows::Win32::UI::WindowsAndMessaging::{GetAncestor, GetClassNameW, WindowFromPoint, GA_ROOT};

pub struct UiaOut {
    pub text: String,
    /// False when the time ran out before everything was read.
    pub complete: bool,
}

/// Rectangles `x, y, w, h` of a text range, as `(l, t, r, b)`.
unsafe fn rects_of(range: &IUIAutomationTextRange) -> Vec<(f64, f64, f64, f64)> {
    let Ok(sa) = range.GetBoundingRectangles() else { return Vec::new() };
    let out = read_doubles(sa);
    out.chunks_exact(4).map(|c| (c[0], c[1], c[0] + c[2], c[1] + c[3])).collect()
}

unsafe fn read_doubles(sa: *mut SAFEARRAY) -> Vec<f64> {
    if sa.is_null() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let (lo, hi) = (SafeArrayGetLBound(sa, 1).unwrap_or(0), SafeArrayGetUBound(sa, 1).unwrap_or(-1));
    if hi >= lo {
        let mut data: *mut std::ffi::c_void = std::ptr::null_mut();
        if SafeArrayAccessData(sa, &mut data).is_ok() && !data.is_null() {
            let n = (hi - lo + 1) as usize;
            out.extend_from_slice(std::slice::from_raw_parts(data as *const f64, n));
            let _ = SafeArrayUnaccessData(sa);
        }
    }
    let _ = SafeArrayDestroy(sa);
    out
}

fn union(r: &[(f64, f64, f64, f64)]) -> Option<(f64, f64, f64, f64)> {
    r.iter().copied().reduce(|a, b| (a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3)))
}

fn overlap(a: (f64, f64, f64, f64), reg: &Rect) -> (f64, f64) {
    let ow = (a.2.min(reg.r as f64) - a.0.max(reg.l as f64)).max(0.0);
    let oh = (a.3.min(reg.b as f64) - a.1.max(reg.t as f64)).max(0.0);
    let w = (a.2 - a.0).max(0.001);
    let h = (a.3 - a.1).max(0.001);
    (ow / w, oh / h)
}

fn clean(s: &str) -> String {
    s.chars().filter(|c| !matches!(c, '\r' | '\n' | '\u{fffc}' | '\u{200b}' | '\u{feff}')).collect::<String>().trim_end().to_string()
}

/// A piece of text with its box.
#[derive(Clone, Debug)]
struct Item {
    rect: (f64, f64, f64, f64),
    text: String,
}

/// Rows of the pieces, in document order: consecutive pieces on the same visual row (table cells,
/// which a page offers as separate lines) become one line, tab separated when the gap is wide.
fn rows(items: &[Item]) -> Vec<(f64, String)> {
    let mut out: Vec<(f64, (f64, f64), String, f64)> = Vec::new(); // top, y-range, text, last right edge
    for it in items {
        let (t, b) = (it.rect.1, it.rect.3);
        let h = (b - t).max(1.0);
        if let Some(last) = out.last_mut() {
            let ov = b.min(last.1 .1) - t.max(last.1 .0);
            if ov >= 0.5 * h.min(last.1 .1 - last.1 .0) && it.rect.0 >= last.3 - 1.0 {
                let gap = it.rect.0 - last.3;
                last.2.push_str(if gap > h * 0.8 { "	" } else { " " });
                last.2.push_str(&it.text);
                last.3 = it.rect.2;
                last.1 = (last.1 .0.min(t), last.1 .1.max(b));
                continue;
            }
        }
        out.push((t, (t, b), it.text.clone(), it.rect.2));
    }
    out.into_iter().map(|r| (r.0, r.2)).collect()
}

/// The part of one text pattern that lies in `reg`: its lines, top to bottom.
unsafe fn pattern_lines(pat: &IUIAutomationTextPattern, reg: &Rect, deadline: Instant, complete: &mut bool, ranges_seen: &mut usize) -> Vec<Item> {
    let mut out: Vec<Item> = Vec::new();
    let mut seen: std::collections::HashSet<(i64, i64, i64, i64, String)> = std::collections::HashSet::new();
    let Ok(visible) = pat.GetVisibleRanges() else { return out };
    let n = visible.Length().unwrap_or(0);
    *ranges_seen += n.max(0) as usize;
    for vi in 0..n {
        let Ok(vr) = visible.GetElement(vi) else { continue };
        // a range that lies completely outside the region is skipped
        if let Some(u) = union(&rects_of(&vr)) {
            if overlap(u, reg).0 == 0.0 || overlap(u, reg).1 == 0.0 {
                continue;
            }
        }
        let Ok(cur) = vr.Clone() else { continue };
        if cur.MoveEndpointByRange(TextPatternRangeEndpoint_End, &vr, TextPatternRangeEndpoint_Start).is_err() || cur.ExpandToEnclosingUnit(TextUnit_Line).is_err() {
            continue;
        }
        let mut guard = 0;
        loop {
            guard += 1;
            if guard > 4000 {
                *complete = false;
                break;
            }
            if Instant::now() > deadline {
                *complete = false;
                return out;
            }
            let rects = rects_of(&cur);
            #[cfg(test)]
            if std::env::var_os("SNAPPER_UIA_TRACE").is_some() {
                println!("line {:?} {:?}", rects, cur.GetText(-1).map(|t| t.to_string()).unwrap_or_default());
            }
            if let Some(u) = union(&rects) {
                let (ow, oh) = overlap(u, reg);
                if oh >= 0.5 && ow > 0.0 {
                    let whole = rects.iter().all(|r| overlap(*r, reg).0 >= 0.999);
                    if whole {
                        if let Ok(t) = cur.GetText(-1) {
                            let t = clean(&t.to_string());
                            if !t.trim().is_empty() && seen.insert((u.0 as i64, u.1 as i64, u.2 as i64, u.3 as i64, t.clone())) {
                                out.push(Item { rect: u, text: t });
                            }
                        }
                    } else if let Some(t) = words_in(&cur, reg, deadline, complete) {
                        if !t.trim().is_empty() && seen.insert((u.0 as i64, u.1 as i64, u.2 as i64, u.3 as i64, t.clone())) {
                            out.push(Item { rect: u, text: t });
                        }
                    }
                }
            }
            match cur.Move(TextUnit_Line, 1) {
                Ok(moved) if moved > 0 => {}
                _ => break,
            }
            // stop after the visible range
            if cur.CompareEndpoints(TextPatternRangeEndpoint_Start, &vr, TextPatternRangeEndpoint_End).map_or(true, |c| c >= 0) {
                break;
            }
        }
    }
    out
}

/// The words of a line whose boxes are mostly inside the region.
unsafe fn words_in(line: &IUIAutomationTextRange, reg: &Rect, deadline: Instant, complete: &mut bool) -> Option<String> {
    let w = line.Clone().ok()?;
    w.MoveEndpointByRange(TextPatternRangeEndpoint_End, line, TextPatternRangeEndpoint_Start).ok()?;
    w.ExpandToEnclosingUnit(TextUnit_Word).ok()?;
    let mut s = String::new();
    for _ in 0..600 {
        if Instant::now() > deadline {
            *complete = false;
            break;
        }
        // past the end of the line?
        if w.CompareEndpoints(TextPatternRangeEndpoint_Start, line, TextPatternRangeEndpoint_End).map_or(true, |c| c >= 0) {
            break;
        }
        if let Some(u) = union(&rects_of(&w)) {
            if overlap(u, reg).0 >= 0.5 {
                if let Ok(t) = w.GetText(-1) {
                    s.push_str(&clean(&t.to_string()));
                    // keep the space that separates words (clean trims it)
                    s.push(' ');
                }
            }
        }
        match w.Move(TextUnit_Word, 1) {
            Ok(m) if m > 0 => {}
            _ => break,
        }
    }
    Some(s.trim().to_string())
}

fn is_chromium(hwnd: windows::Win32::Foundation::HWND) -> bool {
    let mut buf = [0u16; 80];
    let n = unsafe { GetClassNameW(hwnd, &mut buf) } as usize;
    let name = String::from_utf16_lossy(&buf[..n.min(80)]);
    name.starts_with("Chrome_WidgetWin") || name.starts_with("MozillaWindowClass") || name.starts_with("Chrome_RenderWidgetHostHWND")
}

struct Native {
    text: String,
    rect: (f64, f64, f64, f64),
}

unsafe fn variant_text(v: &VARIANT) -> String {
    if v.Anonymous.Anonymous.vt == windows::Win32::System::Variant::VT_BSTR {
        return v.Anonymous.Anonymous.Anonymous.bstrVal.to_string();
    }
    String::new()
}

/// Names and values of the elements of one window that lie in the region.
unsafe fn natives_of(uia: &IUIAutomation, top: &IUIAutomationElement, reg: &Rect, out: &mut Vec<Native>) {
    let Ok(cond) = uia.CreatePropertyCondition(UIA_IsOffscreenPropertyId, &VARIANT::from(false)) else { return };
    let Ok(req) = uia.CreateCacheRequest() else { return };
    for p in [UIA_NamePropertyId, UIA_BoundingRectanglePropertyId, UIA_ControlTypePropertyId, UIA_ValueValuePropertyId, UIA_IsPasswordPropertyId] {
        let _ = req.AddProperty(p);
    }
    let Ok(all) = top.FindAllBuildCache(TreeScope_Subtree, &cond, &req) else { return };
    for i in 0..all.Length().unwrap_or(0) {
        let Ok(e) = all.GetElement(i) else { continue };
        let ct = e.CachedControlType().map(|c| c.0).unwrap_or(0);
        // text, button, hyperlink, list/menu/tab/tree/data item, check/radio, edit, combo, header item, title bar, split button
        if !matches!(ct, 50020 | 50000 | 50005 | 50007 | 50011 | 50019 | 50024 | 50029 | 50002 | 50013 | 50004 | 50003 | 50035 | 50037 | 50031) {
            continue;
        }
        if e.CachedIsPassword().map(|b| b.as_bool()).unwrap_or(false) {
            continue;
        }
        let Ok(rc) = e.CachedBoundingRectangle() else { continue };
        let rect = (rc.left as f64, rc.top as f64, rc.right as f64, rc.bottom as f64);
        if rect.2 - rect.0 < 1.0 || rect.3 - rect.1 < 1.0 {
            continue;
        }
        let (ow, oh) = overlap(rect, reg);
        if ow * oh < 0.5 {
            continue;
        }
        let value = e.GetCachedPropertyValue(UIA_ValueValuePropertyId).map(|v| variant_text(&v)).unwrap_or_default();
        let name = e.CachedName().map(|n| n.to_string()).unwrap_or_default();
        // an edit or combo box shows its value; its name is only the label
        let text = if matches!(ct, 50004 | 50003) && !value.trim().is_empty() { value } else { name };
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if text.is_empty() || text.chars().count() > 1500 {
            continue;
        }
        out.push(Native { text, rect });
    }
}

/// A group or list item names itself after its children: keep the children, and one of two copies.
fn dedupe(mut v: Vec<Native>) -> Vec<Native> {
    let area = |r: (f64, f64, f64, f64)| (r.2 - r.0) * (r.3 - r.1);
    let inside = |a: (f64, f64, f64, f64), b: (f64, f64, f64, f64)| a.0 >= b.0 - 1.0 && a.1 >= b.1 - 1.0 && a.2 <= b.2 + 1.0 && a.3 <= b.3 + 1.0;
    let mut drop = vec![false; v.len()];
    for i in 0..v.len() {
        for j in 0..v.len() {
            if i == j || drop[j] {
                continue;
            }
            let (a, b) = (&v[i], &v[j]);
            if a.text == b.text && inside(a.rect, b.rect) && inside(b.rect, a.rect) {
                // same text, same box: keep the first
                if i > j {
                    drop[i] = true;
                }
            } else if a.text == b.text && inside(a.rect, b.rect) && area(a.rect) < area(b.rect) {
                drop[j] = true;
            } else if inside(b.rect, a.rect) && a.text.contains(&b.text) && a.text != b.text {
                drop[i] = true;
            }
        }
    }
    let mut k = 0;
    v.retain(|_| {
        k += 1;
        !drop[k - 1]
    });
    v
}

/// Reads the text under `reg` (virtual-desktop pixels) until `deadline`.
pub fn read(reg: Rect, deadline: Instant) -> Option<UiaOut> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let uia: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).ok()?;
        let walker = uia.ControlViewWalker().ok()?;
        let nx = (reg.w() / 70).clamp(2, 9);
        let ny = (reg.h() / 50).clamp(2, 7);
        let mut complete = true;
        let mut blocks: Vec<(f64, String)> = Vec::new();
        let mut text_boxes: Vec<(f64, f64, f64, f64)> = Vec::new();
        let mut found: Vec<IUIAutomationElement> = Vec::new();
        let mut tops: Vec<windows::Win32::Foundation::HWND> = Vec::new();
        let mut chromium = false;
        let mut ranges_seen = 0usize;
        for attempt in 0..3 {
            for iy in 0..ny {
                for ix in 0..nx {
                    if Instant::now() > deadline {
                        complete = false;
                        break;
                    }
                    let p = POINT { x: reg.l + (reg.w() * (2 * ix + 1)) / (2 * nx), y: reg.t + (reg.h() * (2 * iy + 1)) / (2 * ny) };
                    let hw = WindowFromPoint(p);
                    let top = GetAncestor(hw, GA_ROOT);
                    if !top.is_invalid() && !tops.contains(&top) {
                        tops.push(top);
                        chromium |= is_chromium(top);
                    }
                    let Ok(mut e) = uia.ElementFromPoint(p) else { continue };
                    // the highest ancestor that still offers a text pattern is the document (a leaf
                    // of a page offers one for its own text only)
                    let mut best: Option<(IUIAutomationElement, IUIAutomationTextPattern)> = None;
                    for _ in 0..24 {
                        let pat = e.GetCurrentPattern(UIA_TextPatternId).ok().and_then(|p| p.cast::<IUIAutomationTextPattern>().ok());
                        if let Some(tp) = pat {
                            best = Some((e.clone(), tp));
                        }
                        match walker.GetParentElement(&e) {
                            Ok(parent) => e = parent,
                            Err(_) => break,
                        }
                    }
                    if let Some((e, tp)) = best {
                        if !found.iter().any(|f| uia.CompareElements(f, &e).map(|b| b.as_bool()).unwrap_or(false)) {
                            let is_pw = e.GetCurrentPropertyValue(UIA_IsPasswordPropertyId).map(|v| v.Anonymous.Anonymous.Anonymous.boolVal.as_bool()).unwrap_or(false);
                            if !is_pw {
                                let lines = pattern_lines(&tp, &reg, deadline, &mut complete, &mut ranges_seen);
                                if let Ok(rc) = e.CurrentBoundingRectangle() {
                                    text_boxes.push((rc.left as f64, rc.top as f64, rc.right as f64, rc.bottom as f64));
                                }
                                blocks.extend(rows(&lines));
                            }
                            found.push(e);
                        }
                    }
                }
            }
            // a browser that has just been asked for accessibility needs a moment to build its tree
            if (!found.is_empty() && ranges_seen > 0) || !chromium || Instant::now() > deadline || attempt >= 2 {
                break;
            }
            // nothing visible yet: start over after a pause
            std::thread::sleep(Duration::from_millis(250));
            found.clear();
            blocks.clear();
            text_boxes.clear();
        }
        // the other elements of every window under the region (not inside a browser: the sample
        // points above are all a page offers cheaply)
        let mut natives: Vec<Native> = Vec::new();
        for top in &tops {
            if Instant::now() > deadline {
                complete = false;
                break;
            }
            if is_chromium(*top) {
                continue;
            }
            if let Ok(el) = uia.ElementFromHandle(*top) {
                natives_of(&uia, &el, &reg, &mut natives);
            }
        }
        natives.retain(|n| !text_boxes.iter().any(|b| {
            let (ow, oh) = overlap(n.rect, &Rect { l: b.0 as i32, t: b.1 as i32, r: b.2 as i32, b: b.3 as i32 });
            ow * oh >= 0.5
        }));
        let natives = dedupe(natives);
        let words: Vec<Word> = natives
            .iter()
            .map(|n| Word { text: n.text.clone(), l: (n.rect.0 - reg.l as f64) as f32, t: (n.rect.1 - reg.t as f64) as f32, r: (n.rect.2 - reg.l as f64) as f32, b: (n.rect.3 - reg.t as f64) as f32 })
            .collect();
        let native_text = tl::layout(&words);

        // lines of a text pattern keep the document order; the native block goes before the first
        // line that starts below its top
        let mut parts: Vec<String> = blocks.iter().map(|b| b.1.clone()).collect();
        if !native_text.is_empty() {
            let top = natives.iter().map(|n| n.rect.1).fold(f64::MAX, f64::min);
            let at = blocks.iter().position(|b| b.0 > top).unwrap_or(parts.len());
            parts.insert(at, native_text);
        }
        let text = parts.join("
");
        Some(UiaOut { text, complete })
    }
}

/// Debug helper for the benchmark: what UI Automation sees at one point.
#[cfg(test)]
pub fn debug_point(x: i32, y: i32) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let uia: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).unwrap();
        let walker = uia.ControlViewWalker().unwrap();
        let hw = WindowFromPoint(POINT { x, y });
        println!("window from point {:?} root {:?} chromium {}", hw, GetAncestor(hw, GA_ROOT), is_chromium(GetAncestor(hw, GA_ROOT)));
        let Ok(mut e) = uia.ElementFromPoint(POINT { x, y }) else { return println!("no element") };
        for depth in 0..12 {
            let name = e.CurrentName().map(|n| n.to_string()).unwrap_or_default();
            let class = e.CurrentClassName().map(|n| n.to_string()).unwrap_or_default();
            let ct = e.CurrentControlType().map(|c| c.0).unwrap_or(0);
            let tp = e.GetCurrentPattern(UIA_TextPatternId).is_ok();
            println!("{depth}: ct {ct} class {class:?} text-pattern {tp} name {:?}", name.chars().take(40).collect::<String>());
            match walker.GetParentElement(&e) {
                Ok(p) => e = p,
                Err(_) => break,
            }
        }
    }
}
