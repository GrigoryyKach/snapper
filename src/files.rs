//! Output files: names, collision handling and PNG/JPEG encoding of the captured image.

use crate::config::Format;
use std::path::{Path, PathBuf};

/// A finished screenshot: top-down BGRA pixels, alpha is always 255.
pub struct Image {
    pub w: u32,
    pub h: u32,
    pub bgra: Vec<u8>,
}

/// `Snapper_YYYY-MM-DD_HH-mm-ss` (no extension).
pub fn base_name(y: u16, mo: u16, d: u16, h: u16, mi: u16, s: u16) -> String {
    format!("Snapper_{y:04}-{mo:02}-{d:02}_{h:02}-{mi:02}-{s:02}")
}

/// First free path `dir\base.ext`, then `dir\base_2.ext`, `base_3.ext`, ...
pub fn unique_path(dir: &Path, base: &str, ext: &str, exists: impl Fn(&Path) -> bool) -> PathBuf {
    let first = dir.join(format!("{base}.{ext}"));
    if !exists(&first) {
        return first;
    }
    let mut n = 2;
    loop {
        let p = dir.join(format!("{base}_{n}.{ext}"));
        if !exists(&p) {
            return p;
        }
        n += 1;
    }
}

/// PNG, 8-bit RGB. `fast` trades size for speed (used for the clipboard).
pub fn encode_png(img: &Image, fast: bool) -> Result<Vec<u8>, String> {
    let mut rgb = Vec::with_capacity(img.bgra.len() / 4 * 3);
    for px in img.bgra.chunks_exact(4) {
        rgb.extend_from_slice(&[px[2], px[1], px[0]]);
    }
    let mut out = Vec::with_capacity(rgb.len() / 3);
    {
        let mut enc = png::Encoder::new(&mut out, img.w, img.h);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        if fast {
            enc.set_compression(png::Compression::Fast);
            enc.set_filter(png::FilterType::Sub);
        }
        let mut w = enc.write_header().map_err(|e| e.to_string())?;
        w.write_image_data(&rgb).map_err(|e| e.to_string())?;
    }
    Ok(out)
}

/// JPEG at the given quality (1..=100).
pub fn encode_jpeg(img: &Image, quality: u8) -> Result<Vec<u8>, String> {
    let (w, h) = (u16::try_from(img.w).map_err(|_| "image too wide for JPEG")?, u16::try_from(img.h).map_err(|_| "image too tall for JPEG")?);
    let mut out = Vec::new();
    let enc = jpeg_encoder::Encoder::new(&mut out, quality);
    enc.encode(&img.bgra, w, h, jpeg_encoder::ColorType::Bgra).map_err(|e| e.to_string())?;
    Ok(out)
}

pub fn encode(img: &Image, format: Format) -> Result<Vec<u8>, String> {
    match format {
        Format::Png => encode_png(img, false),
        Format::Jpg => encode_jpeg(img, 90),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn names_follow_the_pattern() {
        assert_eq!(base_name(2026, 11, 5, 9, 3, 7), "Snapper_2026-11-05_09-03-07");
    }

    #[test]
    fn collisions_get_a_suffix() {
        let dir = Path::new("C:\\Shots");
        let taken: HashSet<PathBuf> = ["a.png", "a_2.png", "a_3.png"].iter().map(|n| dir.join(n)).collect();
        assert_eq!(unique_path(dir, "a", "png", |p| taken.contains(p)), dir.join("a_4.png"));
        assert_eq!(unique_path(dir, "b", "png", |p| taken.contains(p)), dir.join("b.png"));
        assert_eq!(unique_path(dir, "a", "jpg", |p| taken.contains(p)), dir.join("a.jpg"));
    }

    fn sample() -> Image {
        // 3x2: BGRA
        let mut bgra = Vec::new();
        for i in 0..6u8 {
            bgra.extend_from_slice(&[i * 10, i * 20, i * 30, 255]);
        }
        Image { w: 3, h: 2, bgra }
    }

    #[test]
    fn png_roundtrips_pixels() {
        let data = encode_png(&sample(), false).unwrap();
        assert_eq!(&data[1..4], b"PNG");
        let dec = png::Decoder::new(std::io::Cursor::new(data));
        let mut reader = dec.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).unwrap();
        assert_eq!((info.width, info.height), (3, 2));
        assert_eq!(&buf[0..3], &[0, 0, 0]);
        assert_eq!(&buf[3..6], &[30, 20, 10]); // pixel 1 is stored BGR, comes back RGB
        let fast = encode_png(&sample(), true).unwrap();
        assert_eq!(&fast[1..4], b"PNG");
    }

    #[test]
    fn jpeg_has_the_right_header() {
        let data = encode_jpeg(&sample(), 90).unwrap();
        assert_eq!(&data[0..2], &[0xFF, 0xD8]);
    }
}
