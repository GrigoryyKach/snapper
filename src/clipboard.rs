//! Clipboard output: the image goes out both as `CF_DIB` and as the registered `PNG` format, so
//! Paint, Office, browsers, Discord and Telegram all paste it. The data is owned by the system
//! clipboard, so it stays available after the overlay (or even Snapper) is gone.

use crate::files::{encode_png, Image};
use windows::core::w;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};

const CF_DIB: u32 = 8;

/// 40-byte BITMAPINFOHEADER followed by bottom-up 32-bit BGRA rows.
pub fn build_dib(img: &Image) -> Vec<u8> {
    let (w, h) = (img.w as usize, img.h as usize);
    let stride = w * 4;
    let mut out = Vec::with_capacity(40 + stride * h);
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(img.w as i32).to_le_bytes());
    out.extend_from_slice(&(img.h as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    out.extend_from_slice(&((stride * h) as u32).to_le_bytes());
    out.extend_from_slice(&[0u8; 16]); // resolution and palette fields
    for row in (0..h).rev() {
        out.extend_from_slice(&img.bgra[row * stride..(row + 1) * stride]);
    }
    out
}

unsafe fn global_from(data: &[u8]) -> Option<HGLOBAL> {
    let h = GlobalAlloc(GMEM_MOVEABLE, data.len()).ok()?;
    let p = GlobalLock(h);
    if p.is_null() {
        let _ = GlobalFree(Some(h));
        return None;
    }
    std::ptr::copy_nonoverlapping(data.as_ptr(), p as *mut u8, data.len());
    let _ = GlobalUnlock(h);
    Some(h)
}

pub fn copy_image(img: &Image) -> Result<(), String> {
    let dib = build_dib(img);
    let png = encode_png(img, true)?;
    unsafe {
        let mut opened = false;
        for _ in 0..25 {
            if OpenClipboard(None).is_ok() {
                opened = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        if !opened {
            return Err("clipboard is busy".into());
        }
        let result = (|| {
            EmptyClipboard().map_err(|e| e.to_string())?;
            let hdib = global_from(&dib).ok_or("out of memory")?;
            if SetClipboardData(CF_DIB, Some(HANDLE(hdib.0))).is_err() {
                let _ = GlobalFree(Some(hdib));
                return Err("SetClipboardData(CF_DIB) failed".to_string());
            }
            let png_fmt = RegisterClipboardFormatW(w!("PNG"));
            if png_fmt != 0 {
                if let Some(hpng) = global_from(&png) {
                    if SetClipboardData(png_fmt, Some(HANDLE(hpng.0))).is_err() {
                        let _ = GlobalFree(Some(hpng));
                    }
                }
            }
            Ok(())
        })();
        let _ = CloseClipboard();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dib_is_bottom_up_with_header() {
        let img = Image { w: 2, h: 2, bgra: vec![1, 1, 1, 255, 2, 2, 2, 255, 3, 3, 3, 255, 4, 4, 4, 255] };
        let d = build_dib(&img);
        assert_eq!(d.len(), 40 + 16);
        assert_eq!(&d[0..4], &40u32.to_le_bytes());
        assert_eq!(&d[4..8], &2i32.to_le_bytes());
        assert_eq!(&d[8..12], &2i32.to_le_bytes()); // positive height = bottom-up
        assert_eq!(d[40], 3); // first stored row is the last image row
        assert_eq!(d[48], 1);
    }
}
