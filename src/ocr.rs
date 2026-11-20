//! OCR through `Windows.Media.Ocr` (offline, part of Windows). The pixels are prepared for the
//! recogniser first (dark text on white, scaled so a text line is about 32 px tall, tiled when the
//! engine's size limit is reached), every useful language engine reads them, the best result is
//! picked by script (`textlayout::pick_engine`) and the word boxes become text (`textlayout::layout`).

use crate::files::Image;
use crate::textlayout::{self as tl, EngineText, Gray, Word};
use windows::core::HSTRING;
use windows::Globalization::Language;
use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::OcrEngine;
use windows::Storage::Streams::DataWriter;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

pub struct OcrOut {
    pub text: String,
    /// The language of the engine whose result was used.
    pub tag: Option<String>,
    /// Every engine's text (for diagnostics).
    pub all: Vec<EngineText>,
    /// How many engines could read.
    pub engines: usize,
    /// Wanted languages (profile languages and the interface language) without OCR support installed.
    pub missing: Vec<String>,
}

fn base(tag: &str) -> String {
    tag.split(['-', '_']).next().unwrap_or(tag).to_lowercase()
}

fn profile_languages() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(list) = windows::System::UserProfile::GlobalizationPreferences::Languages() {
        for t in list {
            out.push(t.to_string());
        }
    }
    out
}

/// OCR engines to run, in the order of the user's language list, then en, ru, uk.
/// Also the languages that were wanted but have no OCR pack.
fn engines(ui_lang: &str) -> (Vec<(String, OcrEngine)>, Vec<String>) {
    let mut available: Vec<String> = Vec::new();
    if let Ok(list) = OcrEngine::AvailableRecognizerLanguages() {
        for l in list {
            if let Ok(t) = l.LanguageTag() {
                available.push(t.to_string());
            }
        }
    }
    let profile = profile_languages();
    let mut wanted: Vec<String> = profile.iter().map(|t| base(t)).collect();
    for b in ["en", "ru", "uk"] {
        if !wanted.iter().any(|w| w == b) {
            wanted.push(b.to_string());
        }
    }
    let mut out: Vec<(String, OcrEngine)> = Vec::new();
    for want in &wanted {
        // an exact profile tag first, otherwise any available tag with this base
        let tag = profile.iter().find(|p| base(p) == *want && available.iter().any(|a| a.eq_ignore_ascii_case(p))).cloned().or_else(|| available.iter().find(|a| base(a) == *want).cloned());
        let Some(tag) = tag else { continue };
        if out.iter().any(|(t, _)| base(t) == *want) {
            continue;
        }
        if let Ok(lang) = Language::CreateLanguage(&HSTRING::from(tag.as_str())) {
            if let Ok(e) = OcrEngine::TryCreateFromLanguage(&lang) {
                out.push((tag, e));
            }
        }
    }
    let mut must: Vec<String> = profile.iter().map(|t| base(t)).collect();
    must.push(base(ui_lang));
    must.dedup();
    let mut missing: Vec<String> = Vec::new();
    for m in must {
        if !available.iter().any(|a| base(a) == m) && !missing.contains(&m) {
            missing.push(m);
        }
    }
    out.truncate(4);
    (out, missing)
}

fn max_dim() -> usize {
    OcrEngine::MaxImageDimension().map(|d| d as usize).unwrap_or(2600).min(10000).max(500)
}

/// Dark-on-white pixels scaled by `scale` (Lanczos) with a white margin.
fn upscale(g: &Gray, scale: usize) -> Gray {
    let pad = 12 * scale;
    let (sw, sh) = (g.w * scale, g.h * scale);
    let scaled: Vec<u8> = if scale == 1 {
        g.px.clone()
    } else {
        match image::GrayImage::from_raw(g.w as u32, g.h as u32, g.px.clone()) {
            Some(img) => image::imageops::resize(&img, sw as u32, sh as u32, image::imageops::FilterType::Lanczos3).into_raw(),
            None => return g.clone(),
        }
    };
    let (w, h) = (sw + 2 * pad, sh + 2 * pad);
    let mut px = vec![255u8; w * h];
    for y in 0..sh {
        px[(y + pad) * w + pad..(y + pad) * w + pad + sw].copy_from_slice(&scaled[y * sw..(y + 1) * sw]);
    }
    Gray { w, h, px }
}

fn bitmap(g: &Gray) -> Result<SoftwareBitmap, String> {
    let e = |e: windows::core::Error| e.to_string();
    let writer = DataWriter::new().map_err(e)?;
    writer.WriteBytes(&g.px).map_err(e)?;
    let buf = writer.DetachBuffer().map_err(e)?;
    SoftwareBitmap::CreateCopyFromBuffer(&buf, BitmapPixelFormat::Gray8, g.w as i32, g.h as i32).map_err(e)
}

