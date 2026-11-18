//! Screen grabbing on Linux.
//!
//! - X11: the root window is read with `GetImage` (x11rb, no system libraries); monitors come from RandR.
//! - Wayland: the compositor owns the screen, so the frozen image comes from the
//!   `org.freedesktop.portal.Screenshot` portal (ashpd). It returns one picture of the whole desktop.
//!
//! A [`Shot`] is handed to the overlay process through a small raw file (see [`write_shot`]).

use crate::geom::{rect, Rect};
use std::path::{Path, PathBuf};

/// A frozen copy of the desktop: top-down BGRA, alpha always 255.
pub struct Shot {
    /// Rectangle of the picture in desktop pixels (the origin is the top-left corner of the first monitor group).
    pub rect: Rect,
    pub monitors: Vec<Rect>,
    pub bgra: Vec<u8>,
}

impl Shot {
    pub fn w(&self) -> usize {
        self.rect.w() as usize
    }
    pub fn h(&self) -> usize {
        self.rect.h() as usize
    }
}

/// Grabs the desktop with the backend that fits the session.
pub fn grab() -> Result<Shot, String> {
    if crate::sys::is_wayland() {
        grab_portal()
    } else {
        grab_x11()
    }
}

// ───────────────────────── X11 ─────────────────────────

pub fn grab_x11() -> Result<Shot, String> {
    use x11rb::connection::Connection;
    use x11rb::protocol::randr::ConnectionExt as _;
    use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat, ImageOrder};

    let (conn, screen_num) = x11rb::connect(None).map_err(|e| format!("cannot connect to the X server: {e}"))?;
    let setup = conn.setup();
    let screen = &setup.roots[screen_num];
    let root = screen.root;
    let (sw, sh) = (screen.width_in_pixels as i32, screen.height_in_pixels as i32);

    let mut monitors: Vec<Rect> = Vec::new();
    if let Ok(cookie) = conn.randr_get_monitors(root, true) {
        if let Ok(reply) = cookie.reply() {
            for m in reply.monitors {
                if m.width > 0 && m.height > 0 {
                    monitors.push(rect(m.x as i32, m.y as i32, m.x as i32 + m.width as i32, m.y as i32 + m.height as i32));
                }
            }
        }
    }
    if monitors.is_empty() {
        monitors.push(rect(0, 0, sw, sh));
    }
    let vr = monitors.iter().copied().reduce(|a, b| a.union(&b)).unwrap();
    // Never ask for pixels outside the root window.
    let vr = vr.intersect(&rect(0, 0, sw, sh)).ok_or("the monitors lie outside the screen")?;

    let fmt = setup
        .pixmap_formats
        .iter()
        .find(|f| f.depth == screen.root_depth)
        .ok_or("no pixmap format for the root depth")?;
    if fmt.bits_per_pixel != 32 || setup.image_byte_order != ImageOrder::LSB_FIRST {
        return Err(format!("unsupported X11 pixel format ({} bpp)", fmt.bits_per_pixel));
    }

    let (w, h) = (vr.w() as usize, vr.h() as usize);
    let mut bgra = vec![0u8; w * h * 4];
    let band = (4 * 1024 * 1024 / (w * 4)).clamp(1, h);
    let mut y = 0usize;
    while y < h {
        let rows = band.min(h - y);
        let reply = conn
            .get_image(ImageFormat::Z_PIXMAP, root, vr.l as i16, (vr.t + y as i32) as i16, w as u16, rows as u16, !0)
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| format!("GetImage failed: {e}"))?;
        let want = w * rows * 4;
        if reply.data.len() < want {
            return Err("short GetImage reply".into());
        }
        bgra[y * w * 4..y * w * 4 + want].copy_from_slice(&reply.data[..want]);
        y += rows;
    }
    for px in bgra.chunks_exact_mut(4) {
        px[3] = 255;
    }
    // Monitors relative to the picture.
    let monitors = monitors.iter().filter_map(|m| m.intersect(&vr)).map(|m| m.offset(-vr.l, -vr.t)).collect();
    Ok(Shot { rect: rect(0, 0, vr.w(), vr.h()), monitors, bgra })
}

// ───────────────────────── Wayland: xdg-desktop-portal ─────────────────────────

/// `file:///home/u/Pictures/Screenshot.png` -> `/home/u/Pictures/Screenshot.png` (with %XX decoding).
pub fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // an optional host part ("file://localhost/...") ends at the first slash
    let path = &rest[rest.find('/')?..];
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(PathBuf::from(String::from_utf8(out).ok()?))
}

