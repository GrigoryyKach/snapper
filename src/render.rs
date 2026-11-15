//! Direct2D / DirectWrite helpers: the annotation model, its drawing code (shared by the on-screen
//! overlay and the final off-screen render) and the output image renderer.
//!
//! All render targets use 96 dpi, so one device-independent unit equals one physical pixel.

use crate::files::Image;
use crate::geom::Rect;
use std::cell::RefCell;
use windows::core::{w, Result};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};

pub type Pf = (f32, f32);

/// Stroke widths in physical pixels: thin, medium, thick.
pub const STROKES: [f32; 3] = [3.0, 5.0, 9.0];
/// Text sizes (pixels) matching the stroke steps.
pub const TEXT_SIZES: [f32; 3] = [20.0, 28.0, 40.0];

#[derive(Clone, Debug)]
pub enum Ann {
    Pen { pts: Vec<Pf>, color: u32, w: f32 },
    Arrow { a: Pf, b: Pf, color: u32, w: f32 },
    Rect { r: (f32, f32, f32, f32), color: u32, w: f32 },
    Fill { r: (f32, f32, f32, f32), color: u32 },
    Text { pos: Pf, text: String, color: u32, size: f32 },
}

pub fn norm_rect(a: Pf, b: Pf) -> (f32, f32, f32, f32) {
    (a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1))
}

pub fn colorf(rgb: u32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r: ((rgb >> 16) & 0xff) as f32 / 255.0, g: ((rgb >> 8) & 0xff) as f32 / 255.0, b: (rgb & 0xff) as f32 / 255.0, a }
}

pub fn rf(l: f32, t: f32, r: f32, b: f32) -> D2D_RECT_F {
    D2D_RECT_F { left: l, top: t, right: r, bottom: b }
}

pub fn rect_f(r: &Rect) -> D2D_RECT_F {
    rf(r.l as f32, r.t as f32, r.r as f32, r.b as f32)
}

pub fn p2(p: Pf) -> windows_numerics::Vector2 {
    windows_numerics::Vector2 { X: p.0, Y: p.1 }
}

pub fn v2(x: f32, y: f32) -> windows_numerics::Vector2 {
    windows_numerics::Vector2 { X: x, Y: y }
}

pub struct Gfx {
    pub d2d: ID2D1Factory,
    pub dw: IDWriteFactory,
    pub wic: IWICImagingFactory,
    fonts: RefCell<Vec<(u32, bool, IDWriteTextFormat)>>,
}

impl Gfx {
    pub fn new() -> Result<Gfx> {
        unsafe {
            let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let dw: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let wic: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
            Ok(Gfx { d2d, dw, wic, fonts: RefCell::new(Vec::new()) })
        }
    }

    /// Segoe UI text format of the given pixel size (cached).
    pub fn font(&self, size: f32, bold: bool) -> IDWriteTextFormat {
        let key = (size * 4.0) as u32;
        if let Some((_, _, f)) = self.fonts.borrow().iter().find(|(k, b, _)| *k == key && *b == bold) {
            return f.clone();
        }
        let weight = if bold { DWRITE_FONT_WEIGHT_SEMI_BOLD } else { DWRITE_FONT_WEIGHT_NORMAL };
        let f = unsafe {
            self.dw
                .CreateTextFormat(w!("Segoe UI"), None, weight, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, size, w!("en-us"))
                .expect("CreateTextFormat")
        };
        let mut v = self.fonts.borrow_mut();
        if v.len() > 24 {
            v.clear();
        }
        v.push((key, bold, f.clone()));
        f
    }

    pub fn layout(&self, text: &str, size: f32, bold: bool) -> Option<IDWriteTextLayout> {
        let wide: Vec<u16> = text.encode_utf16().collect();
        let fmt = self.font(size, bold);
        unsafe { self.dw.CreateTextLayout(&wide, &fmt, 100_000.0, 100_000.0).ok() }
    }

    /// Width and height of `text` laid out on one line.
    pub fn measure(&self, text: &str, size: f32, bold: bool) -> (f32, f32) {
        let Some(l) = self.layout(text, size, bold) else { return (0.0, size) };
        let mut m = DWRITE_TEXT_METRICS::default();
        unsafe {
            let _ = l.GetMetrics(&mut m);
        }
        (m.widthIncludingTrailingWhitespace, m.height)
    }
}

