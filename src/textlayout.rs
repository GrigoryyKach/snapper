//! Pure logic of "Copy text": turning word boxes into text (lines, columns, tables, indentation),
//! comparing the exact (UI Automation) text with the OCR text, picking the OCR language and
//! preparing the pixels for the recogniser. No Win32 here, so everything is unit-tested.

/// A recognised word (or phrase) with its box in region pixels.
#[derive(Clone, Debug, PartialEq)]
pub struct Word {
    pub text: String,
    pub l: f32,
    pub t: f32,
    pub r: f32,
    pub b: f32,
}

impl Word {
    #[cfg(test)]
    pub fn new(text: &str, l: f32, t: f32, r: f32, b: f32) -> Word {
        Word { text: text.to_string(), l, t, r, b }
    }
    fn w(&self) -> f32 {
        self.r - self.l
    }
    fn h(&self) -> f32 {
        self.b - self.t
    }
    fn cy(&self) -> f32 {
        (self.t + self.b) / 2.0
    }
}

fn median(v: &mut Vec<f32>) -> f32 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    v[v.len() / 2]
}

// ───────────────────────── script helpers ─────────────────────────

pub fn is_cyrillic(c: char) -> bool {
    matches!(c, '\u{0400}'..='\u{04FF}' | '\u{0500}'..='\u{052F}')
}

pub fn is_latin(c: char) -> bool {
    c.is_ascii_alphabetic() || matches!(c, '\u{00C0}'..='\u{024F}')
}

/// Latin letters that look like a Cyrillic one, and back.
const HOMOGLYPHS: [(char, char); 26] = [
    ('a', 'а'),
    ('c', 'с'),
    ('e', 'е'),
    ('o', 'о'),
    ('p', 'р'),
    ('x', 'х'),
    ('y', 'у'),
    ('i', 'і'),
    ('A', 'А'),
    ('B', 'В'),
    ('C', 'С'),
    ('E', 'Е'),
    ('H', 'Н'),
    ('K', 'К'),
    ('M', 'М'),
    ('O', 'О'),
    ('P', 'Р'),
    ('T', 'Т'),
    ('X', 'Х'),
    ('I', 'І'),
    ('Y', 'У'),
    ('S', 'Ѕ'),
    ('J', 'Ј'),
    ('N', 'Н'),
    ('r', 'г'),
    ('u', 'и'),
];

/// An OCR word that mixes Cyrillic and Latin letters is almost always one script with a few
/// look-alike letters wrong ("Привeт"): pull the minority letters over to the majority script.
/// A mixed word is left alone when a letter has no look-alike.
pub fn fix_homoglyphs(word: &str) -> String {
    let cyr = word.chars().filter(|c| is_cyrillic(*c)).count();
    let lat = word.chars().filter(|c| is_latin(*c)).count();
    if cyr == 0 || lat == 0 {
        return word.to_string();
    }
    let to_cyr = cyr > lat;
    let map = |c: char| -> Option<char> {
        if to_cyr {
            HOMOGLYPHS.iter().find(|(l, _)| *l == c && !matches!(c, 'r' | 'u' | 'N' | 'S' | 'J' | 'Y')).map(|(_, y)| *y)
        } else {
            HOMOGLYPHS.iter().find(|(_, y)| *y == c).map(|(l, _)| *l)
        }
    };
    let minority = |c: char| if to_cyr { is_latin(c) } else { is_cyrillic(c) };
    let fixed: Option<String> = word.chars().map(|c| if minority(c) { map(c) } else { Some(c) }).collect();
    match fixed {
        Some(f) if cyr != lat => f,
        _ => word.to_string(),
    }
}

/// Cyrillic letter and the Latin letter it can be mistaken for.
const TO_LATIN: [(char, char); 25] = [
    ('а', 'a'),
    ('с', 'c'),
    ('е', 'e'),
    ('о', 'o'),
    ('р', 'p'),
    ('х', 'x'),
    ('у', 'y'),
    ('і', 'i'),
    ('к', 'k'),
    ('п', 'n'),
    ('г', 'r'),
    ('А', 'A'),
    ('В', 'B'),
    ('Е', 'E'),
    ('К', 'K'),
    ('М', 'M'),
    ('Н', 'H'),
    ('О', 'O'),
    ('Р', 'P'),
    ('С', 'C'),
    ('Т', 'T'),
    ('Х', 'X'),
    ('У', 'Y'),
    ('І', 'I'),
    ('м', 'm'),
];

fn all_lookalike(word: &str) -> bool {
    word.chars().count() >= 2 && word.chars().all(|c| TO_LATIN.iter().any(|(y, _)| *y == c))
}

