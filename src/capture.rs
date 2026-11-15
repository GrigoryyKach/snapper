//! Screen grabbing: monitor enumeration and a GDI `BitBlt` of the whole virtual desktop into a DIB section.
//! All coordinates are physical pixels of the virtual desktop (the process is Per-Monitor-V2 DPI aware).

use crate::geom::{rect, Rect};
use windows::core::BOOL;
use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, EnumDisplayMonitors, GetDC, GetMonitorInfoW,
    ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, CAPTUREBLT, DIB_RGB_COLORS, HBITMAP, HDC, HMONITOR,
    MONITORINFO, SRCCOPY,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};

#[derive(Clone, Copy, Debug)]
pub struct Monitor {
    pub rect: Rect,
    pub dpi: u32,
}

impl Monitor {
    pub fn scale(&self) -> f32 {
        self.dpi as f32 / 96.0
    }
}

unsafe extern "system" fn enum_proc(hmon: HMONITOR, _hdc: HDC, _rc: *mut RECT, data: LPARAM) -> BOOL {
    let list = &mut *(data.0 as *mut Vec<Monitor>);
    let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
    if GetMonitorInfoW(hmon, &mut mi).as_bool() {
        let (mut dx, mut dy) = (96u32, 96u32);
        let _ = GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        let r = mi.rcMonitor;
        list.push(Monitor { rect: rect(r.left, r.top, r.right, r.bottom), dpi: dx.max(48) });
    }
    BOOL(1)
}

/// All attached monitors in virtual-desktop coordinates.
pub fn monitors() -> Vec<Monitor> {
    let mut list: Vec<Monitor> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(enum_proc), LPARAM(&mut list as *mut _ as isize));
    }
    list
}

pub fn virtual_rect(mons: &[Monitor]) -> Rect {
    mons.iter().map(|m| m.rect).reduce(|a, b| a.union(&b)).unwrap_or(rect(0, 0, 1, 1))
}

/// A frozen copy of the virtual desktop: top-down 32-bit BGRA in a DIB section (alpha forced to 255).
pub struct Capture {
    pub rect: Rect,
    hbmp: HBITMAP,
    ptr: *mut u8,
}

impl Capture {
    pub fn stride(&self) -> usize {
        self.rect.w() as usize * 4
    }
    pub fn ptr(&self) -> *const u8 {
        self.ptr
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.hbmp.into());
        }
    }
}

/// Grabs the whole virtual desktop in one `BitBlt`. Must run before any overlay window is shown.
pub fn capture(r: Rect) -> Option<Capture> {
    let (w, h) = (r.w(), r.h());
    if w <= 0 || h <= 0 {
        return None;
    }
    unsafe {
        let screen = GetDC(None);
        if screen.is_invalid() {
            return None;
        }
        let mem = CreateCompatibleDC(Some(screen));
        let mut bi = BITMAPINFO::default();
        bi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            ..Default::default()
        };
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let hbmp = CreateDIBSection(Some(mem), &bi, DIB_RGB_COLORS, &mut bits, None, 0);
        let Ok(hbmp) = hbmp else {
            let _ = DeleteDC(mem);
            ReleaseDC(None, screen);
            return None;
        };
        let old = SelectObject(mem, hbmp.into());
        let ok = BitBlt(mem, 0, 0, w, h, Some(screen), r.l, r.t, SRCCOPY | CAPTUREBLT).is_ok();
        SelectObject(mem, old);
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        if !ok || bits.is_null() {
            let _ = DeleteObject(hbmp.into());
            return None;
        }
        // BitBlt leaves alpha at 0; make the frozen image opaque.
        let px = std::slice::from_raw_parts_mut(bits as *mut u32, (w as usize) * (h as usize));
        for p in px.iter_mut() {
            *p |= 0xFF00_0000;
        }
        Some(Capture { rect: r, hbmp, ptr: bits as *mut u8 })
    }
}
