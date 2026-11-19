//! Scrolling capture where the user scrolls: stitches the frames sampled from a live region into
//! one tall image, page by page.
//!
//! The canvas is a list of *segments*, one per page (a page that was reached by clicking a link,
//! "next page", and so on). Each segment keeps its rows plus their signatures, and the stitcher keeps
//! a pointer: where the last frame sat in its segment. For a new frame:
//!
//! - it is located against the canvas near the pointer first (exact row hashes, then a tolerant
//!   luma comparison), then in the whole segment, then in all other segments;
//! - found below the segment's bottom: only the rows below are appended, to that segment;
//! - found inside the captured rows (scrolling up, or back down over captured content): the pointer
//!   moves and nothing is appended, so nothing is ever duplicated, and returning to an earlier page
//!   adds nothing;
//! - not found anywhere: wait until the view is stable, then start a new segment. Leading rows that
//!   equal the top of an earlier page (the site header, a search box) are not repeated.
//!
//! Fixed rows (sticky headers and footers) are excluded from the comparison; the header is kept
//! once at the top of its page and the footer of the last frame is added once at the very bottom.
//! Blank and half-rendered frames are never appended: new rows need a stable view first.

use crate::stitch::{compare_width, row_info, Frame, Rows, Stitched, BUCKETS};

/// Minimum matching non-blank rows.
const MIN_MATCH: usize = 12;
/// Minimum share of matching rows (exact comparison).
const MIN_RATIO: f32 = 0.80;
/// The same for the tolerant comparison.
const TOL_RATIO: f32 = 0.65;
/// Tolerance of the luma comparison: average level difference per bucket.
const ROW_TOL: u32 = BUCKETS as u32 * 5;
/// No single bucket may differ more than this: other text in the same row fails here.
const BUCKET_TOL: u32 = 14;
/// A frame that fits nowhere starts a new page only after this many identical samples.
pub const NEW_PAGE_STABLE: usize = 5;
/// New rows are appended only after this many identical samples (or when forced after a long motion).
pub const APPEND_STABLE: usize = 2;
pub const MAX_SEGMENTS: usize = 40;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ev {
    /// Same place as before, nothing new.
    Same,
    /// Moved over rows that were already captured.
    Moved,
    /// This many rows were appended below the captured part of a page.
    Appended(usize),
    /// A new page started with this many rows.
    NewPage(usize),
    /// Not decided yet: wait for a more stable view.
    Wait,
    /// A blank or unusable frame.
    Ignored,
    /// A limit was reached; the result is cut there.
    Limit,
}

struct Seg {
    n: usize,
    bgra: Vec<u8>,
    rows: Rows,
}

impl Seg {
    fn from_frame(f: &Frame, fr: &Rows, a: usize, b: usize) -> Seg {
        let w4 = f.w * 4;
        Seg {
            n: b - a,
            bgra: f.bgra[a * w4..b * w4].to_vec(),
            rows: Rows { sig: fr.sig[a * BUCKETS..b * BUCKETS].to_vec(), hash: fr.hash[a..b].to_vec(), flat: fr.flat[a..b].to_vec() },
        }
    }

    fn append(&mut self, f: &Frame, fr: &Rows, a: usize, b: usize) {
        let w4 = f.w * 4;
        self.bgra.extend_from_slice(&f.bgra[a * w4..b * w4]);
        self.rows.sig.extend_from_slice(&fr.sig[a * BUCKETS..b * BUCKETS]);
        self.rows.hash.extend_from_slice(&fr.hash[a..b]);
        self.rows.flat.extend_from_slice(&fr.flat[a..b]);
        self.n += b - a;
    }

    fn truncate(&mut self, keep: usize, w: usize) {
        self.bgra.truncate(keep * w * 4);
        self.rows.sig.truncate(keep * BUCKETS);
        self.rows.hash.truncate(keep);
        self.rows.flat.truncate(keep);
        self.n = keep;
    }
}

pub struct Manual {
    w: usize,
    h: usize,
    cw: usize,
    segs: Vec<Seg>,
    cur: usize,
    /// Row of the current segment at the top of the last frame (negative: the frame's top lies above it).
    ptr: i64,
    prev: Option<Rows>,
    /// Fixed rows at the top and bottom of a frame (sticky header and footer).
    ht: usize,
    fb: usize,
    footer: Vec<u8>,
    max_rows: usize,
    frames: usize,
    limit_hit: bool,
}