/// A recogniser with Cyrillic letters reads Latin words as look-alike Cyrillic ones (`can` as
/// `сап`, `OCR` as `0CR`). Inside a line that is Latin (at least four letters in Latin words and no
/// Cyrillic word that has a letter without a Latin twin) a word made only of look-alike Cyrillic
/// letters is turned back into Latin, and a zero between capitals into `O`. Lines with real
/// Cyrillic words stay as they are.
pub fn fix_script_context(words: &[Word]) -> Vec<Word> {
    let mut out: Vec<Word> = Vec::with_capacity(words.len());
    for line in cluster_lines(words) {
        let (mut lat, mut cyr) = (0usize, 0usize);
        for w in &line.words {
            let n = w.text.chars().filter(|c| c.is_alphabetic()).count();
            if w.text.chars().all(|c| !c.is_alphabetic() || is_latin(c)) {
                lat += n;
            } else if w.text.chars().all(|c| !c.is_alphabetic() || is_cyrillic(c)) && !all_lookalike(&w.text) {
                cyr += n;
            }
        }
        let latin_line = lat >= 4 && cyr == 0;
        for w in line.words {
            let mut text = w.text.clone();
            if latin_line && all_lookalike(&text) {
                text = text.chars().map(|c| TO_LATIN.iter().find(|(y, _)| *y == c).map_or(c, |(_, l)| *l)).collect();
            }
            if text.chars().count() >= 3 && text.contains('0') && text.chars().filter(|c| c.is_ascii_uppercase()).count() >= 2 && text.chars().all(|c| c == '0' || c.is_ascii_uppercase()) {
                text = text.replace('0', "O");
            }
            out.push(Word { text, ..w });
        }
    }
    out
}

// ───────────────────────── layout ─────────────────────────

struct Ctx {
    /// Median word height.
    mh: f32,
    /// Median character width.
    cw: f32,
    /// Monospace-looking text (code): runs of spaces are kept.
    mono: bool,
}

fn context(words: &[Word]) -> Ctx {
    let mut hs: Vec<f32> = words.iter().map(|w| w.h()).collect();
    let mh = median(&mut hs).max(1.0);
    let mut cws: Vec<f32> = words.iter().filter(|w| w.text.chars().count() >= 3).map(|w| w.w() / w.text.chars().count() as f32).collect();
    let cw = if cws.is_empty() { mh * 0.5 } else { median(&mut cws) }.max(1.0);
    let mut mono = false;
    if cws.len() >= 4 {
        let mean = cws.iter().sum::<f32>() / cws.len() as f32;
        let var = cws.iter().map(|c| (c - mean) * (c - mean)).sum::<f32>() / cws.len() as f32;
        mono = var.sqrt() / mean <= 0.12;
    }
    Ctx { mh, cw, mono }
}

struct Line {
    words: Vec<Word>,
    t: f32,
    b: f32,
}

impl Line {
    fn cy(&self) -> f32 {
        (self.t + self.b) / 2.0
    }
    fn l(&self) -> f32 {
        self.words.first().map_or(0.0, |w| w.l)
    }
}

/// Groups words into lines by vertical overlap, top to bottom; words left to right.
fn cluster_lines(words: &[Word]) -> Vec<Line> {
    let mut order: Vec<&Word> = words.iter().collect();
    order.sort_by(|a, b| a.cy().partial_cmp(&b.cy()).unwrap_or(std::cmp::Ordering::Equal));
    let mut lines: Vec<Line> = Vec::new();
    for w in order {
        let mut best: Option<usize> = None;
        for (i, ln) in lines.iter().enumerate().rev().take(6) {
            let ov = w.b.min(ln.b) - w.t.max(ln.t);
            if ov >= 0.5 * w.h().min(ln.b - ln.t) {
                best = Some(i);
                break;
            }
        }
        match best {
            Some(i) => {
                let ln = &mut lines[i];
                ln.t = ln.t.min(w.t);
                ln.b = ln.b.max(w.b);
                ln.words.push(w.clone());
            }
            None => lines.push(Line { words: vec![w.clone()], t: w.t, b: w.b }),
        }
    }
    for ln in &mut lines {
        ln.words.sort_by(|a, b| a.l.partial_cmp(&b.l).unwrap_or(std::cmp::Ordering::Equal));
    }
    lines.sort_by(|a, b| a.cy().partial_cmp(&b.cy()).unwrap_or(std::cmp::Ordering::Equal));
    lines
}

/// Full-height vertical gutters split the words into columns (left to right).
fn columns(words: &[Word], gutter: f32) -> Vec<(f32, f32)> {
    let mut iv: Vec<(f32, f32)> = words.iter().map(|w| (w.l, w.r)).collect();
    iv.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let mut out: Vec<(f32, f32)> = Vec::new();
    for (l, r) in iv {
        match out.last_mut() {
            Some(last) if l - last.1 < gutter => last.1 = last.1.max(r),
            _ => out.push((l, r)),
        }
    }
    out
}

fn push_spaces(s: &mut String, n: usize) {
    for _ in 0..n {
        s.push(' ');
    }
}

/// Words of one line joined with spaces; in monospace text the gaps keep their width.
fn join_words(words: &[Word], ctx: &Ctx) -> String {
    let mut s = String::new();
    let mut prev: Option<&Word> = None;
    for w in words {
        if let Some(p) = prev {
            let gap = w.l - p.r;
            let n = if ctx.mono && gap / ctx.cw >= 1.6 { (gap / ctx.cw).round() as usize } else { 1 };
            push_spaces(&mut s, n.max(1));
        }
        s.push_str(&w.text);
        prev = Some(w);
    }
    s
}

