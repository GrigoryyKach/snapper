//! The annotation model shared by both platforms: pure data and geometry, no drawing API.

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

pub fn arrow_head_len(w: f32) -> f32 {
    (w * 4.0).max(14.0)
}

/// Shaft end and the three head corners (tip, left, right) of an arrow from `a` to `b`.
#[cfg_attr(windows, allow(dead_code))]
pub fn arrow_points(a: Pf, b: Pf, w: f32) -> Option<(Pf, [Pf; 3])> {
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
    Some(((b.0 - ux * hl * 0.6, b.1 - uy * hl * 0.6), [b, left, right]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arrow_points_form_a_head() {
        let (shaft, [tip, l, r]) = arrow_points((0.0, 0.0), (100.0, 0.0), 5.0).unwrap();
        assert_eq!(tip, (100.0, 0.0));
        assert!((shaft.0 - 88.0).abs() < 1e-3 && shaft.1.abs() < 1e-3); // head len 20, shaft ends 60% in
        assert!((l.0 - 80.0).abs() < 1e-3 && (r.0 - 80.0).abs() < 1e-3);
        assert!((l.1 + r.1).abs() < 1e-3 && (l.1 - r.1).abs() > 1.0);
    }

    #[test]
    fn degenerate_arrow_is_none() {
        assert!(arrow_points((5.0, 5.0), (5.2, 5.2), 5.0).is_none());
    }

    #[test]
    fn rect_is_normalised() {
        assert_eq!(norm_rect((10.0, 2.0), (3.0, 9.0)), (3.0, 2.0, 10.0, 9.0));
    }
}