fn recognize_one(engine: &OcrEngine, g: &Gray) -> Result<Vec<Word>, String> {
    let e = |e: windows::core::Error| e.to_string();
    let bmp = bitmap(g)?;
    let result = engine.RecognizeAsync(&bmp).map_err(e)?.join().map_err(e)?;
    let mut words = Vec::new();
    for line in result.Lines().map_err(e)? {
        for w in line.Words().map_err(e)? {
            let (Ok(r), Ok(t)) = (w.BoundingRect(), w.Text()) else { continue };
            words.push(Word { text: tl::fix_homoglyphs(&t.to_string()), l: r.X, t: r.Y, r: r.X + r.Width, b: r.Y + r.Height });
        }
    }
    Ok(words)
}

fn crop(g: &Gray, x0: usize, y0: usize, x1: usize, y1: usize) -> Gray {
    let w = x1 - x0;
    let mut px = Vec::with_capacity(w * (y1 - y0));
    for y in y0..y1 {
        px.extend_from_slice(&g.px[y * g.w + x0..y * g.w + x1]);
    }
    Gray { w, h: y1 - y0, px }
}

/// Reads an image of any size: pieces of at most `max` pixels per side, cut where the page is quiet.
fn recognize(engine: &OcrEngine, g: &Gray, max: usize) -> Result<Vec<Word>, String> {
    if g.w <= max && g.h <= max {
        return recognize_one(engine, g);
    }
    let rows: Vec<f32> = (0..g.h).map(|y| g.px[y * g.w..(y + 1) * g.w].iter().filter(|p| **p < 160).count() as f32).collect();
    let cols: Vec<f32> = (0..g.w).map(|x| (0..g.h).filter(|y| g.px[y * g.w + x] < 160).count() as f32).collect();
    let mut out = Vec::new();
    for (y0, y1) in tl::split_points(g.h, max, &rows) {
        for (x0, x1) in tl::split_points(g.w, max, &cols) {
            let piece = crop(g, x0, y0, x1, y1);
            for mut w in recognize_one(engine, &piece)? {
                w.l += x0 as f32;
                w.r += x0 as f32;
                w.t += y0 as f32;
                w.b += y0 as f32;
                out.push(w);
            }
        }
    }
    Ok(out)
}

/// Text-line bands of a dark-on-white image: `(top, bottom)` row ranges that hold ink.
fn bands(g: &Gray, scale: usize) -> Vec<(usize, usize)> {
    let min_ink = (g.w / 300).max(2);
    let ink: Vec<bool> = (0..g.h).map(|y| g.px[y * g.w..(y + 1) * g.w].iter().filter(|p| **p < 140).count() >= min_ink).collect();
    let mut out: Vec<(usize, usize)> = Vec::new();
    let mut y = 0;
    while y < g.h {
        if ink[y] {
            let s = y;
            while y < g.h && ink[y] {
                y += 1;
            }
            match out.last_mut() {
                // a gap of a few rows inside a line (the dot of an i, a comma) does not split it
                Some(last) if s - last.1 <= 2 * scale => last.1 = y,
                _ => out.push((s, y)),
            }
        } else {
            y += 1;
        }
    }
    out.retain(|(a, b)| b - a >= 5 * scale && b - a <= 90 * scale);
    out
}

/// The engine drops lines it takes for non-text (short tokens and symbols: `a, b = 0, 1`). Every
/// line band without a recognised word is cut out and read on its own.
fn recognize_missed(engine: &OcrEngine, g: &Gray, scale: usize, words: &mut Vec<Word>) {
    let mut done = 0;
    for (t, b) in bands(g, scale) {
        let covered = words.iter().any(|w| {
            let cy = (w.t + w.b) / 2.0;
            cy >= t as f32 && cy <= b as f32
        });
        if covered || done >= 40 {
            continue;
        }
        done += 1;
        let pad = 8 * scale;
        let (y0, y1) = (t.saturating_sub(pad), (b + pad).min(g.h));
        let mut strip = crop(g, 0, y0, g.w, y1);
        // the engine needs some height: pad with white
        let min_h = 48;
        if strip.h < min_h {
            let extra = min_h - strip.h;
            let mut px = vec![255u8; strip.w * (strip.h + extra)];
            let top = extra / 2;
            px[top * strip.w..(top + strip.h) * strip.w].copy_from_slice(&strip.px);
            strip = Gray { w: strip.w, h: strip.h + extra, px };
            if let Ok(found) = recognize_one(engine, &strip) {
                words.extend(found.into_iter().map(|mut w| {
                    w.t += y0 as f32 - top as f32;
                    w.b += y0 as f32 - top as f32;
                    w
                }));
            }
        } else if let Ok(found) = recognize_one(engine, &strip) {
            words.extend(found.into_iter().map(|mut w| {
                w.t += y0 as f32;
                w.b += y0 as f32;
                w
            }));
        }
    }
}