/// Text of one column or block: lines, indentation, blank lines between paragraphs and tab
/// separated cells where several lines share the same gaps.
fn layout_block(words: &[Word], ctx: &Ctx) -> String {
    let lines = cluster_lines(words);
    if lines.is_empty() {
        return String::new();
    }
    // Cell anchors: x positions where a segment starts after a wide gap, seen on at least two lines.
    let seg_gap = ctx.mh * 1.0;
    let mut starts: Vec<f32> = Vec::new();
    let mut line_starts: Vec<Vec<f32>> = Vec::new();
    for ln in &lines {
        let mut v = Vec::new();
        for pair in ln.words.windows(2) {
            if pair[1].l - pair[0].r > seg_gap {
                v.push(pair[1].l);
            }
        }
        starts.extend(v.iter().copied());
        line_starts.push(v);
    }
    let tol = ctx.mh * 0.6;
    starts.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mut anchors: Vec<(f32, usize)> = Vec::new();
    for x in starts {
        match anchors.last_mut() {
            Some(a) if x - a.0 <= tol => a.1 += 1,
            _ => anchors.push((x, 1)),
        }
    }
    let anchors: Vec<f32> = anchors.into_iter().filter(|a| a.1 >= 2).map(|a| a.0).collect();
    let anchor_of = |x: f32| anchors.iter().position(|a| (a - x).abs() <= tol);

    let min_l = lines.iter().map(|l| l.l()).fold(f32::MAX, f32::min);
    let mut pitches: Vec<f32> = lines.windows(2).map(|p| p[1].cy() - p[0].cy()).filter(|d| *d > 0.0).collect();
    pitches.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let pitch = if pitches.is_empty() { ctx.mh * 1.3 } else { pitches[pitches.len() / 4] }.max(1.0);
    let mut out = String::new();
    for (i, ln) in lines.iter().enumerate() {
        if i > 0 {
            out.push('\n');
            let prev = &lines[i - 1];
            if ln.t - prev.b > ctx.mh * 0.9 && ln.cy() - prev.cy() > pitch * 1.35 {
                out.push('\n');
            }
        }
        let dx = ln.l() - min_l;
        let indent = if ctx.mono {
            (dx / ctx.cw).round() as usize
        } else if dx >= ctx.cw * 1.2 {
            (dx / (ctx.cw * 0.55)).round() as usize
        } else {
            0
        };
        push_spaces(&mut out, indent);
        // split the line at the anchored gaps
        let mut cells: Vec<(usize, Vec<Word>)> = vec![(0, vec![ln.words[0].clone()])];
        for pair in ln.words.windows(2) {
            let gap = pair[1].l - pair[0].r;
            let col = if gap > seg_gap { anchor_of(pair[1].l) } else { None };
            match col {
                Some(a) if cells.last().is_some_and(|c| c.0 < a + 1) => cells.push((a + 1, vec![pair[1].clone()])),
                _ => cells.last_mut().unwrap().1.push(pair[1].clone()),
            }
        }
        let mut prev_col = 0;
        for (k, (col, ws)) in cells.iter().enumerate() {
            if k > 0 {
                for _ in 0..(col - prev_col).max(1) {
                    out.push('\t');
                }
            }
            out.push_str(&join_words(ws, ctx));
            prev_col = *col;
        }
    }
    out
}

/// Words per line, per column, to tell prose columns from table columns.
fn avg_words_per_line(words: &[Word]) -> f32 {
    let lines = cluster_lines(words);
    if lines.is_empty() {
        return 0.0;
    }
    words.len() as f32 / lines.len() as f32
}

fn is_table(cols: &[Vec<Word>]) -> bool {
    let lines: Vec<Vec<f32>> = cols.iter().map(|c| cluster_lines(c).iter().map(|l| l.cy()).collect()).collect();
    if cols.iter().all(|c| avg_words_per_line(c) >= 5.0) && lines.iter().any(|l| l.len() >= 3) {
        return false; // newspaper columns
    }
    let mut hs: Vec<f32> = cols.iter().flatten().map(|w| w.h()).collect();
    let tol = median(&mut hs).max(1.0) * 0.6;
    for pair in lines.windows(2) {
        let (small, large) = if pair[0].len() <= pair[1].len() { (&pair[0], &pair[1]) } else { (&pair[1], &pair[0]) };
        let matched = small.iter().filter(|y| large.iter().any(|z| (*y - z).abs() < tol)).count();
        if (matched as f32) < 0.75 * small.len() as f32 || large.len() > 2 * small.len().max(1) + 1 {
            return false;
        }
    }
    true
}

/// Reading-order text of the word boxes.
///
/// Full-height gutters (wider than 1.5 word heights) split the words into columns. Columns whose
/// lines line up and are short are a table (cells joined with tabs); wide prose columns are read one
/// after the other. Inside a column the lines come from vertical overlap, a gap above 0.9 word
/// heights is a paragraph break, and the leftmost line sets the zero of the indentation.
pub fn layout(words: &[Word]) -> String {
    let words: Vec<Word> = words.iter().filter(|w| !w.text.trim().is_empty() && w.w() > 0.0 && w.h() > 0.0).cloned().collect();
    if words.is_empty() {
        return String::new();
    }
    let ctx = context(&words);
    let cols = columns(&words, ctx.mh * 1.5);
    if cols.len() <= 1 {
        return layout_block(&words, &ctx);
    }
    let groups: Vec<Vec<Word>> = cols
        .iter()
        .map(|(l, r)| words.iter().filter(|w| w.l >= *l - 0.5 && w.r <= *r + 0.5).cloned().collect::<Vec<_>>())
        .filter(|g: &Vec<Word>| !g.is_empty())
        .collect();
    if groups.len() >= 2 && is_table(&groups) {
        let rows = cluster_lines(&words);
        let mut out = String::new();
        for (i, row) in rows.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            let mut cells: Vec<Vec<Word>> = vec![Vec::new(); cols.len()];
            for w in &row.words {
                let c = cols.iter().position(|(l, r)| w.l >= *l - 0.5 && w.r <= *r + 0.5).unwrap_or(0);
                cells[c].push(w.clone());
            }
            let texts: Vec<String> = cells.iter().map(|c| join_words(c, &ctx)).collect();
            out.push_str(texts.join("\t").trim_end_matches('\t'));
        }
        return out;
    }
    groups.iter().map(|g| layout_block(g, &ctx)).collect::<Vec<_>>().join("\n\n")
}

