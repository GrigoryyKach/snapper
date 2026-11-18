//! Linux output renderer: crops the frozen desktop and draws the annotations on the CPU with
//! tiny-skia (shapes) and ab_glyph (text). Produces the same `Image` the Windows renderer does.

use crate::ann::{arrow_points, Ann};
use crate::files::Image;
use crate::geom::Rect;
use ab_glyph::{Font, FontArc, PxScale, ScaleFont};
use tiny_skia::{FillRule, LineCap, LineJoin, Paint, PathBuilder, Pixmap, Stroke, Transform};

fn paint(rgb: u32) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color_rgba8(((rgb >> 16) & 0xff) as u8, ((rgb >> 8) & 0xff) as u8, (rgb & 0xff) as u8, 255);
    p.anti_alias = true;
    p
}

fn round_stroke(w: f32) -> Stroke {
    Stroke { width: w, line_cap: LineCap::Round, line_join: LineJoin::Round, ..Default::default() }
}

/// Vertical metrics of the text font at `size` pixels: (ascent, line height).
#[allow(dead_code)]
pub fn text_metrics(font: &FontArc, size: f32) -> (f32, f32) {
    let f = font.as_scaled(font.pt_to_px_scale(size).unwrap_or(PxScale::from(size)));
    (f.ascent(), f.ascent() - f.descent() + f.line_gap())
}

/// Width of one line of text.
#[allow(dead_code)]
pub fn text_width(font: &FontArc, size: f32, line: &str) -> f32 {
    let f = font.as_scaled(font.pt_to_px_scale(size).unwrap_or(PxScale::from(size)));
    let mut x = 0.0;
    let mut prev = None;
    for c in line.chars() {
        let id = f.glyph_id(c);
        if let Some(p) = prev {
            x += f.kern(p, id);
        }
        x += f.h_advance(id);
        prev = Some(id);
    }
    x
}

fn draw_text(pm: &mut Pixmap, font: &FontArc, pos: (f32, f32), text: &str, rgb: u32, size: f32) {
    let scale = font.pt_to_px_scale(size).unwrap_or(PxScale::from(size));
    let f = font.as_scaled(scale);
    let (ascent, line_h) = (f.ascent(), f.ascent() - f.descent() + f.line_gap());
    let (cr, cg, cb) = (((rgb >> 16) & 0xff) as f32, ((rgb >> 8) & 0xff) as f32, (rgb & 0xff) as f32);
    let (pw, ph) = (pm.width() as i32, pm.height() as i32);
    let data = pm.data_mut();
    for (li, line) in text.split('\n').enumerate() {
        let base_y = pos.1 + ascent + li as f32 * line_h;
        let mut x = pos.0;
        let mut prev = None;
        for c in line.chars() {
            let id = f.glyph_id(c);
            if let Some(p) = prev {
                x += f.kern(p, id);
            }
            let glyph = id.with_scale_and_position(scale, ab_glyph::point(x, base_y));
            if let Some(o) = font.outline_glyph(glyph) {
                let b = o.px_bounds();
                o.draw(|gx, gy, cov| {
                    let (px, py) = (b.min.x as i32 + gx as i32, b.min.y as i32 + gy as i32);
                    if px < 0 || py < 0 || px >= pw || py >= ph || cov <= 0.0 {
                        return;
                    }
                    let i = ((py * pw + px) * 4) as usize;
                    // the pixmap is premultiplied and opaque here: plain source-over
                    let a = cov.min(1.0);
                    data[i] = (cr * a + data[i] as f32 * (1.0 - a)).round() as u8;
                    data[i + 1] = (cg * a + data[i + 1] as f32 * (1.0 - a)).round() as u8;
                    data[i + 2] = (cb * a + data[i + 2] as f32 * (1.0 - a)).round() as u8;
                });
            }
            x += f.h_advance(id);
            prev = Some(id);
        }
    }
}