/// Words in the coordinates of the original region (margin and scale undone).
fn to_region(words: Vec<Word>, scale: usize) -> Vec<Word> {
    let pad = (12 * scale) as f32;
    let s = scale as f32;
    words.into_iter().map(|w| Word { text: w.text, l: (w.l - pad) / s, t: (w.t - pad) / s, r: (w.r - pad) / s, b: (w.b - pad) / s }).collect()
}

/// Recognises the text of a region of raw (undimmed, native-pixel) BGRA pixels.
pub fn read(img: &Image, ui_lang: &str) -> Result<OcrOut, String> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let t0 = std::time::Instant::now();
    let (engines, missing) = engines(ui_lang);
    let none = |missing: Vec<String>| OcrOut { text: String::new(), tag: None, all: Vec::new(), engines: 0, missing };
    if engines.is_empty() {
        return Ok(none(missing));
    }
    let gray = tl::to_gray(img.w as usize, img.h as usize, &img.bgra);
    let pol = tl::polarity(&gray);
    let norm = tl::normalise(&gray, pol);
    let max = max_dim();
    let scale = tl::pick_scale(tl::line_ink_height(&norm), norm.w, norm.h, max.saturating_sub(40), 24_000_000);
    #[cfg(test)]
    let scale = std::env::var("SNAPPER_OCR_SCALE").ok().and_then(|v| v.parse().ok()).unwrap_or(scale);
    let prepared = upscale(&norm, scale);
    crate::sys::log(&format!("ocr: {}x{} {:?} scale {scale} prepared in {:?}", img.w, img.h, pol, t0.elapsed()));

    // stage 1: the first Latin engine and the first Cyrillic one, in parallel; stage 2: the other Cyrillic ones
    let first_latin = engines.iter().position(|(t, _)| !tl::is_cyrillic_lang(t));
    let first_cyr = engines.iter().position(|(t, _)| tl::is_cyrillic_lang(t));
    let stage1: Vec<usize> = [first_latin, first_cyr].into_iter().flatten().collect();
    let run = |idx: &[usize]| -> Vec<(usize, Result<Vec<Word>, String>)> {
        std::thread::scope(|sc| {
            let handles: Vec<_> = idx
                .iter()
                .map(|&i| {
                    let (engine, prepared) = (&engines[i].1, &prepared);
                    (i, sc.spawn(move || {
                        unsafe {
                            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                        }
                        recognize(engine, prepared, max).map(|mut words| {
                            recognize_missed(engine, prepared, scale, &mut words);
                            words
                        })
                    }))
                })
                .collect();
            handles.into_iter().map(|(i, h)| (i, h.join().unwrap_or_else(|_| Err("ocr thread panicked".into())))).collect()
        })
    };
    let mut results: Vec<(usize, String)> = Vec::new();
    let mut last_err = None;
    let mut collect = |done: Vec<(usize, Result<Vec<Word>, String>)>, results: &mut Vec<(usize, String)>| {
        for (i, r) in done {
            match r {
                Ok(words) => results.push((i, tl::layout(&tl::fix_script_context(&to_region(words, scale))))),
                Err(e) => last_err = Some(e),
            }
        }
    };
    collect(run(&stage1), &mut results);
    let cyr_seen = results.iter().any(|(i, t)| tl::is_cyrillic_lang(&engines[*i].0) && tl::cyrillic_share(t) >= tl::CYRILLIC_TEXT);
    if cyr_seen {
        let more: Vec<usize> = engines.iter().enumerate().filter(|(i, (t, _))| tl::is_cyrillic_lang(t) && !stage1.contains(i)).map(|(i, _)| i).collect();
        if !more.is_empty() {
            collect(run(&more), &mut results);
        }
    }
    if results.is_empty() {
        return Err(last_err.unwrap_or_else(|| "ocr failed".into()));
    }
    results.sort_by_key(|(i, _)| *i);
    let texts: Vec<EngineText> = results.iter().map(|(i, t)| EngineText { tag: engines[*i].0.clone(), text: t.clone() }).collect();
    let best = tl::pick_engine(&texts).unwrap_or(0);
    crate::sys::log(&format!("ocr: {} engines, picked {} in {:?}", texts.len(), texts[best].tag, t0.elapsed()));
    Ok(OcrOut { text: texts[best].text.clone(), tag: Some(texts[best].tag.clone()), all: texts, engines: engines.len(), missing })
}