/// Brush and stroke style for one render target.
pub struct Pen {
    pub brush: ID2D1SolidColorBrush,
    pub round: ID2D1StrokeStyle,
}

impl Pen {
    pub fn new(gfx: &Gfx, rt: &ID2D1RenderTarget) -> Result<Pen> {
        unsafe {
            let brush = rt.CreateSolidColorBrush(&colorf(0xffffff, 1.0), None)?;
            let props = D2D1_STROKE_STYLE_PROPERTIES {
                startCap: D2D1_CAP_STYLE_ROUND,
                endCap: D2D1_CAP_STYLE_ROUND,
                dashCap: D2D1_CAP_STYLE_ROUND,
                lineJoin: D2D1_LINE_JOIN_ROUND,
                miterLimit: 10.0,
                dashStyle: D2D1_DASH_STYLE_SOLID,
                dashOffset: 0.0,
            };
            let round = gfx.d2d.CreateStrokeStyle(&props, None)?;
            Ok(Pen { brush, round })
        }
    }
}

fn set(pen: &Pen, rgb: u32, a: f32) {
    unsafe { pen.brush.SetColor(&colorf(rgb, a)) }
}

/// Midpoint-quadratic smoothing of a freehand polyline.
fn pen_path(gfx: &Gfx, pts: &[Pf]) -> Result<ID2D1PathGeometry> {
    unsafe {
        let g = gfx.d2d.CreatePathGeometry()?;
        let sink = g.Open()?;
        sink.BeginFigure(p2(pts[0]), D2D1_FIGURE_BEGIN_HOLLOW);
        if pts.len() == 2 {
            sink.AddLine(p2(pts[1]));
        } else {
            for i in 1..pts.len() - 1 {
                let mid = ((pts[i].0 + pts[i + 1].0) / 2.0, (pts[i].1 + pts[i + 1].1) / 2.0);
                sink.AddQuadraticBezier(&D2D1_QUADRATIC_BEZIER_SEGMENT { point1: p2(pts[i]), point2: p2(mid) });
            }
            sink.AddLine(p2(pts[pts.len() - 1]));
        }
        sink.EndFigure(D2D1_FIGURE_END_OPEN);
        sink.Close()?;
        Ok(g)
    }
}

pub fn arrow_head_len(w: f32) -> f32 {
    (w * 4.0).max(14.0)
}

fn arrow_geometry(gfx: &Gfx, a: Pf, b: Pf, w: f32) -> Option<(Pf, ID2D1PathGeometry)> {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len = (dx * dx + dy * dy).sqrt();
    if len < 1.0 {
        return None;
    }
    let (ux, uy) = (dx / len, dy / len);
    let hl = arrow_head_len(w).min(len);
    let hw = hl * 0.45;
    let base = (b.0 - ux * hl, b.1 - uy * hl);
    let left = (base.0 - uy * hw, base.1 + ux * hw);
    let right = (base.0 + uy * hw, base.1 - ux * hw);
    unsafe {
        let g = gfx.d2d.CreatePathGeometry().ok()?;
        let sink = g.Open().ok()?;
        sink.BeginFigure(p2(b), D2D1_FIGURE_BEGIN_FILLED);
        sink.AddLine(p2(left));
        sink.AddLine(p2(right));
        sink.EndFigure(D2D1_FIGURE_END_CLOSED);
        sink.Close().ok()?;
        // the shaft ends inside the head so the tip stays sharp
        Some(((b.0 - ux * hl * 0.6, b.1 - uy * hl * 0.6), g))
    }
}

