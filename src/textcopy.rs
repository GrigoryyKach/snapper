//! "Copy text": the text of the selected region as plain text.
//!
//! Two sources run side by side on the pixels and the screen of the region:
//! - exact text from UI Automation (`uia`), when the program under the region exposes it;
//! - OCR with Windows' own engine (`ocr`) on the undimmed pixels.
//!
//! `textlayout::choose` decides: the exact text is used only when it covers what OCR sees there
//! (so half of the text is never returned silently), otherwise the OCR text is used.

use crate::files::Image;
use crate::geom::Rect;
use crate::i18n::Lang;
use crate::strings;
use crate::textlayout::{self as tl, Source};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Method {
    Exact,
    Ocr,
}

pub struct Outcome {
    /// `None`: nothing was found, the clipboard stays as it was.
    pub text: Option<String>,
    pub method: Method,
    /// Wanted OCR languages with no language pack, when that matters for the result.
    pub missing: Vec<String>,
}

/// Both raw results, for `extract` and for the accuracy benchmark.
pub struct Parts {
    pub uia: Option<crate::uia::UiaOut>,
    pub ocr: Result<crate::ocr::OcrOut, String>,
}

/// Runs UI Automation and OCR in parallel. `region` is in virtual-desktop pixels, `img` holds the
/// raw pixels of the same region. UI Automation gets at most `uia_budget`.
pub fn read_parts(img: &Image, region: Rect, lang_code: &str, uia_budget: Duration) -> Parts {
    let deadline = Instant::now() + uia_budget;
    std::thread::scope(|sc| {
        let u = sc.spawn(move || crate::uia::read(region, deadline));
        let o = sc.spawn(|| crate::ocr::read(img, lang_code));
        Parts { uia: u.join().ok().flatten(), ocr: o.join().unwrap_or_else(|_| Err("ocr thread panicked".into())) }
    })
}

/// Applies the choice rule to both results.
pub fn decide(parts: Parts) -> Outcome {
    let uia_text = parts.uia.as_ref().map(|u| u.text.trim().to_string()).unwrap_or_default();
    let (ocr_text, missing) = match &parts.ocr {
        Ok(o) => (o.text.trim().to_string(), o.missing.clone()),
        Err(_) => (String::new(), Vec::new()),
    };
    let src = tl::choose(&uia_text, &ocr_text, parts.uia.as_ref().map_or(true, |u| u.complete));
    let (complete, tag, engines) = (parts.uia.as_ref().map_or(true, |u| u.complete), parts.ocr.as_ref().ok().and_then(|o| o.tag.clone()), parts.ocr.as_ref().map_or(0, |o| o.engines));
    crate::sys::log(&format!("text: uia {} chars (complete: {complete}), ocr {} chars ({tag:?}, {engines} engines), chose {src:?}", uia_text.len(), ocr_text.len()));
    if let Ok(o) = &parts.ocr {
        crate::sys::log(&format!("text: ocr candidates {:?}", o.all.iter().map(|e| e.tag.as_str()).collect::<Vec<_>>()));
    }
    match src {
        Some(Source::Exact) => Outcome { text: Some(uia_text), method: Method::Exact, missing: Vec::new() },
        Some(Source::Ocr) => Outcome { text: Some(ocr_text), method: Method::Ocr, missing },
        None => Outcome { text: None, method: Method::Ocr, missing },
    }
}

pub fn extract(img: &Image, region: Rect, lang_code: &str) -> Outcome {
    decide(read_parts(img, region, lang_code, Duration::from_millis(2200)))
}

/// "3 lines" with the plural form of the language.
pub fn lines_label(lang: Lang, n: usize) -> String {
    let t = strings::tr(lang);
    let idx = match lang.code() {
        "ru" | "uk" => {
            let (m10, m100) = (n % 10, n % 100);
            if m10 == 1 && m100 != 11 {
                0
            } else if (2..=4).contains(&m10) && !(12..=14).contains(&m100) {
                1
            } else {
                2
            }
        }
        _ => usize::from(n != 1),
    };
    t.text_lines[idx].replace("{n}", &n.to_string())
}