// ───────────────────────── comparing exact text and OCR ─────────────────────────

/// Lower-case words of at least two letters or digits (`ё` folded to `е`).
pub fn tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let flush = |cur: &mut String, out: &mut Vec<String>| {
        if cur.chars().count() >= 2 {
            out.push(std::mem::take(cur));
        } else {
            cur.clear();
        }
    };
    for c in text.chars() {
        if c.is_alphanumeric() {
            for l in c.to_lowercase() {
                cur.push(if l == 'ё' { 'е' } else { l });
            }
        } else {
            flush(&mut cur, &mut out);
        }
    }
    flush(&mut cur, &mut out);
    out
}

pub fn levenshtein(a: &[char], b: &[char]) -> usize {
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    /// The exact text from UI Automation.
    Exact,
    Ocr,
}

/// Letters and digits only, lower case, `ё` folded to `е`: for comparing text that OCR may have
/// split or joined.
fn squash(text: &str) -> String {
    text.chars().filter(|c| c.is_alphanumeric()).flat_map(|c| c.to_lowercase()).map(|c| if c == 'ё' { 'е' } else { c }).collect()
}

/// `a` is a damaged copy of `b`: at most 40 percent of the letters differ.
fn loosely_near(a: &str, b: &str) -> bool {
    let (ca, cb): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let n = ca.len().max(cb.len());
    if ca.len().abs_diff(cb.len()) * 10 > n * 4 {
        return false;
    }
    levenshtein(&ca, &cb) * 10 <= (n * 4).max(10)
}

#[derive(Clone, Copy, Debug)]
pub struct Stats {
    /// Share of the substantial OCR words (4+ letters) that the exact text explains: found in it,
    /// or a damaged copy of one of its words. OCR slips letters, so a wrong word still counts;
    /// a word that is nowhere near the exact text is text the exact text does not have.
    pub explained: f32,
    /// Share of the substantial exact words that OCR found (loosely).
    pub seen: f32,
    /// Substantial OCR words / exact words.
    pub n_ocr: usize,
    pub n_uia: usize,
}

pub fn stats(uia: &str, ocr: &str) -> Stats {
    let long = |t: &str| -> Vec<String> { tokens(t).into_iter().filter(|w| w.chars().count() >= 4).collect() };
    let (u, o) = (long(uia), long(ocr));
    let squashed = squash(uia);
    let u_set: std::collections::HashSet<&str> = u.iter().map(|s| s.as_str()).collect();
    let mut u_uniq: Vec<&str> = u_set.iter().copied().collect();
    u_uniq.sort_unstable();
    let explained = o.iter().filter(|w| u_set.contains(w.as_str()) || squashed.contains(&squash(w)) || u_uniq.iter().any(|x| loosely_near(w, x))).count();
    let o_set: std::collections::HashSet<&str> = o.iter().map(|s| s.as_str()).collect();
    let o_uniq: Vec<&str> = o_set.iter().copied().collect();
    let o_squashed = squash(ocr);
    let seen = u.iter().filter(|w| o_set.contains(w.as_str()) || o_squashed.contains(&squash(w)) || o_uniq.iter().any(|x| loosely_near(w, x))).count();
    Stats { explained: explained as f32 / o.len().max(1) as f32, seen: seen as f32 / u.len().max(1) as f32, n_ocr: o.len(), n_uia: u.len() }
}

/// Picks the text to copy.
///
/// The exact text wins when it explains the OCR text: at least 80 percent of the substantial OCR
/// words (90 percent when the exact text is incomplete) are in it or damaged copies of its words,
/// so OCR's own slips do not count against it. If OCR sees real words that the exact text lacks
/// (an image of text beside real text, a canvas) the exact text is only part of what is on screen,
/// and the OCR text is used. The exact text also loses when it holds many words OCR finds hardly
/// any of (text from outside the region). With no exact text, OCR; with no OCR result the exact
/// text stands alone.
pub fn choose(uia: &str, ocr: &str, uia_complete: bool) -> Option<Source> {
    let (u, o) = (tokens(uia), tokens(ocr));
    match (u.is_empty(), o.is_empty()) {
        (true, true) => None,
        (true, false) => Some(Source::Ocr),
        (false, true) => Some(Source::Exact),
        (false, false) => {
            let st = stats(uia, ocr);
            let need = if uia_complete { 0.8 } else { 0.9 };
            let outside = st.n_uia >= 20 && st.seen < 0.15;
            if (st.n_ocr < 3 || st.explained >= need) && !outside {
                Some(Source::Exact)
            } else {
                Some(Source::Ocr)
            }
        }
    }
}

// ───────────────────────── OCR language ─────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub struct EngineText {
    /// BCP-47 tag, e.g. `ru-RU`.
    pub tag: String,
    pub text: String,
}

fn base(tag: &str) -> &str {
    tag.split(['-', '_']).next().unwrap_or(tag)
}