/// Asks the portal for a full-screen screenshot without a dialog where the portal allows it.
pub fn grab_portal() -> Result<Shot, String> {
    use ashpd::desktop::screenshot::Screenshot;
    let uri = pollster::block_on(async {
        let resp = Screenshot::request().interactive(false).send().await.map_err(|e| e.to_string())?.response().map_err(|e| e.to_string())?;
        Ok::<String, String>(resp.uri().to_string())
    })
    .map_err(|e| format!("screenshot portal: {e}"))?;
    let path = file_uri_to_path(&uri).ok_or_else(|| format!("unexpected portal answer: {uri}"))?;
    let shot = load_png_shot(&path);
    // The portal leaves its file behind in the user's Pictures folder (GNOME) or a temp folder.
    let _ = std::fs::remove_file(&path);
    shot
}

/// Loads an image file as a one-monitor shot.
pub fn load_png_shot(path: &Path) -> Result<Shot, String> {
    let img = image::open(path).map_err(|e| e.to_string())?.to_rgba8();
    let (w, h) = (img.width() as i32, img.height() as i32);
    let mut bgra = img.into_raw();
    for px in bgra.chunks_exact_mut(4) {
        px.swap(0, 2);
        px[3] = 255;
    }
    Ok(Shot { rect: rect(0, 0, w, h), monitors: vec![rect(0, 0, w, h)], bgra })
}

// ───────────────────────── hand-off file ─────────────────────────

const MAGIC: &[u8; 4] = b"SNP1";

fn put(v: &mut Vec<u8>, n: i32) {
    v.extend_from_slice(&n.to_le_bytes());
}

/// Header: magic, width, height, monitor count, monitor rectangles; then the BGRA pixels.
pub fn encode_shot(s: &Shot) -> Vec<u8> {
    let mut v = Vec::with_capacity(16 + s.monitors.len() * 16 + s.bgra.len());
    v.extend_from_slice(MAGIC);
    put(&mut v, s.rect.w());
    put(&mut v, s.rect.h());
    put(&mut v, s.monitors.len() as i32);
    for m in &s.monitors {
        for n in [m.l, m.t, m.r, m.b] {
            put(&mut v, n);
        }
    }
    v.extend_from_slice(&s.bgra);
    v
}

pub fn decode_shot(data: &[u8]) -> Option<Shot> {
    if data.len() < 16 || &data[..4] != MAGIC {
        return None;
    }
    let rd = |i: usize| -> Option<i32> { Some(i32::from_le_bytes(data.get(i..i + 4)?.try_into().ok()?)) };
    let (w, h, n) = (rd(4)?, rd(8)?, rd(12)?);
    if w <= 0 || h <= 0 || !(0..=64).contains(&n) {
        return None;
    }
    let mut monitors = Vec::new();
    let mut at = 16;
    for _ in 0..n {
        monitors.push(rect(rd(at)?, rd(at + 4)?, rd(at + 8)?, rd(at + 12)?));
        at += 16;
    }
    let bgra = data.get(at..)?;
    if bgra.len() != w as usize * h as usize * 4 {
        return None;
    }
    Some(Shot { rect: rect(0, 0, w, h), monitors, bgra: bgra.to_vec() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_to_path() {
        assert_eq!(file_uri_to_path("file:///tmp/a.png"), Some(PathBuf::from("/tmp/a.png")));
        assert_eq!(file_uri_to_path("file:///home/u/My%20Pics/a%2Bb.png"), Some(PathBuf::from("/home/u/My Pics/a+b.png")));
        assert_eq!(file_uri_to_path("file://localhost/x/y.png"), Some(PathBuf::from("/x/y.png")));
        assert_eq!(file_uri_to_path("https://example.com/a.png"), None);
        assert_eq!(file_uri_to_path("file:///a%"), Some(PathBuf::from("/a%")));
    }

    #[test]
    fn shot_roundtrip() {
        let s = Shot { rect: rect(0, 0, 2, 1), monitors: vec![rect(0, 0, 1, 1), rect(1, 0, 2, 1)], bgra: vec![1, 2, 3, 255, 4, 5, 6, 255] };
        let d = decode_shot(&encode_shot(&s)).unwrap();
        assert_eq!((d.w(), d.h()), (2, 1));
        assert_eq!(d.monitors, s.monitors);
        assert_eq!(d.bgra, s.bgra);
        assert!(decode_shot(b"nope").is_none());
        let mut bad = encode_shot(&s);
        bad.pop();
        assert!(decode_shot(&bad).is_none());
    }
}