/// Draws one annotation; `t` moves desktop coordinates into the pixmap.
pub fn draw_ann(pm: &mut Pixmap, font: Option<&FontArc>, a: &Ann, t: Transform) {
    match a {
        Ann::Pen { pts, color, w } => {
            let p = paint(*color);
            match pts.len() {
                0 => {}
                1 => {
                    if let Some(path) = PathBuilder::from_circle(pts[0].0, pts[0].1, w / 2.0) {
                        pm.fill_path(&path, &p, FillRule::Winding, t, None);
                    }
                }
                n => {
                    let mut pb = PathBuilder::new();
                    pb.move_to(pts[0].0, pts[0].1);
                    if n == 2 {
                        pb.line_to(pts[1].0, pts[1].1);
                    } else {
                        for i in 1..n - 1 {
                            let mid = ((pts[i].0 + pts[i + 1].0) / 2.0, (pts[i].1 + pts[i + 1].1) / 2.0);
                            pb.quad_to(pts[i].0, pts[i].1, mid.0, mid.1);
                        }
                        pb.line_to(pts[n - 1].0, pts[n - 1].1);
                    }
                    if let Some(path) = pb.finish() {
                        pm.stroke_path(&path, &p, &round_stroke(*w), t, None);
                    }
                }
            }
        }
        Ann::Arrow { a: pa, b: pb_, color, w } => {
            if let Some((shaft_end, [tip, l, r])) = arrow_points(*pa, *pb_, *w) {
                let p = paint(*color);
                let mut shaft = PathBuilder::new();
                shaft.move_to(pa.0, pa.1);
                shaft.line_to(shaft_end.0, shaft_end.1);
                if let Some(path) = shaft.finish() {
                    pm.stroke_path(&path, &p, &round_stroke(*w), t, None);
                }
                let mut head = PathBuilder::new();
                head.move_to(tip.0, tip.1);
                head.line_to(l.0, l.1);
                head.line_to(r.0, r.1);
                head.close();
                if let Some(path) = head.finish() {
                    pm.fill_path(&path, &p, FillRule::Winding, t, None);
                    pm.stroke_path(&path, &p, &round_stroke((*w * 0.35).max(1.0)), t, None);
                }
            }
        }
        Ann::Rect { r, color, w } => {
            let mut pb = PathBuilder::new();
            pb.move_to(r.0, r.1);
            pb.line_to(r.2, r.1);
            pb.line_to(r.2, r.3);
            pb.line_to(r.0, r.3);
            pb.close();
            if let Some(path) = pb.finish() {
                pm.stroke_path(&path, &paint(*color), &round_stroke(*w), t, None);
            }
        }
        Ann::Fill { r, color } => {
            if let Some(rc) = tiny_skia::Rect::from_ltrb(r.0, r.1, r.2.max(r.0 + 0.01), r.3.max(r.1 + 0.01)) {
                let mut p = paint(*color);
                p.anti_alias = false;
                pm.fill_rect(rc, &p, t, None);
            }
        }
        Ann::Text { pos, text, color, size } => {
            if let Some(f) = font {
                // text is drawn in pixmap space: apply the translation by hand
                draw_text(pm, f, (pos.0 + t.tx, pos.1 + t.ty), text, *color, *size);
            }
        }
    }
}

