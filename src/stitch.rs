//! Frame model and row signatures for scrolling capture (pure, platform independent).
//!
//! Every frame row is reduced to a 64-bit hash (pixels lightly quantised, the right-hand strip that
//! may hold a scrollbar is ignored) plus a few luma buckets for a tolerant comparison. The stitching
//! itself lives in `manual.rs`.

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

/// Luma buckets per row for the tolerant matcher.
pub const BUCKETS: usize = 48;

pub struct Rows {
    /// `BUCKETS` average luma values per row.
    pub sig: Vec<u8>,
    pub hash: Vec<u64>,
    /// True when every compared pixel of the row is (nearly) the same colour.
    pub flat: Vec<bool>,
}

/// Compared width: the right-hand strip with a possible scrollbar is left out.
pub fn compare_width(w: usize) -> usize {
    if w >= 64 {
        w - (w / 8).min(40)
    } else {
        w
    }
}

pub fn row_info(f: &Frame, cw: usize) -> Rows {
    row_info_of(&f.bgra, f.w, f.h, cw)
}

/// Signatures of `h` rows of `w`-pixel BGRA data.
pub fn row_info_of(bgra: &[u8], w: usize, h: usize, cw: usize) -> Rows {
    let mut hash = Vec::with_capacity(h);
    let mut flat = Vec::with_capacity(h);
    let mut sig = Vec::with_capacity(h * BUCKETS);
    let nb = BUCKETS.min(cw).max(1);
    for y in 0..h {
        let row = &bgra[y * w * 4..y * w * 4 + cw * 4];
        for b in 0..BUCKETS {
            let (x0, x1) = if b < nb { (b * cw / nb, (b + 1) * cw / nb) } else { (0, 0) };
            let mut sum = 0u32;
            for px in row[x0 * 4..x1 * 4].chunks_exact(4) {
                sum += (px[0] as u32 * 29 + px[1] as u32 * 150 + px[2] as u32 * 77) >> 8;
            }
            sig.push(if x1 > x0 { (sum / (x1 - x0) as u32) as u8 } else { 0 });
        }
        let mut h64: u64 = 0xcbf2_9ce4_8422_2325;
        let mut first = 0u32;
        let mut same = true;
        for (i, px) in row.chunks_exact(4).enumerate() {
            let v = u32::from_le_bytes([px[0], px[1], px[2], 0]) & 0x00FC_FCFC;
            if i == 0 {
                first = v;
            } else if v != first {
                same = false;
            }
            h64 = (h64 ^ v as u64).wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash.push(h64);
        flat.push(same);
    }
    Rows { sig, hash, flat }
}
