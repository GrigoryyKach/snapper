//! Clipboard output on Linux. X11: `arboard` (it keeps serving the data from this process, which is
//! why the resident process owns the clipboard). Wayland: `wl-copy` when installed (it works on GNOME,
//! where the data-control protocol is missing), otherwise `arboard`.

use crate::files::{encode_png, Image};
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Mutex;

static CLIP: Mutex<Option<arboard::Clipboard>> = Mutex::new(None);

/// BGRA -> RGBA (what arboard wants).
pub fn to_rgba(img: &Image) -> Vec<u8> {
    let mut v = img.bgra.clone();
    for px in v.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    v
}

fn wl_copy(png: &[u8]) -> bool {
    let Ok(mut child) = Command::new("wl-copy").args(["--type", "image/png"]).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()
    else {
        return false;
    };
    let ok = child.stdin.take().is_some_and(|mut s| s.write_all(png).is_ok());
    // wl-copy forks into the background once it owns the selection; do not wait for it to stay alive
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    ok
}

pub fn copy_image(img: &Image) -> Result<(), String> {
    if crate::sys::is_wayland() {
        let png = encode_png(img, true)?;
        if wl_copy(&png) {
            return Ok(());
        }
    }
    let mut guard = CLIP.lock().map_err(|_| "clipboard lock poisoned".to_string())?;
    if guard.is_none() {
        *guard = Some(arboard::Clipboard::new().map_err(|e| e.to_string())?);
    }
    let cb = guard.as_mut().unwrap();
    cb.set_image(arboard::ImageData { width: img.w as usize, height: img.h as usize, bytes: std::borrow::Cow::Owned(to_rgba(img)) })
        .map_err(|e| e.to_string())
}

/// Reads the image back (used by the tests and the self-check).
#[allow(dead_code)]
pub fn read_image() -> Option<Image> {
    let mut guard = CLIP.lock().ok()?;
    if guard.is_none() {
        *guard = arboard::Clipboard::new().ok();
    }
    let got = guard.as_mut()?.get_image().ok()?;
    let mut bgra = got.bytes.into_owned();
    for px in bgra.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    Some(Image { w: got.width as u32, h: got.height as u32, bgra })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgba_swaps_channels() {
        let img = Image { w: 1, h: 1, bgra: vec![1, 2, 3, 255] };
        assert_eq!(to_rgba(&img), vec![3, 2, 1, 255]);
    }
}