/// Crops `sel` out of the frozen desktop (`bgra`, `sw` pixels wide) and draws the annotations on it.
pub fn render_output(bgra: &[u8], sw: usize, sel: &Rect, anns: &[Ann], font: Option<&FontArc>) -> Result<Image, String> {
    let (w, h) = (sel.w() as u32, sel.h() as u32);
    let mut pm = Pixmap::new(w, h).ok_or("empty selection")?;
    {
        let dst = pm.data_mut();
        for y in 0..h as usize {
            let src = ((sel.t as usize + y) * sw + sel.l as usize) * 4;
            let row = bgra.get(src..src + w as usize * 4).ok_or("selection outside the picture")?;
            for (x, px) in row.chunks_exact(4).enumerate() {
                let o = (y * w as usize + x) * 4;
                dst[o] = px[2];
                dst[o + 1] = px[1];
                dst[o + 2] = px[0];
                dst[o + 3] = 255;
            }
        }
    }
    let t = Transform::from_translate(-(sel.l as f32), -(sel.t as f32));
    for a in anns {
        draw_ann(&mut pm, font, a, t);
    }
    let mut out = pm.take();
    for px in out.chunks_exact_mut(4) {
        px.swap(0, 2); // RGBA -> BGRA
        px[3] = 255;
    }
    Ok(Image { w, h, bgra: out })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::rect;

    fn grey(w: usize, h: usize, v: u8) -> Vec<u8> {
        let mut d = Vec::new();
        for _ in 0..w * h {
            d.extend_from_slice(&[v, v, v, 255]);
        }
        d
    }

    fn px(img: &Image, x: u32, y: u32) -> [u8; 3] {
        let i = ((y * img.w + x) * 4) as usize;
        [img.bgra[i + 2], img.bgra[i + 1], img.bgra[i]] // as RGB
    }

    #[test]
    fn crop_has_the_selection_size() {
        let src = grey(10, 8, 77);
        let img = render_output(&src, 10, &rect(2, 1, 7, 6), &[], None).unwrap();
        assert_eq!((img.w, img.h), (5, 5));
        assert_eq!(px(&img, 0, 0), [77, 77, 77]);
    }

    #[test]
    fn crop_keeps_pixel_positions() {
        let mut src = grey(6, 6, 0);
        let i = (3 * 6 + 4) * 4;
        src[i..i + 4].copy_from_slice(&[10, 20, 30, 255]); // B G R at (4,3)
        let img = render_output(&src, 6, &rect(3, 2, 6, 5), &[], None).unwrap();
        assert_eq!(px(&img, 1, 1), [30, 20, 10]);
    }

    #[test]
    fn filled_rect_lands_in_selection_coordinates() {
        let src = grey(20, 20, 0);
        let a = Ann::Fill { r: (6.0, 6.0, 10.0, 10.0), color: 0xff0000 };
        let img = render_output(&src, 20, &rect(5, 5, 15, 15), &[a], None).unwrap();
        assert_eq!(px(&img, 2, 2), [255, 0, 0]); // desktop (7,7)
        assert_eq!(px(&img, 0, 0), [0, 0, 0]);
        assert_eq!(px(&img, 8, 8), [0, 0, 0]);
    }

    #[test]
    fn rect_outline_and_arrow_and_pen_draw_something() {
        let src = grey(60, 60, 0);
        let anns = vec![
            Ann::Rect { r: (10.0, 10.0, 40.0, 40.0), color: 0x00ff00, w: 3.0 },
            Ann::Arrow { a: (5.0, 50.0), b: (50.0, 50.0), color: 0x0000ff, w: 5.0 },
            Ann::Pen { pts: vec![(5.0, 5.0), (20.0, 8.0), (30.0, 5.0)], color: 0xffffff, w: 3.0 },
        ];
        let img = render_output(&src, 60, &rect(0, 0, 60, 60), &anns, None).unwrap();
        assert_eq!(px(&img, 25, 10)[1], 255); // top edge of the rectangle
        assert_eq!(px(&img, 25, 25), [0, 0, 0]); // inside stays untouched
        assert_eq!(px(&img, 30, 50), [0, 0, 255]); // arrow shaft
        assert!(px(&img, 20, 8)[0] > 100); // pen
    }

    #[test]
    fn selection_outside_the_picture_is_an_error() {
        let src = grey(4, 4, 0);
        assert!(render_output(&src, 4, &rect(0, 0, 9, 9), &[], None).is_err());
    }
}