/// Draws one annotation in the render target's current coordinate system (virtual-desktop pixels).
pub fn draw_ann(gfx: &Gfx, rt: &ID2D1RenderTarget, pen: &Pen, ann: &Ann) {
    unsafe {
        match ann {
            Ann::Pen { pts, color, w } => {
                set(pen, *color, 1.0);
                match pts.len() {
                    0 => {}
                    1 => rt.FillEllipse(&D2D1_ELLIPSE { point: p2(pts[0]), radiusX: w / 2.0, radiusY: w / 2.0 }, &pen.brush),
                    _ => {
                        if let Ok(g) = pen_path(gfx, pts) {
                            rt.DrawGeometry(&g, &pen.brush, *w, &pen.round);
                        }
                    }
                }
            }
            Ann::Arrow { a, b, color, w } => {
                set(pen, *color, 1.0);
                if let Some((shaft_end, head)) = arrow_geometry(gfx, *a, *b, *w) {
                    rt.DrawLine(p2(*a), p2(shaft_end), &pen.brush, *w, &pen.round);
                    rt.FillGeometry(&head, &pen.brush, None);
                    // round the head corners a little with a thin outline in the same colour
                    rt.DrawGeometry(&head, &pen.brush, (*w * 0.35).max(1.0), &pen.round);
                }
            }
            Ann::Rect { r, color, w } => {
                set(pen, *color, 1.0);
                rt.DrawRectangle(&rf(r.0, r.1, r.2, r.3), &pen.brush, *w, &pen.round);
            }
            Ann::Fill { r, color } => {
                set(pen, *color, 1.0);
                rt.FillRectangle(&rf(r.0, r.1, r.2, r.3), &pen.brush);
            }
            Ann::Text { pos, text, color, size } => {
                if let Some(l) = gfx.layout(text, *size, false) {
                    set(pen, *color, 1.0);
                    rt.DrawTextLayout(p2(*pos), &l, &pen.brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
                }
            }
        }
    }
}

pub fn bitmap_props() -> D2D1_BITMAP_PROPERTIES {
    D2D1_BITMAP_PROPERTIES {
        pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
        dpiX: 96.0,
        dpiY: 96.0,
    }
}

/// Renders the selected area of the frozen desktop plus all annotations at full physical resolution.
/// `pixels` is the top-down BGRA frozen image of `vrect`.
pub fn render_output(gfx: &Gfx, pixels: *const u8, vrect: &Rect, sel: &Rect, anns: &[Ann]) -> std::result::Result<Image, String> {
    let (w, h) = (sel.w() as u32, sel.h() as u32);
    let stride = vrect.w() as usize * 4;
    unsafe {
        let bmp = gfx.wic.CreateBitmap(w, h, &GUID_WICPixelFormat32bppPBGRA, WICBitmapCacheOnLoad).map_err(|e| e.to_string())?;
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
            dpiX: 96.0,
            dpiY: 96.0,
            usage: D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        let rt = gfx.d2d.CreateWicBitmapRenderTarget(&bmp, &props).map_err(|e| e.to_string())?;
        let rt: ID2D1RenderTarget = rt.into();
        let pen = Pen::new(gfx, &rt).map_err(|e| e.to_string())?;
        let offset = (sel.t - vrect.t) as usize * stride + (sel.l - vrect.l) as usize * 4;
        let frozen = rt
            .CreateBitmap(D2D_SIZE_U { width: w, height: h }, Some(pixels.add(offset) as *const _), stride as u32, &bitmap_props())
            .map_err(|e| e.to_string())?;
        rt.BeginDraw();
        rt.DrawBitmap(&frozen, Some(&rf(0.0, 0.0, w as f32, h as f32)), 1.0, D2D1_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR, None);
        rt.SetTransform(&windows_numerics_translate(-(sel.l as f32), -(sel.t as f32)));
        for a in anns {
            draw_ann(gfx, &rt, &pen, a);
        }
        rt.SetTransform(&windows_numerics_translate(0.0, 0.0));
        rt.EndDraw(None, None).map_err(|e| e.to_string())?;
        drop(frozen);

        let region = WICRect { X: 0, Y: 0, Width: w as i32, Height: h as i32 };
        let lock = bmp.Lock(&region, WICBitmapLockRead.0 as u32).map_err(|e| e.to_string())?;
        let mut size = 0u32;
        let mut ptr: *mut u8 = std::ptr::null_mut();
        lock.GetDataPointer(&mut size, &mut ptr).map_err(|e| e.to_string())?;
        let lstride = lock.GetStride().map_err(|e| e.to_string())? as usize;
        let mut out = vec![0u8; w as usize * h as usize * 4];
        for y in 0..h as usize {
            let src = std::slice::from_raw_parts(ptr.add(y * lstride), w as usize * 4);
            out[y * w as usize * 4..(y + 1) * w as usize * 4].copy_from_slice(src);
        }
        for px in out.chunks_exact_mut(4) {
            px[3] = 255;
        }
        Ok(Image { w, h, bgra: out })
    }
}

pub fn windows_numerics_translate(x: f32, y: f32) -> windows_numerics::Matrix3x2 {
    windows_numerics::Matrix3x2 { M11: 1.0, M12: 0.0, M21: 0.0, M22: 1.0, M31: x, M32: y }
}