/// The balloon text for a finished outcome.
pub fn notice(lang: Lang, out: &Outcome) -> String {
    let t = strings::tr(lang);
    match &out.text {
        Some(text) => {
            let n = text.lines().filter(|l| !l.trim().is_empty()).count().max(1);
            let m = if out.method == Method::Exact { t.text_exact } else { t.text_ocr };
            t.text_copied.replace("{n}", &lines_label(lang, n)).replace("{m}", m)
        }
        None => {
            if out.missing.is_empty() {
                t.text_none.to_string()
            } else {
                format!("{}\n{}", t.text_none, t.text_no_ocr.replace("{l}", &out.missing.join(", ")))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plural_forms() {
        assert_eq!(lines_label(Lang::En, 1), "1 line");
        assert_eq!(lines_label(Lang::En, 3), "3 lines");
        assert_eq!(lines_label(Lang::Ru, 1), "1 строка");
        assert_eq!(lines_label(Lang::Ru, 3), "3 строки");
        assert_eq!(lines_label(Lang::Ru, 11), "11 строк");
        assert_eq!(lines_label(Lang::Ru, 22), "22 строки");
        assert_eq!(lines_label(Lang::Uk, 5), "5 рядків");
    }

    #[test]
    fn notice_names_the_method() {
        let o = Outcome { text: Some("a\nb\n".into()), method: Method::Exact, missing: vec![] };
        assert_eq!(notice(Lang::En, &o), "Text copied \u{b7} 2 lines \u{b7} exact text");
        let o = Outcome { text: None, method: Method::Ocr, missing: vec!["uk".into()] };
        assert!(notice(Lang::En, &o).contains("No text found") && notice(Lang::En, &o).contains("uk"));
    }

    /// Accuracy benchmark against a rendered test page. Run by hand (see tasks/2026-11-20):
    /// `SNAPPER_BENCH_DIR=<dir> cargo test --release bench -- --ignored --nocapture`.
    /// `<dir>/cases.json` lists the regions in CSS pixels of a browser page that shows a magenta
    /// 10 px marker at its top-left corner; the page is on screen and in front.
    #[test]
    #[ignore]
    fn bench() {
        use crate::capture;
        use crate::textlayout::levenshtein;
        let Some(dir) = std::env::var_os("SNAPPER_BENCH_DIR") else { return };
        unsafe {
            let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
        let dir = std::path::PathBuf::from(dir);
        let cases: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join("cases.json")).unwrap()).unwrap();
        let dpr = cases["dpr"].as_f64().unwrap();
        let mons = capture::monitors();
        let vr = capture::virtual_rect(&mons);
        let cap = capture::capture(vr).expect("capture");
        let (vw, vh) = (vr.w() as usize, vr.h() as usize);
        let stride = cap.stride();
        let px = |x: usize, y: usize| unsafe { std::slice::from_raw_parts(cap.ptr().add(y * stride + x * 4), 4) };
        let (mut ox, mut oy, mut n) = (usize::MAX, usize::MAX, 0);
        for y in 0..vh {
            for x in 0..vw {
                let p = px(x, y);
                if p[0] == 255 && p[1] == 0 && p[2] == 255 {
                    ox = ox.min(x);
                    oy = oy.min(y);
                    n += 1;
                }
            }
        }
        assert!(n > 20, "marker not found: the test page must be visible");
        println!("origin {ox},{oy} dpr {dpr} marker px {n}");
        let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        let acc = |out: &str, truth: &str| {
            let (a, b): (Vec<char>, Vec<char>) = (norm(out).chars().collect(), norm(truth).chars().collect());
            (1.0 - levenshtein(&a, &b) as f64 / b.len().max(1) as f64).max(0.0)
        };
        let exact = |out: &str, truth: &str| {
            let fix = |s: &str| s.replace("\r\n", "\n").lines().map(|l| l.trim_end().to_string()).collect::<Vec<_>>().join("\n").trim_matches('\n').to_string();
            let (a, b): (Vec<char>, Vec<char>) = (fix(out).chars().collect(), fix(truth).chars().collect());
            (1.0 - levenshtein(&a, &b) as f64 / b.len().max(1) as f64).max(0.0)
        };
        std::fs::create_dir_all(dir.join("out")).unwrap();
        let only = std::env::var("SNAPPER_BENCH_ONLY").ok();
        for c in cases["cases"].as_array().unwrap() {
            let id = c["id"].as_str().unwrap();
            if only.as_deref().is_some_and(|o| !o.split(',').any(|x| x == id)) {
                continue;
            }
            let r: Vec<f64> = c["rect"].as_array().unwrap().iter().map(|v| v.as_f64().unwrap()).collect();
            let (l, t) = (ox as f64 + (r[0] * dpr).floor(), oy as f64 + (r[1] * dpr).floor());
            let (rr, b) = (ox as f64 + (r[2] * dpr).ceil(), oy as f64 + (r[3] * dpr).ceil());
            let region = Rect { l: l as i32 + vr.l, t: t as i32 + vr.t, r: rr as i32 + vr.l, b: b as i32 + vr.t };
            let (w, h) = (region.w() as usize, region.h() as usize);
            let mut bgra = Vec::with_capacity(w * h * 4);
            for y in 0..h {
                for x in 0..w {
                    bgra.extend_from_slice(px(l as usize + x, t as usize + y));
                }
            }
            let img = Image { w: w as u32, h: h as u32, bgra };
            if std::env::var_os("SNAPPER_BENCH_DUMP").is_some() {
                std::fs::write(dir.join("out").join(format!("{id}.png")), crate::files::encode_png(&img, true).unwrap()).unwrap();
            }
            if std::env::var_os("SNAPPER_BENCH_DEBUG").is_some() {
                crate::uia::debug_point((region.l + region.r) / 2, (region.t + region.b) / 2);
            }
            let truth = c["truth"].as_str().unwrap();
            let t0 = Instant::now();
            let parts = read_parts(&img, region, "en", Duration::from_millis(2200));
            let ms = t0.elapsed().as_millis();
            let uia = parts.uia.as_ref().map(|u| u.text.clone()).unwrap_or_default();
            let (ocr, tag) = parts.ocr.as_ref().map(|o| (o.text.clone(), o.tag.clone().unwrap_or_default())).unwrap_or_default();
            let st = crate::textlayout::stats(&uia, &ocr);
            println!("STATS	{id}	explained {:.2} seen {:.2} ({} ocr words, {} exact words)", st.explained, st.seen, st.n_ocr, st.n_uia);
            if let Ok(o) = &parts.ocr {
                for e in &o.all {
                    std::fs::write(dir.join("out").join(format!("{id}.ocr.{}.txt", e.tag)), &e.text).unwrap();
                }
            }
            let out = decide(Parts { uia: parts.uia, ocr: parts.ocr });
            let chosen = out.text.clone().unwrap_or_default();
            std::fs::write(dir.join("out").join(format!("{id}.uia.txt")), &uia).unwrap();
            std::fs::write(dir.join("out").join(format!("{id}.ocr.txt")), &ocr).unwrap();
            std::fs::write(dir.join("out").join(format!("{id}.chosen.txt")), &chosen).unwrap();
            println!(
                "RESULT\t{id}\tuia {:.3}\tocr {:.3}\tchosen {:?} {:.3}\tlayout uia {:.3} ocr {:.3} chosen {:.3}\t{ms} ms\t{tag}",
                acc(&uia, truth),
                acc(&ocr, truth),
                out.method,
                acc(&chosen, truth),
                exact(&uia, truth),
                exact(&ocr, truth),
                exact(&chosen, truth)
            );
        }
        drop(cap);
    }

    /// The clipboard round trip of "Copy text", with the user's text clipboard saved and restored.
    #[test]
    #[ignore]
    fn clipboard_text_roundtrip() {
        let saved = crate::clipboard::read_text();
        let sample = "Привіт, ёжик\nsecond line\ttab";
        crate::clipboard::copy_text(sample).unwrap();
        let got = crate::clipboard::read_text();
        match &saved {
            Some(t) => crate::clipboard::copy_text(t).unwrap(),
            None => {}
        }
        assert_eq!(got.as_deref(), Some("Привіт, ёжик\r\nsecond line\ttab"));
    }

    /// Lists the OCR engines this machine can run.
    #[test]
    #[ignore]
    fn ocr_engines() {
        let img = Image { w: 40, h: 20, bgra: vec![255; 40 * 20 * 4] };
        let o = crate::ocr::read(&img, "en").unwrap();
        println!("engines {} missing {:?} tag {:?}", o.engines, o.missing, o.tag);
    }

    #[test]
    #[ignore]
    fn uia_debug() {
        unsafe {
            let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
        let x: i32 = std::env::var("X").unwrap().parse().unwrap();
        let y: i32 = std::env::var("Y").unwrap().parse().unwrap();
        crate::uia::debug_point(x, y);
    }
}