/// Leading rows of `f` that equal the top rows of `seg`; trailing uniform rows are trimmed off.
fn lead_eq(f: &Rows, seg: &Seg, h: usize) -> usize {
    let lim = (h / 2).min(seg.n);
    let mut c = (0..lim).take_while(|&r| f.hash[r] == seg.rows.hash[r]).count();
    while c > 0 && c < h && f.hash[c - 1] == f.hash[c] {
        c -= 1;
    }
    c
}

/// Rows at the top and the bottom that are identical at the same place in both frames.
fn fixed_rows(p: &Rows, f: &Rows, h: usize) -> (usize, usize) {
    let mut ht = (0..h).take_while(|&y| f.hash[y] == p.hash[y]).count().min(h - 1);
    while ht > 0 && p.hash[ht - 1] == p.hash[ht] {
        ht -= 1;
    }
    let mut fb = (0..h).take_while(|&k| f.hash[h - 1 - k] == p.hash[h - 1 - k]).count().min(h - 1);
    while fb > 0 && p.hash[h - fb] == p.hash[h - fb - 1] {
        fb -= 1;
    }
    if (ht + fb) * 10 > h * 6 {
        return (0, 0);
    }
    (ht, fb)
}

impl Manual {
    pub fn new(first: Frame, max_rows: usize) -> Manual {
        let (w, h) = (first.w, first.h);
        let cw = compare_width(w);
        let fr = row_info(&first, cw);
        let seg = Seg::from_frame(&first, &fr, 0, h);
        Manual { w, h, cw, segs: vec![seg], cur: 0, ptr: 0, prev: Some(fr), ht: 0, fb: 0, footer: Vec::new(), max_rows: max_rows.max(h), frames: 1, limit_hit: false }
    }

    /// Rows in the result so far (the footer included).
    pub fn rows(&self) -> usize {
        self.segs.iter().map(|s| s.n).sum::<usize>() + self.fb
    }

    pub fn frames(&self) -> usize {
        self.frames
    }

    pub fn pages(&self) -> usize {
        self.segs.len()
    }

    fn min_overlap(&self) -> i64 {
        ((self.h / 6).max(16)) as i64
    }

    /// Best offset of `f` in `seg` within `lo..=hi` (offset = segment row at the frame's top).
    fn best_offset(&self, seg: &Seg, f: &Rows, skip_top: usize, lo: i64, hi: i64, hint: i64, tolerant: bool) -> Option<i64> {
        let h = self.h as i64;
        let n = seg.n as i64;
        let min_ov = self.min_overlap();
        let skip_bot = self.fb as i64;
        let lo = lo.max(-(h - min_ov));
        let hi = hi.min(n - min_ov);
        let mut cands: Vec<(i64, i64)> = Vec::new();
        let mut best = i64::MIN;
        for o in lo..=hi {
            let r0 = (skip_top as i64).max(-o);
            let r1 = (h - skip_bot).min(n - o);
            if r1 - r0 < min_ov {
                continue;
            }
            let (mut matched, mut miss, mut centre) = (0usize, 0usize, 0usize);
            for r in r0..r1 {
                let c = (o + r) as usize;
                let r = r as usize;
                if tolerant {
                    if f.flat[r] && seg.rows.flat[c] {
                        continue;
                    }
                    // a fractional scroll blends neighbouring rows: the best of three source rows counts
                    let a = &f.sig[r * BUCKETS..(r + 1) * BUCKETS];
                    let mut best = u32::MAX;
                    let mut centre_t = u32::MAX;
                    for cc in c.saturating_sub(1)..(c + 2).min(seg.n) {
                        let b = &seg.rows.sig[cc * BUCKETS..(cc + 1) * BUCKETS];
                        let mut t = 0u32;
                        for i in 0..BUCKETS {
                            let d = (a[i] as i32 - b[i] as i32).unsigned_abs();
                            t += d;
                            if t > ROW_TOL || d > BUCKET_TOL {
                                t = u32::MAX;
                                break;
                            }
                        }
                        best = best.min(t);
                        if cc == c {
                            centre_t = t;
                        }
                    }
                    if best <= ROW_TOL {
                        matched += 1;
                        if centre_t <= ROW_TOL {
                            centre += 1;
                        }
                    } else {
                        miss += 1;
                    }
                } else if f.hash[r] == seg.rows.hash[c] {
                    if !f.flat[r] {
                        matched += 1;
                    }
                } else {
                    miss += 1;
                }
            }
            let (need, ratio) = if tolerant { (MIN_MATCH * 2, TOL_RATIO) } else { (MIN_MATCH, MIN_RATIO) };
            if matched < need || (matched as f32) < ratio * (matched + miss) as f32 {
                continue;
            }
            // the exact-row alignment beats its neighbours in the tolerant mode
            let score = matched as i64 - 2 * miss as i64 + centre as i64;
            best = best.max(score);
            cands.push((o, score));
        }
        let slack = (best / 40).max(3);
        cands.into_iter().filter(|&(_, s)| s >= best - slack).min_by_key(|&(o, _)| (o - hint).abs()).map(|(o, _)| o)
    }

