//! Pure geometry for the capture overlay: rectangles, selection handles, resize/move math and
//! toolbar placement. Everything is in physical pixels of the virtual desktop (coordinates can be
//! negative when a monitor sits left of or above the primary one). No Win32 here, so it is unit-tested.

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Pt {
    pub x: i32,
    pub y: i32,
}

pub const fn pt(x: i32, y: i32) -> Pt {
    Pt { x, y }
}

/// Half-open rectangle: `l..r`, `t..b`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Rect {
    pub l: i32,
    pub t: i32,
    pub r: i32,
    pub b: i32,
}

pub const fn rect(l: i32, t: i32, r: i32, b: i32) -> Rect {
    Rect { l, t, r, b }
}

impl Rect {
    pub fn w(&self) -> i32 {
        self.r - self.l
    }
    pub fn h(&self) -> i32 {
        self.b - self.t
    }
    pub fn contains(&self, p: Pt) -> bool {
        p.x >= self.l && p.x < self.r && p.y >= self.t && p.y < self.b
    }
    pub fn intersect(&self, o: &Rect) -> Option<Rect> {
        let r = rect(self.l.max(o.l), self.t.max(o.t), self.r.min(o.r), self.b.min(o.b));
        (r.r > r.l && r.b > r.t).then_some(r)
    }
    pub fn union(&self, o: &Rect) -> Rect {
        rect(self.l.min(o.l), self.t.min(o.t), self.r.max(o.r), self.b.max(o.b))
    }
    pub fn inflate(&self, d: i32) -> Rect {
        rect(self.l - d, self.t - d, self.r + d, self.b + d)
    }
    pub fn offset(&self, dx: i32, dy: i32) -> Rect {
        rect(self.l + dx, self.t + dy, self.r + dx, self.b + dy)
    }
    pub fn contains_rect(&self, o: &Rect) -> bool {
        o.l >= self.l && o.r <= self.r && o.t >= self.t && o.b <= self.b
    }
    pub fn center(&self) -> Pt {
        pt((self.l + self.r) / 2, (self.t + self.b) / 2)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Handle {
    TopLeft,
    Top,
    TopRight,
    Right,
    BottomRight,
    Bottom,
    BottomLeft,
    Left,
}

impl Handle {
    pub const ALL: [Handle; 8] = [
        Handle::TopLeft,
        Handle::Top,
        Handle::TopRight,
        Handle::Right,
        Handle::BottomRight,
        Handle::Bottom,
        Handle::BottomLeft,
        Handle::Left,
    ];
}

/// Which resize cursor a handle needs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ResizeCursor {
    NwSe,
    NeSw,
    Ns,
    We,
}

pub fn handle_cursor(h: Handle) -> ResizeCursor {
    match h {
        Handle::TopLeft | Handle::BottomRight => ResizeCursor::NwSe,
        Handle::TopRight | Handle::BottomLeft => ResizeCursor::NeSw,
        Handle::Top | Handle::Bottom => ResizeCursor::Ns,
        Handle::Left | Handle::Right => ResizeCursor::We,
    }
}

/// Centre of each handle: the corners and the middle of each side.
pub fn handle_point(r: &Rect, h: Handle) -> Pt {
    let (cx, cy) = ((r.l + r.r) / 2, (r.t + r.b) / 2);
    match h {
        Handle::TopLeft => pt(r.l, r.t),
        Handle::Top => pt(cx, r.t),
        Handle::TopRight => pt(r.r, r.t),
        Handle::Right => pt(r.r, cy),
        Handle::BottomRight => pt(r.r, r.b),
        Handle::Bottom => pt(cx, r.b),
        Handle::BottomLeft => pt(r.l, r.b),
        Handle::Left => pt(r.l, cy),
    }
}

/// Handle under `p` (square hit area of `half` pixels around its centre). Corners win over sides.
pub fn hit_handle(r: &Rect, p: Pt, half: i32) -> Option<Handle> {
    let order = [
        Handle::TopLeft,
        Handle::TopRight,
        Handle::BottomRight,
        Handle::BottomLeft,
        Handle::Top,
        Handle::Right,
        Handle::Bottom,
        Handle::Left,
    ];
    order.into_iter().find(|&h| {
        let c = handle_point(r, h);
        (p.x - c.x).abs() <= half && (p.y - c.y).abs() <= half
    })
}

/// New position of one axis while one edge follows the pointer. `fixed` is the opposite edge.
/// Crossing the fixed edge flips the rectangle; the result is never smaller than `min`.
fn axis(fixed: i32, moving: i32, min: i32, lo: i32, hi: i32) -> (i32, i32) {
    let moving = moving.clamp(lo, hi);
    let (mut a, mut b) = if moving >= fixed { (fixed, moving) } else { (moving, fixed) };
    if b - a < min {
        if moving >= fixed {
            b = a + min;
            if b > hi {
                b = hi;
                a = b - min;
            }
        } else {
            a = b - min;
            if a < lo {
                a = lo;
                b = a + min;
            }
        }
    }
    (a, b)
}

/// Resizes `orig` by dragging `handle` to pointer `p`, staying inside `bounds`, at least `min` px.
pub fn resize(orig: &Rect, handle: Handle, p: Pt, min: i32, bounds: &Rect) -> Rect {
    let mut out = *orig;
    let (mv_l, mv_r, mv_t, mv_b) = match handle {
        Handle::TopLeft => (true, false, true, false),
        Handle::Top => (false, false, true, false),
        Handle::TopRight => (false, true, true, false),
        Handle::Right => (false, true, false, false),
        Handle::BottomRight => (false, true, false, true),
        Handle::Bottom => (false, false, false, true),
        Handle::BottomLeft => (true, false, false, true),
        Handle::Left => (true, false, false, false),
    };
    if mv_l || mv_r {
        let fixed = if mv_l { orig.r } else { orig.l };
        let (a, b) = axis(fixed, p.x, min, bounds.l, bounds.r);
        out.l = a;
        out.r = b;
    }
    if mv_t || mv_b {
        let fixed = if mv_t { orig.b } else { orig.t };
        let (a, b) = axis(fixed, p.y, min, bounds.t, bounds.b);
        out.t = a;
        out.b = b;
    }
    out
}

/// Moves `orig` by (`dx`, `dy`), clamped so the whole rectangle stays inside `bounds`.
pub fn move_rect(orig: &Rect, dx: i32, dy: i32, bounds: &Rect) -> Rect {
    let dx = dx.clamp(bounds.l - orig.l, bounds.r - orig.r);
    let dy = dy.clamp(bounds.t - orig.t, bounds.b - orig.b);
    orig.offset(dx, dy)
}

/// A monitor as seen by the placement code: its rectangle and the toolbar size for its DPI.
#[derive(Clone, Copy, Debug)]
pub struct Place {
    pub rect: Rect,
    pub toolbar: (i32, i32),
}

/// Places the toolbar just below the selection's bottom-right corner, right-aligned with it.
/// Falls back to above the selection, then inside it, then clamps into the corner monitor.
/// The result always lies fully inside one monitor. Returns the rectangle and the monitor index.
pub fn place_toolbar(sel: &Rect, monitors: &[Place], gap: i32) -> Option<(Rect, usize)> {
    if monitors.is_empty() {
        return None;
    }
    let corner = pt(sel.r - 1, sel.b - 1);
    // Monitors ordered: the one under the bottom-right corner first, then by distance to it.
    let mut order: Vec<usize> = (0..monitors.len()).collect();
    order.sort_by_key(|&i| {
        let m = &monitors[i].rect;
        let dx = (m.l - corner.x).max(0).max(corner.x - (m.r - 1));
        let dy = (m.t - corner.y).max(0).max(corner.y - (m.b - 1));
        (dx as i64) * (dx as i64) + (dy as i64) * (dy as i64)
    });
    for &i in &order {
        let m = &monitors[i];
        let (w, h) = m.toolbar;
        let x = (sel.r - w).clamp(m.rect.l, (m.rect.r - w).max(m.rect.l));
        let candidates = [
            rect(x, sel.b + gap, x + w, sel.b + gap + h),
            rect(x, sel.t - gap - h, x + w, sel.t - gap),
            rect(x, sel.b - gap - h, x + w, sel.b - gap),
        ];
        for c in candidates {
            if m.rect.contains_rect(&c) {
                return Some((c, i));
            }
        }
    }
    // Nothing fits cleanly (tiny monitor?): clamp into the corner monitor.
    let i = order[0];
    let m = &monitors[i];
    let (w, h) = m.toolbar;
    let x = (sel.r - w).clamp(m.rect.l, (m.rect.r - w).max(m.rect.l));
    let y = (sel.b + gap).clamp(m.rect.t, (m.rect.b - h).max(m.rect.t));
    Some((rect(x, y, x + w, y + h), i))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOUNDS: Rect = rect(-1920, 0, 3840, 1080);

    #[test]
    fn rect_size_works_in_negative_space() {
        let r = rect(-100, -50, 100, 50);
        assert_eq!((r.w(), r.h()), (200, 100));
        assert!(r.contains(pt(-100, -50)) && !r.contains(pt(100, 0)));
        assert_eq!(r.intersect(&rect(0, 0, 500, 500)), Some(rect(0, 0, 100, 50)));
        assert_eq!(r.intersect(&rect(100, 0, 200, 10)), None);
    }

    #[test]
    fn handles_have_expected_points() {
        let r = rect(-200, 100, -100, 160);
        assert_eq!(handle_point(&r, Handle::TopLeft), pt(-200, 100));
        assert_eq!(handle_point(&r, Handle::Bottom), pt(-150, 160));
        assert_eq!(handle_point(&r, Handle::Right), pt(-100, 130));
        assert_eq!(Handle::ALL.len(), 8);
    }

    #[test]
    fn hit_testing_prefers_corners_and_works_negative() {
        let r = rect(-300, 10, -100, 90);
        assert_eq!(hit_handle(&r, pt(-298, 12), 6), Some(Handle::TopLeft));
        assert_eq!(hit_handle(&r, pt(-200, 8), 6), Some(Handle::Top));
        assert_eq!(hit_handle(&r, pt(-100, 50), 6), Some(Handle::Right));
        assert_eq!(hit_handle(&r, pt(-200, 50), 6), None);
        // tiny selection: the corner wins over the side handles
        let tiny = rect(0, 0, 4, 4);
        assert_eq!(hit_handle(&tiny, pt(0, 0), 6), Some(Handle::TopLeft));
    }

    #[test]
    fn resize_edges_and_corners() {
        let r = rect(100, 100, 300, 200);
        assert_eq!(resize(&r, Handle::Right, pt(400, 150), 2, &BOUNDS), rect(100, 100, 400, 200));
        assert_eq!(resize(&r, Handle::TopLeft, pt(50, 20), 2, &BOUNDS), rect(50, 20, 300, 200));
        assert_eq!(resize(&r, Handle::Bottom, pt(0, 250), 2, &BOUNDS), rect(100, 100, 300, 250));
    }

    #[test]
    fn resize_flips_over_the_opposite_edge() {
        let r = rect(100, 100, 300, 200);
        assert_eq!(resize(&r, Handle::Left, pt(400, 0), 2, &BOUNDS), rect(300, 100, 400, 200));
        assert_eq!(resize(&r, Handle::BottomRight, pt(40, 30), 2, &BOUNDS), rect(40, 30, 100, 100));
    }

    #[test]
    fn resize_keeps_minimum_and_bounds() {
        let r = rect(100, 100, 300, 200);
        let m = resize(&r, Handle::Right, pt(100, 150), 2, &BOUNDS);
        assert_eq!((m.w(), m.h()), (2, 100));
        let c = resize(&r, Handle::Right, pt(99999, 150), 2, &BOUNDS);
        assert_eq!(c.r, 3840);
        let neg = resize(&r, Handle::Left, pt(-99999, 150), 2, &BOUNDS);
        assert_eq!(neg.l, -1920);
    }

    #[test]
    fn resize_in_negative_space() {
        let r = rect(-500, 100, -300, 200);
        assert_eq!(resize(&r, Handle::TopRight, pt(-100, 40), 2, &BOUNDS), rect(-500, 40, -100, 200));
        let tiny = resize(&r, Handle::Left, pt(-299, 150), 2, &BOUNDS);
        assert_eq!((tiny.l, tiny.r), (-300, -298)); // crossed the fixed edge: flipped and widened to the minimum
    }

    #[test]
    fn move_is_clamped_to_bounds() {
        let r = rect(0, 100, 200, 300);
        assert_eq!(move_rect(&r, -5000, 0, &BOUNDS), rect(-1920, 100, -1720, 300));
        assert_eq!(move_rect(&r, 50, 5000, &BOUNDS), rect(50, 880, 250, 1080));
        assert_eq!(move_rect(&r, 10, 10, &BOUNDS), rect(10, 110, 210, 310));
    }

    fn three() -> Vec<Place> {
        [rect(-1920, 0, 0, 1080), rect(0, 0, 1920, 1080), rect(1920, 0, 3840, 1080)]
            .into_iter()
            .map(|rect| Place { rect, toolbar: (420, 48) })
            .collect()
    }

    #[test]
    fn toolbar_goes_below_the_bottom_right_corner() {
        let (tb, mon) = place_toolbar(&rect(500, 200, 900, 500), &three(), 8).unwrap();
        assert_eq!(tb, rect(480, 508, 900, 556));
        assert_eq!(mon, 1);
    }

    #[test]
    fn toolbar_flips_above_then_inside() {
        let mons = three();
        let (above, _) = place_toolbar(&rect(500, 200, 900, 1070), &mons, 8).unwrap();
        assert_eq!(above, rect(480, 144, 900, 192));
        // full-height selection: no room above or below, so it goes inside
        let (inside, _) = place_toolbar(&rect(500, 0, 900, 1080), &mons, 8).unwrap();
        assert_eq!(inside, rect(480, 1024, 900, 1072));
        assert!(mons[1].rect.contains_rect(&inside));
    }

    #[test]
    fn toolbar_stays_on_one_monitor_across_boundaries() {
        let mons = three();
        // selection spans monitors 0 and 1, bottom-right corner on monitor 1 near its left edge
        let (tb, mon) = place_toolbar(&rect(-800, 100, 100, 400), &mons, 8).unwrap();
        assert_eq!(mon, 1);
        assert!(mons[1].rect.contains_rect(&tb), "{tb:?}");
        // selection covering all three monitors
        let (tb, mon) = place_toolbar(&rect(-1920, 0, 3840, 1080), &mons, 8).unwrap();
        assert!(mons[mon].rect.contains_rect(&tb), "{tb:?}");
    }

    #[test]
    fn toolbar_on_left_monitor_with_negative_coordinates() {
        let mons = three();
        let (tb, mon) = place_toolbar(&rect(-1800, 100, -1500, 300), &mons, 8).unwrap();
        assert_eq!(mon, 0);
        assert!(mons[0].rect.contains_rect(&tb));
        assert_eq!(tb.t, 308);
    }

    #[test]
    fn toolbar_clamps_horizontally() {
        // selection hugging the left edge of the primary monitor: the toolbar must not leave it
        let (tb, _) = place_toolbar(&rect(0, 100, 50, 200), &three(), 8).unwrap();
        assert!(three()[1].rect.contains_rect(&tb) || three()[0].rect.contains_rect(&tb));
    }
}