/// Letters that sit in whole Cyrillic words (3+ letters, no Latin letter inside) as a share of all
/// letters.
pub fn cyrillic_share(t: &str) -> f32 {
    let (mut cyr, mut all) = (0usize, 0usize);
    for w in t.split(|c: char| !c.is_alphabetic()) {
        let n = w.chars().count();
        all += n;
        if n >= 3 && w.chars().all(is_cyrillic) {
            cyr += n;
        }
    }
    if all == 0 {
        0.0
    } else {
        cyr as f32 / all as f32
    }
}

/// How much of a Latin engine's text looks like what a Latin engine makes of Cyrillic letters:
/// words with a capital inside (`npnBeT`), letters mixed with digits (`dcg3`) or with symbols
/// (`m|p`). Real Latin text has almost none of these.
pub fn latin_garbage(t: &str) -> f32 {
    let mut total = 0usize;
    let mut bad = 0usize;
    for w in t.split_whitespace() {
        let w = w.trim_matches(|c: char| !c.is_alphanumeric());
        if w.chars().count() < 2 {
            continue;
        }
        total += 1;
        let letters = w.chars().filter(|c| c.is_alphabetic()).count();
        let digits = w.chars().filter(|c| c.is_ascii_digit()).count();
        let inner_symbol = w.chars().any(|c| !c.is_alphanumeric() && !matches!(c, '-' | '\'' | '.' | '_' | '\u{2019}'));
        let rest: Vec<char> = w.chars().skip(1).collect();
        let inner_capital = rest.iter().any(|c| c.is_uppercase()) && rest.iter().any(|c| c.is_lowercase());
        if (letters >= 3 && digits > 0 && letters > digits) || inner_symbol || (inner_capital && letters >= 4) {
            bad += 1;
        }
    }
    if total == 0 {
        0.0
    } else {
        bad as f32 / total as f32
    }
}

/// Share from which a text counts as Cyrillic.
pub const CYRILLIC_TEXT: f32 = 0.25;
/// Garbage share of the Latin engine's text from which the text is not Latin.
pub const LATIN_GARBAGE: f32 = 0.12;

pub fn is_cyrillic_lang(tag: &str) -> bool {
    matches!(base(tag), "ru" | "uk" | "bg" | "sr" | "be" | "mk" | "kk")
}

/// Index of the best engine result.
///
/// The Windows engine reports no confidence, so the choice is made from the script. The text is
/// Cyrillic when a Cyrillic engine found whole Cyrillic words in at least a quarter of the letters
/// and the Latin engine's text of the same pixels is garbage (capitals inside words, letters mixed
/// with digits or symbols: 12 percent of its words or more). Then only the Cyrillic engines
/// compete (a Latin engine turns Cyrillic into look-alike garbage): the engine whose own letters
/// show up more wins (`ы э ъ ё` for Russian, `і ї є ґ` for Ukrainian), then the order of
/// `results`, which follows the user's language list. Otherwise the Latin engines compete on the
/// number of plausible words (letters only, with a vowel), then by order, and a Cyrillic engine's
/// stray look-alike letters (`п` for `n`) never decide.
pub fn pick_engine(results: &[EngineText]) -> Option<usize> {
    if results.is_empty() {
        return None;
    }
    let share = cyrillic_share;
    let latin_garbage_min = results.iter().filter(|r| !is_cyrillic_lang(&r.tag)).map(|r| latin_garbage(&r.text)).fold(None, |m: Option<f32>, g| Some(m.map_or(g, |m| m.min(g))));
    let cyr_text = results.iter().any(|r| is_cyrillic_lang(&r.tag) && share(&r.text) >= CYRILLIC_TEXT) && latin_garbage_min.map_or(true, |g| g >= LATIN_GARBAGE);
    if cyr_text {
        let unique = |r: &EngineText| -> usize {
            let set: &str = match base(&r.tag) {
                "ru" => "ыэъёЫЭЪЁ",
                "uk" => "іїєґІЇЄҐ",
                _ => "",
            };
            r.text.chars().filter(|c| set.contains(*c)).count()
        };
        let cyr_letters = |r: &EngineText| r.text.chars().filter(|c| is_cyrillic(*c)).count();
        return results
            .iter()
            .enumerate()
            .filter(|(_, r)| is_cyrillic_lang(&r.tag))
            .max_by(|(ia, a), (ib, b)| {
                unique(a).cmp(&unique(b)).then(cyr_letters(a).cmp(&cyr_letters(b))).then(ib.cmp(ia))
            })
            .map(|(i, _)| i);
    }
    let plausible = |t: &str| {
        t.split(|c: char| !c.is_alphabetic())
            .filter(|w| w.chars().count() >= 3 && w.chars().any(|c| "aeiouyAEIOUY".contains(c) || "àáâäèéêëìíîïòóôöùúûü".contains(c)))
            .count()
    };
    let latin: Vec<(usize, &EngineText)> = results.iter().enumerate().filter(|(_, r)| !is_cyrillic_lang(&r.tag)).collect();
    let pool: Vec<(usize, &EngineText)> = if latin.is_empty() { results.iter().enumerate().collect() } else { latin };
    pool.into_iter().max_by(|(ia, a), (ib, b)| plausible(&a.text).cmp(&plausible(&b.text)).then(ib.cmp(ia))).map(|(i, _)| i)
}

// ───────────────────────── pixels ─────────────────────────

/// 8-bit luminance.
#[derive(Clone)]
pub struct Gray {
    pub w: usize,
    pub h: usize,
    pub px: Vec<u8>,
}