    /// Where `f` sits in the canvas: (segment, offset).
    fn locate(&self, f: &Rows) -> Option<(usize, i64)> {
        let h = self.h as i64;
        let skip = |seg: &Seg| self.ht.max(lead_eq(f, seg, self.h)).min(self.h / 2);
        let cur = &self.segs[self.cur];
        let sk = skip(cur);
        for tolerant in [false, true] {
            if let Some(o) = self.best_offset(cur, f, sk, self.ptr - 2 * h, self.ptr + 2 * h, self.ptr, tolerant) {
                return Some((self.cur, o));
            }
        }
        // anywhere else only exact rows count: a loose match would confuse pages that share a layout
        for (i, seg) in self.segs.iter().enumerate() {
            let hint = if i == self.cur { self.ptr } else { 0 };
            if let Some(o) = self.best_offset(seg, f, skip(seg), i64::MIN / 2, i64::MAX / 2, hint, false) {
                return Some((i, o));
            }
        }
        None
    }

    /// Feeds one sampled frame. `stable_for`: how many consecutive samples were identical to this
    /// one; `force`: a long motion without a pause, append anyway.
    pub fn push(&mut self, f: Frame, stable_for: usize, force: bool) -> Ev {
        if self.limit_hit {
            return Ev::Limit;
        }
        if f.w != self.w || f.h != self.h {
            return Ev::Ignored;
        }
        let fr = row_info(&f, self.cw);
        let informative = fr.flat.iter().filter(|x| !**x).count();
        if informative < self.h / 20 {
            return Ev::Ignored;
        }
        let h = self.h;
        match self.locate(&fr) {
            Some((si, o)) => {
                let moved = si != self.cur || o != self.ptr;
                // fixed rows, measured against the previous frame of the same page
                let (mut ht, mut fb) = (self.ht, self.fb);
                if moved && si == self.cur {
                    if let Some(p) = &self.prev {
                        let (a, b) = fixed_rows(p, &fr, h);
                        if a > 0 {
                            ht = a;
                        }
                        if b > 0 {
                            fb = b;
                        }
                    }
                }
                let seg_n = self.segs[si].n as i64;
                let body_end = (h - fb) as i64;
                let appends = o + body_end > seg_n;
                if appends && stable_for < APPEND_STABLE && !force {
                    return Ev::Wait;
                }
                if fb > 0 && self.fb == 0 && self.frames == 1 && self.segs.len() == 1 && self.segs[0].n == h {
                    // the first frame still holds the fixed footer as if it were content
                    self.segs[0].truncate(h - fb, self.w);
                }
                self.ht = ht;
                self.fb = fb;
                self.cur = si;
                self.ptr = o;
                if fb > 0 {
                    self.footer = f.bgra[(h - fb) * self.w * 4..].to_vec();
                }
                self.frames += 1;
                let mut result = if moved { Ev::Moved } else { Ev::Same };
                if appends {
                    let seg_n = self.segs[si].n;
                    let start = ((seg_n as i64 - o).max(ht as i64)) as usize;
                    let end = h - fb;
                    if start < end {
                        let room = self.max_rows.saturating_sub(self.rows());
                        let take = (end - start).min(room);
                        if take > 0 {
                            self.segs[si].append(&f, &fr, start, start + take);
                        }
                        if take < end - start {
                            self.limit_hit = true;
                            result = Ev::Limit;
                        } else {
                            result = Ev::Appended(take);
                        }
                    }
                }
                self.prev = Some(fr);
                result
            }
            None => {
                if stable_for < NEW_PAGE_STABLE {
                    return Ev::Wait;
                }
                if self.segs.len() >= MAX_SEGMENTS {
                    self.limit_hit = true;
                    return Ev::Limit;
                }
                // a new page: drop the top rows it shares with an earlier page (site header, search box)
                let tc = lead_eq(&fr, &self.segs[0], h).max(lead_eq(&fr, &self.segs[self.cur], h));
                let end = h - self.fb;
                if end < tc + 16 {
                    return Ev::Ignored;
                }
                let room = self.max_rows.saturating_sub(self.rows());
                let take = (end - tc).min(room);
                if take < 16 {
                    self.limit_hit = true;
                    return Ev::Limit;
                }
                self.segs.push(Seg::from_frame(&f, &fr, tc, tc + take));
                self.cur = self.segs.len() - 1;
                self.ptr = -(tc as i64);
                if self.fb > 0 {
                    self.footer = f.bgra[(h - self.fb) * self.w * 4..].to_vec();
                }
                self.prev = Some(fr);
                self.frames += 1;
                if take < end - tc {
                    self.limit_hit = true;
                    return Ev::Limit;
                }
                Ev::NewPage(take)
            }
        }
    }

