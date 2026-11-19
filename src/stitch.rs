//! Scrolling capture: stitches consecutive screen grabs of a scrolling area into one tall image.
//!
//! Pure and platform independent (frames are top-down BGRA), so it is covered by unit tests.
//!
//! Method: every frame row is reduced to a 64-bit hash (pixels lightly quantised, the right-hand
//! strip that may hold a scrollbar is ignored). For a new frame the vertical scroll distance `dy`
//! against the previous frame is the shift at which the most rows of the two frames are equal;
//! uniform (blank) rows carry no information and are not counted. Only the strip that is new is
//! appended, so every real row appears exactly once.
//!
//! Fixed page chrome: rows at the top and at the bottom that are identical in both frames at the
//! same position (sticky headers, cookie bars, fixed footers) are excluded from the search. They
//! are never copied from later frames: the header stays where the first frame put it, and the
//! footer of the last frame is added once at the very bottom.

/// One grab: top-down BGRA, alpha ignored.
#[derive(Clone)]
pub struct Frame {
    pub w: usize,
    pub h: usize,
    pub bgra: Vec<u8>,
}

/// The finished stitched picture (top-down BGRA).
pub struct Stitched {
    pub w: usize,
    pub h: usize,
    pub bgra: Vec<u8>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Push {
    /// The frame scrolled by `dy` pixels; `rows` new rows were added.
    Appended { dy: usize, rows: usize },
    /// The content did not move: the end of the page.
    End,
    /// No reliable overlap with the previous frame (animation, video, a jump). Nothing was added.
    NoMatch,
    /// The height limit was reached; the result was cut there.
    Limit,
}

/// Minimum share of matching rows in the overlap.
const MIN_RATIO: f32 = 0.80;
/// Minimum number of matching non-blank rows.
const MIN_MATCH: usize = 12;
/// Share of identical rows above which a frame counts as "did not move".
const SAME_RATIO: f32 = 0.985;
/// Same, when no scroll distance explains the frame at all.
const LOOSE_SAME_RATIO: f32 = 0.85;

/// Luma buckets per row for the tolerant matcher.
const BUCKETS: usize = 48;

struct Rows {
    /// `BUCKETS` average luma values per row.
    sig: Vec<u8>,
    hash: Vec<u64>,
    /// True when every compared pixel of the row is (nearly) the same colour.
    flat: Vec<bool>,
}

pub struct Stitcher {
    w: usize,
    h: usize,
    /// Compared width: the right-hand strip with a possible scrollbar is left out.
    cw: usize,
    canvas: Vec<u8>,
    rows: usize,
    /// The canvas covers the content up to this row of `prev`.
    cb: usize,
    prev: Frame,
    prev_rows: Rows,
    hint: usize,
    max_rows: usize,
    frames: usize,
    limit_hit: bool,
}

fn compare_width(w: usize) -> usize {
    if w >= 64 {
        w - (w / 8).min(40)
    } else {
        w
    }
}

fn row_info(f: &Frame, cw: usize) -> Rows {
    let mut hash = Vec::with_capacity(f.h);
    let mut flat = Vec::with_capacity(f.h);
    let mut sig = Vec::with_capacity(f.h * BUCKETS);
    let nb = BUCKETS.min(cw).max(1);
    for y in 0..f.h {
        let row = &f.bgra[y * f.w * 4..y * f.w * 4 + cw * 4];
        for b in 0..BUCKETS {
            let (x0, x1) = if b < nb { (b * cw / nb, (b + 1) * cw / nb) } else { (0, 0) };
            let mut sum = 0u32;
            for px in row[x0 * 4..x1 * 4].chunks_exact(4) {
                sum += (px[0] as u32 * 29 + px[1] as u32 * 150 + px[2] as u32 * 77) >> 8;
            }
            sig.push(if x1 > x0 { (sum / (x1 - x0) as u32) as u8 } else { 0 });
        }
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut first = 0u32;
        let mut same = true;
        for (i, px) in row.chunks_exact(4).enumerate() {
            let v = u32::from_le_bytes([px[0], px[1], px[2], 0]) & 0x00FC_FCFC;
            if i == 0 {
                first = v;
            } else if v != first {
                same = false;
            }
            h = (h ^ v as u64).wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash.push(h);
        flat.push(same);
    }
    Rows { sig, hash, flat }
}

impl Stitcher {
    /// `hint` is the expected scroll distance in pixels (used to break ties on repeating content).
    pub fn new(first: Frame, hint: usize, max_rows: usize) -> Stitcher {
        let (w, h) = (first.w, first.h);
        let cw = compare_width(w);
        let prev_rows = row_info(&first, cw);
        Stitcher {
            w,
            h,
            cw,
            canvas: first.bgra.clone(),
            rows: h,
            cb: h,
            prev: first,
            prev_rows,
            hint: hint.max(1),
            max_rows: max_rows.max(h),
            frames: 1,
            limit_hit: false,
        }
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn frames(&self) -> usize {
        self.frames
    }

    /// Adds the next grab. Frames must have the size of the first one.
    pub fn push(&mut self, f: Frame) -> Push {
        if self.limit_hit {
            return Push::Limit;
        }
        if f.w != self.w || f.h != self.h {
            return Push::NoMatch;
        }
        let h = self.h;
        let fr = row_info(&f, self.cw);
        let pr = &self.prev_rows;

        // Did the content move at all?
        let mut inf = 0usize;
        let mut same = 0usize;
        for y in 0..h {
            if !fr.flat[y] || !pr.flat[y] {
                inf += 1;
                if fr.hash[y] == pr.hash[y] {
                    same += 1;
                }
            }
        }
        if inf == 0 || same as f32 >= SAME_RATIO * inf as f32 {
            return Push::End;
        }

        // Fixed rows at the top and bottom.
        let mut ht = (0..h).take_while(|&y| fr.hash[y] == pr.hash[y]).count().min(h - 1);
        while ht > 0 && pr.hash[ht - 1] == pr.hash[ht] {
            ht -= 1;
        }
        let mut fb = (0..h).take_while(|&k| fr.hash[h - 1 - k] == pr.hash[h - 1 - k]).count().min(h - 1);
        while fb > 0 && h - fb >= 1 && pr.hash[h - fb] == pr.hash[h - fb - 1] {
            fb -= 1;
        }
        if (ht + fb) * 10 > h * 6 {
            ht = 0;
            fb = 0;
        }

        let Some(dy) = find_dy(pr, &fr, h, ht, fb, self.hint).or_else(|| find_dy_tolerant(pr, &fr, h, ht, fb, self.hint)) else {
            // No shift fits, but most rows are still where they were: the content did not move
            // (a caret, a hover or a selection changed a few rows).
            if same as f32 >= LOOSE_SAME_RATIO * inf as f32 {
                return Push::End;
            }
            return Push::NoMatch;
        };

        // Drop the previous footer (it hides real content), then append the new strip.
        let end = h - fb;
        let cb = self.cb.min(end);
        if cb < dy + ht {
            return Push::NoMatch;
        }
        let start = cb - dy;
        if start > end {
            return Push::NoMatch;
        }
        let trunc = self.cb - cb;
        self.canvas.truncate((self.rows - trunc) * self.w * 4);
        self.rows -= trunc;

        let mut take = end - start;
        let room = self.max_rows.saturating_sub(fb).saturating_sub(self.rows);
        let mut limited = false;
        if take > room {
            take = room;
            limited = true;
        }
        self.canvas.extend_from_slice(&f.bgra[start * self.w * 4..(start + take) * self.w * 4]);
        self.rows += take;
        self.cb = end;
        self.hint = dy;
        self.frames += 1;
        self.prev = f;
        self.prev_rows = fr;
        if limited {
            self.limit_hit = true;
            // keep `cb` consistent: nothing below the cut is part of the canvas
            self.cb = h;
            return Push::Limit;
        }
        Push::Appended { dy, rows: take }
    }

    /// The stitched image: the canvas plus the fixed footer of the last frame (once).
    pub fn finish(mut self) -> Stitched {
        if !self.limit_hit && self.cb < self.h {
            let tail = &self.prev.bgra[self.cb * self.w * 4..self.h * self.w * 4];
            self.canvas.extend_from_slice(tail);
            self.rows += self.h - self.cb;
        }
        Stitched { w: self.w, h: self.rows, bgra: self.canvas }
    }
}

/// Best scroll distance of `f` against `p` (content of `f` row `r` is row `r + dy` of `p`).
fn find_dy(p: &Rows, f: &Rows, h: usize, ht: usize, fb: usize, hint: usize) -> Option<usize> {
    let body_end = h - fb;
    if body_end <= ht + 8 {
        return None;
    }
    let bh = body_end - ht;
    let min_overlap = (bh / 6).max(16).min(bh);
    if bh <= min_overlap {
        return None;
    }
    let mut best: Vec<(usize, i64)> = Vec::new();
    let mut best_score = i64::MIN;
    for dy in 1..=(bh - min_overlap) {
        let mut matched = 0usize;
        let mut miss = 0usize;
        for r in ht..body_end - dy {
            if f.hash[r] == p.hash[r + dy] {
                if !f.flat[r] {
                    matched += 1;
                }
            } else {
                miss += 1;
            }
        }
        if matched < MIN_MATCH || (matched as f32) < MIN_RATIO * (matched + miss) as f32 {
            continue;
        }
        let score = matched as i64 - 2 * miss as i64;
        if score > best_score {
            best_score = score;
        }
        best.push((dy, score));
    }
    let slack = (best_score / 40).max(3);
    best.into_iter()
        .filter(|&(_, s)| s >= best_score - slack)
        .min_by_key(|&(dy, _)| dy.abs_diff(hint))
        .map(|(dy, _)| dy)
}

/// Like `find_dy`, but rows only have to look alike. Browsers scroll by fractions of a pixel and
/// redraw text slightly differently, so exact row hashes can all disagree.
fn find_dy_tolerant(p: &Rows, f: &Rows, h: usize, ht: usize, fb: usize, hint: usize) -> Option<usize> {
    const ROW_TOL: u32 = (BUCKETS as u32) * 12;
    const RATIO: f32 = 0.60;
    let body_end = h - fb;
    if body_end <= ht + 8 {
        return None;
    }
    let bh = body_end - ht;
    let min_overlap = (bh / 6).max(16).min(bh);
    if bh <= min_overlap {
        return None;
    }
    let sad = |a: usize, b: usize| -> u32 {
        let (x, y) = (&f.sig[a * BUCKETS..(a + 1) * BUCKETS], &p.sig[b * BUCKETS..(b + 1) * BUCKETS]);
        let mut t = 0u32;
        for i in 0..BUCKETS {
            t += (x[i] as i32 - y[i] as i32).unsigned_abs();
            if t > ROW_TOL {
                return t;
            }
        }
        t
    };
    let mut scores: Vec<(usize, i64, usize)> = Vec::new(); // (dy, score, matched)
    for dy in 1..=(bh - min_overlap) {
        let mut matched = 0usize;
        let mut inf = 0usize;
        for r in ht..body_end - dy {
            if f.flat[r] && p.flat[r + dy] {
                continue;
            }
            inf += 1;
            if sad(r, r + dy) <= ROW_TOL {
                matched += 1;
            }
        }
        if matched < MIN_MATCH * 2 || (matched as f32) < RATIO * inf as f32 {
            continue;
        }
        scores.push((dy, matched as i64 - 2 * (inf - matched) as i64, matched));
    }
    let best = scores.iter().map(|s| s.1).max()?;
    let slack = (best / 40).max(3);
    scores.iter().filter(|s| s.1 >= best - slack).min_by_key(|s| s.0.abs_diff(hint)).map(|s| s.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: usize = 120;

    /// Deterministic noise: every page row is unique and not flat.
    fn noise(a: u64, b: u64) -> u32 {
        let mut x = a.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ b.wrapping_mul(0xC2B2_AE3D_27D4_EB4F) ^ 0x1234_5678;
        x ^= x >> 29;
        x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
        x ^= x >> 32;
        (x as u32) | 0xFF00_0000
    }

    fn px(out: &mut Vec<u8>, v: u32) {
        out.extend_from_slice(&v.to_le_bytes());
    }

    /// A tall page: row `y` is unique noise, except every `gap_every`-th block of rows that is blank.
    fn page_row(y: usize, blanks: bool) -> Vec<u8> {
        let mut r = Vec::with_capacity(W * 4);
        if blanks && (y / 9) % 3 == 0 {
            for _ in 0..W {
                px(&mut r, 0xFFFF_FFFF);
            }
        } else {
            for x in 0..W {
                px(&mut r, noise(y as u64, x as u64));
            }
        }
        r
    }

    fn fixed_row(tag: u64, i: usize) -> Vec<u8> {
        let mut r = Vec::with_capacity(W * 4);
        for x in 0..W {
            px(&mut r, noise(tag * 1_000_003 + i as u64, x as u64 + 77));
        }
        r
    }

    struct Scene {
        total: usize,
        h: usize,
        ht: usize,
        fb: usize,
        blanks: bool,
    }

    impl Scene {
        fn frame(&self, y0: usize) -> Frame {
            let mut bgra = Vec::new();
            for r in 0..self.h {
                if r < self.ht {
                    bgra.extend(fixed_row(1, r));
                } else if r >= self.h - self.fb {
                    bgra.extend(fixed_row(2, r - (self.h - self.fb)));
                } else {
                    bgra.extend(page_row(y0 + r, self.blanks));
                }
            }
            Frame { w: W, h: self.h, bgra }
        }

        /// What a perfect stitch gives: header, the page without the rows hidden by the chrome, footer.
        fn expected(&self) -> Vec<u8> {
            let mut out = Vec::new();
            for r in 0..self.ht {
                out.extend(fixed_row(1, r));
            }
            for y in self.ht..self.total - self.fb {
                out.extend(page_row(y, self.blanks));
            }
            for r in 0..self.fb {
                out.extend(fixed_row(2, r));
            }
            out
        }

        /// Scrolls by `step` until the end; the last frame is clamped like a real page.
        fn run(&self, step: usize) -> Stitched {
            let mut y0 = 0;
            let mut st = Stitcher::new(self.frame(0), step, 100_000);
            loop {
                let next = (y0 + step).min(self.total - self.h);
                let r = st.push(self.frame(next));
                if next == y0 {
                    assert_eq!(r, Push::End);
                    break;
                }
                assert!(matches!(r, Push::Appended { .. }), "step {step}, y0 {next}: {r:?}");
                y0 = next;
            }
            st.finish()
        }
    }

    fn assert_same(got: &Stitched, want: &[u8]) {
        assert_eq!(got.w, W);
        assert_eq!(got.bgra.len(), want.len(), "rows {} vs {}", got.h, want.len() / (W * 4));
        let rows_got = got.bgra.chunks(W * 4);
        let rows_want = want.chunks(W * 4);
        for (i, (a, b)) in rows_got.zip(rows_want).enumerate() {
            assert!(a == b, "row {i} differs");
        }
    }

    #[test]
    fn reconstructs_exactly_for_different_steps() {
        for step in [40, 77, 100, 133, 150] {
            let s = Scene { total: 1000, h: 200, ht: 0, fb: 0, blanks: false };
            assert_same(&s.run(step), &s.expected());
        }
    }

    #[test]
    fn page_with_blank_bands() {
        for step in [60, 111] {
            let s = Scene { total: 900, h: 200, ht: 0, fb: 0, blanks: true };
            assert_same(&s.run(step), &s.expected());
        }
    }

    #[test]
    fn sticky_header_and_footer_are_drawn_once() {
        for step in [70, 120] {
            let s = Scene { total: 1100, h: 220, ht: 30, fb: 24, blanks: false };
            assert_same(&s.run(step), &s.expected());
        }
    }

    #[test]
    fn header_only_and_footer_only() {
        let s = Scene { total: 800, h: 200, ht: 40, fb: 0, blanks: false };
        assert_same(&s.run(90), &s.expected());
        let s = Scene { total: 800, h: 200, ht: 0, fb: 40, blanks: false };
        assert_same(&s.run(90), &s.expected());
    }

    #[test]
    fn last_frame_overlaps_more() {
        // 1000 is not a multiple of the step: the final scroll is shorter.
        let s = Scene { total: 1003, h: 200, ht: 0, fb: 0, blanks: false };
        assert_same(&s.run(120), &s.expected());
    }

    #[test]
    fn identical_frame_is_the_end() {
        let s = Scene { total: 600, h: 200, ht: 20, fb: 0, blanks: false };
        let mut st = Stitcher::new(s.frame(0), 80, 10_000);
        assert_eq!(st.push(s.frame(0)), Push::End);
        assert!(matches!(st.push(s.frame(80)), Push::Appended { .. }));
        assert_eq!(st.push(s.frame(80)), Push::End);
    }

    #[test]
    fn unrelated_frame_stops_cleanly() {
        let s = Scene { total: 600, h: 200, ht: 0, fb: 0, blanks: false };
        let mut st = Stitcher::new(s.frame(0), 80, 10_000);
        assert!(matches!(st.push(s.frame(80)), Push::Appended { .. }));
        let rows_before = st.rows();
        // an animation: completely different content
        let mut bgra = Vec::new();
        for r in 0..200u64 {
            for x in 0..W as u64 {
                px(&mut bgra, noise(r + 50_000, x + 9));
            }
        }
        assert_eq!(st.push(Frame { w: W, h: 200, bgra }), Push::NoMatch);
        assert_eq!(st.rows(), rows_before);
        // what was stitched so far is still a clean image
        let out = st.finish();
        assert_eq!(out.h, 280);
    }

    #[test]
    fn height_limit_cuts_the_result() {
        let s = Scene { total: 2000, h: 200, ht: 0, fb: 0, blanks: false };
        let mut st = Stitcher::new(s.frame(0), 100, 450);
        let mut last = Push::End;
        for y in (100..2000 - 200).step_by(100) {
            last = st.push(s.frame(y));
            if last == Push::Limit {
                break;
            }
        }
        assert_eq!(last, Push::Limit);
        let out = st.finish();
        assert_eq!(out.h, 450);
        // the cut part is still exact
        assert!(out.bgra == s.expected()[..450 * W * 4]);
    }

    #[test]
    fn slightly_redrawn_rows_still_match() {
        // A fractional scroll redraws text a little differently: every pixel is off by a few levels.
        let s = Scene { total: 700, h: 200, ht: 0, fb: 0, blanks: false };
        let mut st = Stitcher::new(s.frame(0), 90, 10_000);
        let mut f = s.frame(90);
        for (i, v) in f.bgra.iter_mut().enumerate() {
            if i % 4 != 3 {
                *v = if (i / 4) % 3 == 0 { v.saturating_add(7) } else { v.saturating_sub(5) };
            }
        }
        let r = st.push(f);
        assert!(matches!(r, Push::Appended { dy: 90, .. }), "{r:?}");
    }

    #[test]
    fn scrollbar_strip_is_ignored() {
        // the last 12 columns hold a "scrollbar" that changes from frame to frame
        let s = Scene { total: 700, h: 200, ht: 0, fb: 0, blanks: false };
        let mut st = Stitcher::new(s.frame(0), 90, 10_000);
        let mut f = s.frame(90);
        for y in 0..f.h {
            for x in W - 12..W {
                let i = (y * W + x) * 4;
                f.bgra[i] = f.bgra[i].wrapping_add(90);
            }
        }
        assert!(matches!(st.push(f), Push::Appended { dy: 90, .. }));
    }
}