pub fn to_gray(w: usize, h: usize, bgra: &[u8]) -> Gray {
    let px = bgra.chunks_exact(4).take(w * h).map(|p| ((299 * p[2] as u32 + 587 * p[1] as u32 + 114 * p[0] as u32) / 1000) as u8).collect();
    Gray { w, h, px }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Polarity {
    /// Dark text on a light background.
    Normal,
    /// Light text on a dark background: invert.
    Inverted,
    /// Both kinds of areas: normalise each against its own surroundings.
    Mixed,
}

/// The background is the biggest area: when it is dark the text is light on dark. When dark and
/// light areas each take at least 20 percent (a dark code block on a light page) the image is mixed.
pub fn polarity(g: &Gray) -> Polarity {
    if g.px.is_empty() {
        return Polarity::Normal;
    }
    let n = g.px.len() as f32;
    let dark = g.px.iter().filter(|p| **p < 110).count() as f32 / n;
    let light = g.px.iter().filter(|p| **p > 150).count() as f32 / n;
    if dark >= 0.2 && light >= 0.2 {
        return Polarity::Mixed;
    }
    let mut hist = [0u32; 32];
    for p in &g.px {
        hist[(*p >> 3) as usize] += 1;
    }
    let mode = hist.iter().enumerate().max_by_key(|(_, c)| **c).map(|(i, _)| i * 8 + 4).unwrap_or(255);
    if mode < 128 {
        Polarity::Inverted
    } else {
        Polarity::Normal
    }
}

/// Dark text on white whatever the original polarity, stretched to the full range.
pub fn normalise(g: &Gray, pol: Polarity) -> Gray {
    let mut px: Vec<u8> = match pol {
        Polarity::Normal => g.px.clone(),
        Polarity::Inverted => g.px.iter().map(|p| 255 - p).collect(),
        Polarity::Mixed => local_contrast(g),
    };
    let mut hist = [0u32; 256];
    for p in &px {
        hist[*p as usize] += 1;
    }
    let n = px.len() as u32;
    let (mut lo, mut hi, mut acc) = (0usize, 255usize, 0u32);
    for (i, c) in hist.iter().enumerate() {
        acc += c;
        if acc * 100 >= n {
            lo = i;
            break;
        }
    }
    acc = 0;
    for (i, c) in hist.iter().enumerate().rev() {
        acc += c;
        if acc * 100 >= n {
            hi = i;
            break;
        }
    }
    if hi > lo + 8 && hi - lo < 200 {
        let range = (hi - lo) as f32;
        for p in px.iter_mut() {
            *p = (((*p as f32 - lo as f32) / range).clamp(0.0, 1.0) * 255.0).round() as u8;
        }
    }
    Gray { w: g.w, h: g.h, px }
}

/// Distance from the local mean, drawn dark on white: text of either polarity comes out dark.
fn local_contrast(g: &Gray) -> Vec<u8> {
    let (w, h) = (g.w, g.h);
    let mut integral = vec![0u64; (w + 1) * (h + 1)];
    for y in 0..h {
        let mut row = 0u64;
        for x in 0..w {
            row += g.px[y * w + x] as u64;
            integral[(y + 1) * (w + 1) + x + 1] = integral[y * (w + 1) + x + 1] + row;
        }
    }
    let r = 12usize;
    let mut out = vec![255u8; w * h];
    for y in 0..h {
        let (y0, y1) = (y.saturating_sub(r), (y + r + 1).min(h));
        for x in 0..w {
            let (x0, x1) = (x.saturating_sub(r), (x + r + 1).min(w));
            let sum = integral[y1 * (w + 1) + x1] + integral[y0 * (w + 1) + x0] - integral[y0 * (w + 1) + x1] - integral[y1 * (w + 1) + x0];
            let mean = sum as f32 / ((x1 - x0) * (y1 - y0)) as f32;
            let d = (g.px[y * w + x] as f32 - mean).abs();
            out[y * w + x] = (255.0 - (d * 2.2).min(255.0)) as u8;
        }
    }
    out
}

/// Typical height of the ink of a text line, from the rows that hold dark pixels (input is dark on white).
pub fn line_ink_height(g: &Gray) -> Option<f32> {
    if g.w == 0 || g.h == 0 {
        return None;
    }
    let ink: Vec<bool> = (0..g.h).map(|y| g.px[y * g.w..(y + 1) * g.w].iter().filter(|p| **p < 140).count() >= 2).collect();
    let mut runs: Vec<f32> = Vec::new();
    let mut y = 0;
    while y < g.h {
        if ink[y] {
            let s = y;
            while y < g.h && ink[y] {
                y += 1;
            }
            runs.push((y - s) as f32);
        } else {
            y += 1;
        }
    }
    runs.retain(|r| *r >= 4.0);
    if runs.is_empty() {
        None
    } else {
        Some(median(&mut runs))
    }
}

/// Upscale factor from the ink height of a line: around 32 px lines read best, never above 3, and
/// the scaled image stays below `max_pixels` and `max_dim`.
pub fn pick_scale(ink: Option<f32>, w: usize, h: usize, max_dim: usize, max_pixels: usize) -> usize {
    let want = match ink {
        Some(i) => (32.0 / i).round().clamp(1.0, 3.0) as usize,
        None => 2,
    };
    let mut s = want;
    while s > 1 && (w * s > max_dim || h * s > max_dim || w * h * s * s > max_pixels) {
        s -= 1;
    }
    s
}

/// Splits `0..len` into pieces of at most `max` at the quietest position (`quiet[i]` is the
/// activity of row or column `i`) inside the last fifth of each piece.
pub fn split_points(len: usize, max: usize, quiet: &[f32]) -> Vec<(usize, usize)> {
    if len <= max || max == 0 {
        return vec![(0, len)];
    }
    let mut out = Vec::new();
    let mut start = 0;
    while len - start > max {
        let from = start + max - max / 5;
        let to = start + max;
        let cut = (from..to).min_by(|a, b| quiet.get(*a).unwrap_or(&0.0).partial_cmp(quiet.get(*b).unwrap_or(&0.0)).unwrap_or(std::cmp::Ordering::Equal)).unwrap_or(to);
        out.push((start, cut));
        start = cut;
    }
    out.push((start, len));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(t: &str, l: f32, top: f32, r: f32) -> Word {
        Word::new(t, l, top, r, top + 10.0)
    }

    #[test]
    fn words_become_reading_order_lines() {
        let words = vec![w("world", 40.0, 0.0, 70.0), w("Hello", 0.0, 1.0, 35.0), w("Second", 0.0, 14.0, 45.0), w("line", 50.0, 15.0, 70.0)];
        assert_eq!(layout(&words), "Hello world\nSecond line");
    }

    #[test]
    fn paragraph_gap_gives_a_blank_line() {
        let words = vec![w("One", 0.0, 0.0, 30.0), w("Two", 0.0, 12.0, 30.0), w("Three", 0.0, 40.0, 40.0)];
        assert_eq!(layout(&words), "One\nTwo\n\nThree");
    }

    #[test]
    fn monospace_indentation_is_kept() {
        // 6 px per character
        let words = vec![
            w("fn", 0.0, 0.0, 12.0),
            w("main()", 18.0, 0.0, 54.0),
            w("let", 12.0, 14.0, 30.0),
            w("value", 36.0, 14.0, 66.0),
            w("=", 72.0, 14.0, 78.0),
            w("compute();", 84.0, 14.0, 144.0),
            w("}", 0.0, 28.0, 6.0),
        ];
        assert_eq!(layout(&words), "fn main()
  let value = compute();
}");
    }

    #[test]
    fn table_cells_are_joined_with_tabs() {
        let words = vec![
            w("Name", 0.0, 0.0, 28.0),
            w("Qty", 80.0, 0.0, 100.0),
            w("Apple", 0.0, 14.0, 32.0),
            w("3", 80.0, 14.0, 86.0),
            w("Pear", 0.0, 28.0, 26.0),
            w("12", 80.0, 28.0, 92.0),
        ];
        assert_eq!(layout(&words), "Name\tQty\nApple\t3\nPear\t12");
    }

    #[test]
    fn prose_columns_are_read_one_after_the_other() {
        let mut words = Vec::new();
        for row in 0..4 {
            let y = row as f32 * 14.0;
            for (i, x) in [0.0f32, 22.0, 44.0, 66.0, 88.0].iter().enumerate() {
                words.push(w(&format!("a{row}{i}word"), *x, y, x + 20.0));
                words.push(w(&format!("b{row}{i}word"), 200.0 + x, y, 200.0 + x + 20.0));
            }
        }
        let t = layout(&words);
        let (left, right) = t.split_once("\n\n").expect("two columns");
        assert!(left.starts_with("a00word") && left.ends_with("a34word"));
        assert!(right.starts_with("b00word") && right.ends_with("b34word"));
    }

    #[test]
    fn homoglyphs_follow_the_majority_script() {
        assert_eq!(fix_homoglyphs("Пpивeт"), "Привет");
        assert_eq!(fix_homoglyphs("Hеllo"), "Hello");
        assert_eq!(fix_homoglyphs("plain"), "plain");
        assert_eq!(fix_homoglyphs("Привет"), "Привет");
    }

    #[test]
    fn look_alike_words_in_a_latin_line_go_back_to_latin() {
        let w = |t: &str, l: f32| Word::new(t, l, 0.0, l + 30.0, 10.0);
        let line = vec![w("Image", 0.0), w("only", 40.0), w("0CR", 80.0), w("сап", 120.0), w("read", 160.0)];
        let text: Vec<String> = fix_script_context(&line).into_iter().map(|w| w.text).collect();
        assert_eq!(text, ["Image", "only", "OCR", "can", "read"]);
        // a Russian line keeps its words, even the ones made of look-alikes
        let line = vec![w("Привет", 0.0), w("мир", 40.0), w("сам", 80.0), w("ок", 120.0)];
        let text: Vec<String> = fix_script_context(&line).into_iter().map(|w| w.text).collect();
        assert_eq!(text, ["Привет", "мир", "сам", "ок"]);
    }

    #[test]
    fn exact_text_wins_when_it_covers_the_ocr_text() {
        let uia = "The quick brown fox jumps over the lazy dog";
        let ocr = "The quick brovvn fox jumps over the Iazy dog";
        assert_eq!(choose(uia, ocr, true), Some(Source::Exact));
    }

    #[test]
    fn ocr_wins_when_exact_text_is_half_of_it() {
        let uia = "Heading of the page and a paragraph";
        let ocr = "Heading of the page and a paragraph text inside an image another sentence in the picture";
        assert_eq!(choose(uia, ocr, true), Some(Source::Ocr));
    }

    #[test]
    fn ocr_wins_when_exact_text_is_mostly_outside_the_region() {
        let ocr = "short visible words here";
        let uia = "short visible words here plus a very long list of other unrelated sentences about completely different subjects: weather forecast tomorrow morning, football results yesterday evening, parliament debates budget proposal, gardening advice spring flowers, recipes pancakes breakfast";
        assert_eq!(choose(uia, ocr, true), Some(Source::Ocr));
    }

    #[test]
    fn empty_sides_fall_back() {
        assert_eq!(choose("", "text here", true), Some(Source::Ocr));
        assert_eq!(choose("text here", "", true), Some(Source::Exact));
        assert_eq!(choose("", "", true), None);
    }

    #[test]
    fn cyrillic_text_needs_a_cyrillic_engine() {
        let r = vec![
            EngineText { tag: "en-US".into(), text: "npnBeT mnp".into() },
            EngineText { tag: "ru-RU".into(), text: "Привет мир".into() },
            EngineText { tag: "uk-UA".into(), text: "Привет мир".into() },
        ];
        assert_eq!(pick_engine(&r), Some(1));
    }

    #[test]
    fn ukrainian_letters_pick_the_ukrainian_engine() {
        let r = vec![
            EngineText { tag: "ru-RU".into(), text: "Привіт світе їжак".into() },
            EngineText { tag: "uk-UA".into(), text: "Привіт світе їжак".into() },
        ];
        assert_eq!(pick_engine(&r), Some(1));
        let r = vec![
            EngineText { tag: "uk-UA".into(), text: "Съешь ещё этих мягких".into() },
            EngineText { tag: "ru-RU".into(), text: "Съешь ещё этих мягких".into() },
        ];
        assert_eq!(pick_engine(&r), Some(1));
    }

    #[test]
    fn latin_engine_garbage_means_cyrillic_pixels() {
        assert!(latin_garbage("npnBeT mnp Cbeuib 3THx dcg3 m|p") > 0.5);
        assert!(latin_garbage("The quick brown fox, jumps over it. Don't stop; JavaScript is fine") < 0.12);
        // the Latin engine read clean words: a Cyrillic engine's look-alike words do not win
        let r = vec![
            EngineText { tag: "en-US".into(), text: "Name Qty Price Apple Pear Cherry".into() },
            EngineText { tag: "ru".into(), text: "Name Qty Реаг Аррlе Сhеrrу".into() },
        ];
        assert_eq!(pick_engine(&r), Some(0));
    }

    #[test]
    fn latin_text_prefers_the_latin_engine() {
        let r = vec![EngineText { tag: "ru-RU".into(), text: "Hello world".into() }, EngineText { tag: "en-US".into(), text: "Hello world".into() }];
        assert_eq!(pick_engine(&r), Some(1));
    }

    fn gray(w: usize, h: usize, f: impl Fn(usize, usize) -> u8) -> Gray {
        let mut px = Vec::new();
        for y in 0..h {
            for x in 0..w {
                px.push(f(x, y));
            }
        }
        Gray { w, h, px }
    }

    #[test]
    fn light_on_dark_is_detected_and_inverted() {
        let g = gray(40, 40, |x, y| if (x / 4 + y / 4) % 7 == 0 { 230 } else { 20 });
        assert_eq!(polarity(&g), Polarity::Inverted);
        let n = normalise(&g, Polarity::Inverted);
        assert!(n.px.iter().filter(|p| **p > 200).count() > n.px.len() / 2, "background becomes light");
        let g = gray(40, 40, |x, y| if (x / 4 + y / 4) % 7 == 0 { 20 } else { 235 });
        assert_eq!(polarity(&g), Polarity::Normal);
    }

    #[test]
    fn dark_and_light_areas_are_mixed() {
        let g = gray(40, 40, |x, _| if x < 20 { 20 } else { 235 });
        assert_eq!(polarity(&g), Polarity::Mixed);
        let n = normalise(&g, Polarity::Mixed);
        assert!(n.px.chunks(40).all(|row| row[..4].iter().chain(&row[36..]).all(|p| *p > 200)), "flat areas of both kinds come out white");
    }

    #[test]
    fn scale_follows_text_height() {
        assert_eq!(pick_scale(Some(9.0), 600, 400, 10000, 30_000_000), 3);
        assert_eq!(pick_scale(Some(16.0), 600, 400, 10000, 30_000_000), 2);
        assert_eq!(pick_scale(Some(30.0), 600, 400, 10000, 30_000_000), 1);
        assert_eq!(pick_scale(Some(9.0), 4000, 2000, 10000, 30_000_000), 1);
        assert_eq!(pick_scale(Some(9.0), 3000, 1000, 10000, 30_000_000), 3);
        assert_eq!(pick_scale(Some(9.0), 2000, 1000, 5000, 30_000_000), 2);
    }

    #[test]
    fn ink_height_from_rows() {
        let g = gray(60, 60, |_, y| if (10..20).contains(&y) || (30..40).contains(&y) { 0 } else { 255 });
        assert_eq!(line_ink_height(&g), Some(10.0));
    }

    #[test]
    fn tiles_cut_at_quiet_rows() {
        let mut quiet = vec![1.0f32; 250];
        quiet[90] = 0.0;
        let t = split_points(250, 100, &quiet);
        assert_eq!(t[0], (0, 90));
        assert!(t.iter().all(|(a, b)| b - a <= 100));
        assert_eq!(t.last().unwrap().1, 250);
        assert_eq!(split_points(80, 100, &quiet), vec![(0, 80)]);
    }

    #[test]
    fn levenshtein_basics() {
        let a: Vec<char> = "kitten".chars().collect();
        let b: Vec<char> = "sitting".chars().collect();
        assert_eq!(levenshtein(&a, &b), 3);
    }
}