    /// The stitched image: all pages in order, then the fixed footer once.
    pub fn finish(self) -> Stitched {
        let mut bgra = Vec::with_capacity(self.rows() * self.w * 4);
        let mut rows = 0;
        for s in &self.segs {
            bgra.extend_from_slice(&s.bgra);
            rows += s.n;
        }
        if self.fb > 0 && self.footer.len() == self.fb * self.w * 4 {
            bgra.extend_from_slice(&self.footer);
            rows += self.fb;
        }
        Stitched { w: self.w, h: rows, bgra }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: usize = 120;

    fn noise(a: u64, b: u64) -> u32 {
        let mut x = a.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ b.wrapping_mul(0xC2B2_AE3D_27D4_EB4F) ^ 0x1234_5678;
        x ^= x >> 29;
        x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
        x ^= x >> 32;
        (x as u32) | 0xFF00_0000
    }

    fn noise_row(a: u64) -> Vec<u8> {
        let mut r = Vec::with_capacity(W * 4);
        for x in 0..W {
            r.extend_from_slice(&noise(a, x as u64).to_le_bytes());
        }
        r
    }

    /// A page: `chrome` rows shared by every page at the top, then unique rows, optional sticky chrome.
    struct Page {
        seed: u64,
        chrome: usize,
        total: usize,
        h: usize,
        ht: usize,
        fb: usize,
    }

    impl Page {
        fn plain(seed: u64, total: usize, h: usize) -> Page {
            Page { seed, chrome: 0, total, h, ht: 0, fb: 0 }
        }

        fn row(&self, y: usize) -> Vec<u8> {
            if y < self.chrome {
                noise_row(7_000_000 + y as u64)
            } else {
                noise_row(self.seed * 1_000_003 + y as u64)
            }
        }

        fn frame(&self, y0: usize) -> Frame {
            let mut bgra = Vec::new();
            for r in 0..self.h {
                if r < self.ht {
                    bgra.extend(noise_row(9_100_000 + r as u64));
                } else if r >= self.h - self.fb {
                    bgra.extend(noise_row(9_200_000 + (r - (self.h - self.fb)) as u64));
                } else {
                    bgra.extend(self.row(y0 + r));
                }
            }
            Frame { w: W, h: self.h, bgra }
        }

        fn max_y(&self) -> usize {
            self.total - self.h
        }

        fn rows(&self, a: usize, b: usize) -> Vec<u8> {
            (a..b).flat_map(|y| self.row(y)).collect()
        }
    }

    fn feed(m: &mut Manual, p: &Page, ys: &[usize]) {
        for &y in ys {
            let r = m.push(p.frame(y.min(p.max_y())), 5, false);
            assert!(!matches!(r, Ev::Wait | Ev::Ignored | Ev::Limit), "y {y}: {r:?}");
        }
    }

    fn same(got: &Stitched, want: &[u8]) {
        assert_eq!(got.bgra.len(), want.len(), "rows {} vs {}", got.h, want.len() / (W * 4));
        for (i, (a, b)) in got.bgra.chunks(W * 4).zip(want.chunks(W * 4)).enumerate() {
            assert!(a == b, "row {i} differs");
        }
    }

    #[test]
    fn scrolling_down_in_uneven_steps() {
        let p = Page::plain(1, 1000, 200);
        let mut m = Manual::new(p.frame(0), 100_000);
        feed(&mut m, &p, &[37, 120, 150, 310, 330, 480, 600, 700, 790, 800]);
        same(&m.finish(), &p.rows(0, 1000));
    }

    #[test]
    fn scrolling_up_then_down_adds_nothing_twice() {
        let p = Page::plain(2, 1000, 200);
        let mut m = Manual::new(p.frame(0), 100_000);
        feed(&mut m, &p, &[100, 250, 400, 550]);
        let after = m.rows();
        feed(&mut m, &p, &[420, 300, 150, 0, 90, 200, 330, 450, 550]);
        assert_eq!(m.rows(), after);
        feed(&mut m, &p, &[640, 740, 800]);
        same(&m.finish(), &p.rows(0, 1000));
    }

    #[test]
    fn jitter_and_repeated_frames() {
        let p = Page::plain(3, 900, 200);
        let mut m = Manual::new(p.frame(0), 100_000);
        feed(&mut m, &p, &[100, 99, 101, 100, 200, 201, 199, 200, 300, 300, 300, 299, 400, 500, 600, 700, 699, 700]);
        let f = p.frame(700);
        assert!(matches!(m.push(f.clone(), 5, false), Ev::Same | Ev::Moved));
        assert!(matches!(m.push(f, 5, false), Ev::Same));
        same(&m.finish(), &p.rows(0, 900));
    }

    #[test]
    fn sticky_header_and_footer_are_kept_once() {
        let p = Page { seed: 4, chrome: 0, total: 1100, h: 220, ht: 30, fb: 24 };
        let mut m = Manual::new(p.frame(0), 100_000);
        feed(&mut m, &p, &[60, 140, 220, 300, 250, 340, 420, 500, 580, 660, 760, 880]);
        let mut want = Vec::new();
        for r in 0..30 {
            want.extend(noise_row(9_100_000 + r));
        }
        want.extend(p.rows(30, 1100 - 24));
        for r in 0..24 {
            want.extend(noise_row(9_200_000 + r));
        }
        same(&m.finish(), &want);
    }

    #[test]
    fn a_new_page_with_a_shared_header() {
        let p1 = Page { seed: 5, chrome: 60, total: 700, h: 200, ht: 0, fb: 0 };
        let p2 = Page { seed: 6, chrome: 60, total: 600, h: 200, ht: 0, fb: 0 };
        let mut m = Manual::new(p1.frame(0), 100_000);
        feed(&mut m, &p1, &[150, 300, 400, 500]);
        // click "2": the new page appears at its top after a blank and a half-drawn frame
        let blank = Frame { w: W, h: 200, bgra: vec![255; W * 200 * 4] };
        assert_eq!(m.push(blank, 5, false), Ev::Ignored);
        let mut half = p2.frame(0);
        for b in half.bgra[W * 100 * 4..].iter_mut() {
            *b = 255;
        }
        assert_eq!(m.push(half, 2, false), Ev::Wait);
        assert!(matches!(m.push(p2.frame(0), 5, false), Ev::NewPage(_)));
        feed(&mut m, &p2, &[100, 250, 400]);
        let mut want = p1.rows(0, 700);
        want.extend(p2.rows(60, 600));
        same(&m.finish(), &want);
    }

    #[test]
    fn going_back_to_an_earlier_page_adds_nothing() {
        let p1 = Page { seed: 7, chrome: 50, total: 700, h: 200, ht: 0, fb: 0 };
        let p2 = Page { seed: 8, chrome: 50, total: 650, h: 200, ht: 0, fb: 0 };
        let mut m = Manual::new(p1.frame(0), 100_000);
        feed(&mut m, &p1, &[120, 240, 360, 480, 500]);
        assert!(matches!(m.push(p2.frame(0), 5, false), Ev::NewPage(_)));
        feed(&mut m, &p2, &[150, 300, 450]);
        let rows = m.rows();
        // back to page 1, scroll about, then back to page 2 and about
        feed(&mut m, &p1, &[0, 100, 200, 300, 400, 500, 250]);
        assert_eq!(m.pages(), 2);
        feed(&mut m, &p2, &[0, 150, 300, 450, 300]);
        assert_eq!(m.rows(), rows);
        let mut want = p1.rows(0, 700);
        want.extend(p2.rows(50, 650));
        same(&m.finish(), &want);
    }

    #[test]
    fn an_unfinished_page_continues_where_it_was() {
        // page 1 was left before its end; coming back and scrolling on fills its own gap
        let p1 = Page { seed: 9, chrome: 40, total: 800, h: 200, ht: 0, fb: 0 };
        let p2 = Page { seed: 10, chrome: 40, total: 500, h: 200, ht: 0, fb: 0 };
        let mut m = Manual::new(p1.frame(0), 100_000);
        feed(&mut m, &p1, &[120, 240]);
        assert!(matches!(m.push(p2.frame(0), 5, false), Ev::NewPage(_)));
        feed(&mut m, &p2, &[150, 300]);
        feed(&mut m, &p1, &[0, 120, 240, 380, 500, 600]);
        let mut want = p1.rows(0, 800);
        want.extend(p2.rows(40, 500));
        same(&m.finish(), &want);
    }

    /// Rows of text-like pages: the same layout on every page, different words.
    fn texty(seed: u64, y: usize) -> Vec<u8> {
        let line = y / 60;
        let k = y % 60;
        let mut r = Vec::with_capacity(W * 4);
        for x in 0..W {
            let on = (10..14).contains(&k) && noise(seed * 977 + line as u64, (x / 5) as u64) % 3 == 0;
            let v = if on { 70u8 } else if k < 4 { 200 } else { 245 };
            r.extend_from_slice(&[v, v, v, 255]);
        }
        r
    }

    #[test]
    fn pages_that_share_a_layout_are_not_confused() {
        let page = |seed: u64, y0: usize| Frame { w: W, h: 200, bgra: (0..200).flat_map(|r| texty(seed, y0 + r)).collect() };
        let mut m = Manual::new(page(1, 0), 100_000);
        for y in (60..=600).step_by(60) {
            m.push(page(1, y), 5, false);
        }
        let rows = m.rows();
        // page 2 has the same look but other words
        for _ in 0..2 {
            let ev = m.push(page(2, 0), 5, false);
            assert!(matches!(ev, Ev::NewPage(_) | Ev::Same | Ev::Moved), "{ev:?}");
        }
        assert_eq!(m.pages(), 2);
        assert!(m.rows() > rows);
    }

    #[test]
    fn height_limit_cuts_the_result() {
        let p = Page::plain(11, 2000, 200);
        let mut m = Manual::new(p.frame(0), 450);
        let mut last = Ev::Same;
        for y in (100..1800).step_by(100) {
            last = m.push(p.frame(y), 5, false);
            if last == Ev::Limit {
                break;
            }
        }
        assert_eq!(last, Ev::Limit);
        assert_eq!(m.push(p.frame(1700), 5, false), Ev::Limit);
        let out = m.finish();
        assert_eq!(out.h, 450);
        assert!(out.bgra == p.rows(0, 450));
    }

    #[test]
    fn unstable_views_do_not_append() {
        let p = Page::plain(12, 800, 200);
        let mut m = Manual::new(p.frame(0), 100_000);
        assert_eq!(m.push(p.frame(150), 1, false), Ev::Wait);
        // a long motion without a pause is let through
        assert!(matches!(m.push(p.frame(150), 1, true), Ev::Appended(_)));
        assert_eq!(m.rows(), 350);
    }

    #[test]
    fn slightly_redrawn_rows_still_match() {
        let p = Page::plain(13, 700, 200);
        let mut m = Manual::new(p.frame(0), 100_000);
        let mut f = p.frame(90);
        for (i, v) in f.bgra.iter_mut().enumerate() {
            if i % 4 != 3 {
                *v = if (i / 4) % 3 == 0 { v.saturating_add(4) } else { v.saturating_sub(3) };
            }
        }
        let ev = m.push(f, 5, false);
        assert!(matches!(ev, Ev::Appended(90)), "{ev:?}");
    }
}
