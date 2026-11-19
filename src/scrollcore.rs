//! The platform independent part of scrolling capture: the grab / scroll / settle / stitch loop.
//! A platform supplies three small closures: grab the area, send wheel-down notches to it, and
//! tell whether the user asked to stop.

use crate::files::Image;
use crate::stitch::{Frame, Push, Stitcher};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Safety net: no more frames than this.
pub const MAX_FRAMES: usize = 400;
/// Wanted scroll step as a share of the area height (frames then overlap by about half).
const STEP_SHARE: f32 = 0.5;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Note {
    /// Reached the end of the page (or the user stopped it).
    None,
    /// Cut at the height limit.
    Limit,
    /// Frames stopped matching (an animation, a video, a jump).
    NoMatch,
    /// The area never moved.
    Nothing,
    /// Grabbing the screen failed.
    Failed,
}

pub struct Outcome {
    pub image: Option<Image>,
    pub note: Note,
}

pub struct Shared {
    pub stop: AtomicBool,
    pub frames: AtomicUsize,
    pub rows: AtomicUsize,
    pub done: AtomicBool,
    pub outcome: Mutex<Option<Outcome>>,
}

impl Shared {
    pub fn new() -> Shared {
        Shared { stop: AtomicBool::new(false), frames: AtomicUsize::new(0), rows: AtomicUsize::new(0), done: AtomicBool::new(false), outcome: Mutex::new(None) }
    }

    pub fn finish(&self, out: Outcome) {
        *self.outcome.lock().unwrap() = Some(out);
        self.done.store(true, Ordering::Release);
    }
}

/// What the platform gives the loop.
pub struct Io<'a> {
    pub grab: &'a mut dyn FnMut() -> Option<Frame>,
    /// Sends this many wheel-down notches to the middle of the area.
    pub wheel: &'a mut dyn FnMut(i32),
    /// True when the user pressed Esc (the Stop button is `Shared::stop`).
    pub esc: &'a dyn Fn() -> bool,
}

fn stopped(sh: &Shared, io: &Io) -> bool {
    sh.stop.load(Ordering::Relaxed) || (io.esc)()
}

/// Waits until two consecutive grabs are identical (smooth scrolling is over), at most ~0.8 s.
fn settle(io: &mut Io, sh: &Shared) -> Option<Frame> {
    std::thread::sleep(Duration::from_millis(140));
    let mut prev = (io.grab)()?;
    let t0 = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(60));
        let cur = (io.grab)()?;
        if cur.bgra == prev.bgra || t0.elapsed() > Duration::from_millis(800) || stopped(sh, io) {
            return Some(cur);
        }
        prev = cur;
    }
}

pub fn run(max_rows: usize, sh: &Shared, io: &mut Io) -> Outcome {
    let Some(first) = (io.grab)() else { return Outcome { image: None, note: Note::Failed } };
    let h = first.h;
    let mut st = Stitcher::new(first, 100, max_rows);
    let mut notches = 1;
    let mut calibrated = false;
    let mut retried = false;
    let mut resend = true;
    let mut note = Note::None;
    sh.frames.store(1, Ordering::Relaxed);
    sh.rows.store(st.rows(), Ordering::Relaxed);
    loop {
        if stopped(sh, io) {
            break;
        }
        if st.frames() >= MAX_FRAMES {
            note = Note::Limit;
            break;
        }
        if resend {
            (io.wheel)(notches);
        }
        let Some(f) = settle(io, sh) else {
            note = Note::Failed;
            break;
        };
        if std::env::var_os("SNAPPER_DUMP").is_some() {
            let img = Image { w: f.w as u32, h: f.h as u32, bgra: f.bgra.clone() };
            if let Ok(d) = crate::files::encode_png(&img, true) {
                let _ = std::fs::write(crate::config::data_dir().join(format!("frame_{}.png", st.frames())), d);
            }
        }
        let pushed = st.push(f);
        crate::sys::log(&format!("scroll: {pushed:?}, notches {notches}, rows {}", st.rows()));
        match pushed {
            Push::Appended { dy, .. } => {
                retried = false;
                resend = true;
                if !calibrated {
                    // one notch told us how far it goes: aim for about half the area per step
                    calibrated = true;
                    notches = ((STEP_SHARE * h as f32 / dy.max(1) as f32).round() as i32).clamp(1, 20);
                }
                sh.frames.store(st.frames(), Ordering::Relaxed);
                sh.rows.store(st.rows(), Ordering::Relaxed);
            }
            Push::End => {
                if !retried {
                    // maybe the scroll had not started yet: look once more without scrolling
                    retried = true;
                    resend = false;
                    std::thread::sleep(Duration::from_millis(350));
                    continue;
                }
                if st.frames() == 1 {
                    note = Note::Nothing;
                }
                break;
            }
            Push::NoMatch => {
                note = Note::NoMatch;
                break;
            }
            Push::Limit => {
                note = Note::Limit;
                break;
            }
        }
    }
    let frames = st.frames();
    let out = st.finish();
    if frames == 1 && note == Note::Nothing {
        return Outcome { image: None, note };
    }
    Outcome { image: Some(Image { w: out.w as u32, h: out.h as u32, bgra: out.bgra }), note }
}
