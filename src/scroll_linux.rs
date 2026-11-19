//! Scrolling capture on X11: the worker grabs the area with `GetImage`, scrolls it with XTest wheel
//! events (buttons 4 and 5) and stops on Esc (polled with `QueryKeymap`). Wayland does not allow
//! synthetic input or silent screen grabs, so the feature does not exist there.

use crate::geom::Rect;
use crate::scrollcore::{self, Io, Note, Outcome, Shared};
use crate::stitch::Frame;
use std::sync::Arc;
use std::time::Duration;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat, ImageOrder, BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, MOTION_NOTIFY_EVENT};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

/// X keysym of the Escape key.
const XK_ESCAPE: u32 = 0xff1b;

struct X {
    conn: RustConnection,
    root: u32,
    esc: Option<u8>,
    bpp32: bool,
}

fn open() -> Option<X> {
    let (conn, n) = x11rb::connect(None).ok()?;
    let setup = conn.setup();
    let screen = &setup.roots[n];
    let root = screen.root;
    let bpp32 = setup.pixmap_formats.iter().find(|f| f.depth == screen.root_depth).is_some_and(|f| f.bits_per_pixel == 32) && setup.image_byte_order == ImageOrder::LSB_FIRST;
    // keycode of Escape from the keyboard mapping
    let (min, max) = (setup.min_keycode, setup.max_keycode);
    let mut esc = None;
    if let Ok(cookie) = conn.get_keyboard_mapping(min, max - min + 1) {
        if let Ok(m) = cookie.reply() {
            let per = m.keysyms_per_keycode as usize;
            if per > 0 {
                esc = m.keysyms.chunks(per).position(|c| c.contains(&XK_ESCAPE)).map(|i| min + i as u8);
            }
        }
    }
    // the XTEST extension is needed to scroll
    conn.extension_information("XTEST").ok().flatten()?;
    Some(X { conn, root, esc, bpp32 })
}

impl X {
    fn grab(&self, r: Rect) -> Option<Frame> {
        if !self.bpp32 {
            return None;
        }
        let (w, h) = (r.w() as usize, r.h() as usize);
        let reply = self.conn.get_image(ImageFormat::Z_PIXMAP, self.root, r.l as i16, r.t as i16, w as u16, h as u16, !0).ok()?.reply().ok()?;
        if reply.data.len() < w * h * 4 {
            return None;
        }
        let mut bgra = reply.data[..w * h * 4].to_vec();
        for px in bgra.chunks_exact_mut(4) {
            px[3] = 255;
        }
        Some(Frame { w, h, bgra })
    }

    fn pointer(&self) -> Option<(i16, i16)> {
        let p = self.conn.query_pointer(self.root).ok()?.reply().ok()?;
        Some((p.root_x, p.root_y))
    }

    fn move_to(&self, x: i16, y: i16) {
        let _ = self.conn.xtest_fake_input(MOTION_NOTIFY_EVENT, 0, 0, self.root, x, y, 0);
        let _ = self.conn.flush();
    }

    fn wheel_down(&self, at: (i16, i16), notches: i32) {
        self.move_to(at.0, at.1);
        std::thread::sleep(Duration::from_millis(20));
        for _ in 0..notches {
            let _ = self.conn.xtest_fake_input(BUTTON_PRESS_EVENT, 5, 0, self.root, 0, 0, 0);
            let _ = self.conn.xtest_fake_input(BUTTON_RELEASE_EVENT, 5, 0, self.root, 0, 0, 0);
            let _ = self.conn.flush();
            std::thread::sleep(Duration::from_millis(12));
        }
    }

    fn esc_down(&self) -> bool {
        let Some(code) = self.esc else { return false };
        let Some(keys) = self.conn.query_keymap().ok().and_then(|c| c.reply().ok()) else { return false };
        keys.keys[(code / 8) as usize] & (1 << (code % 8)) != 0
    }
}

fn work(area: Rect, max_rows: usize, sh: &Shared) -> Outcome {
    let Some(x) = open() else { return Outcome { image: None, note: Note::Failed } };
    // let the overlay window disappear from the screen
    std::thread::sleep(Duration::from_millis(350));
    let centre = ((area.r - (area.w() / 20).clamp(4, 10)) as i16, (area.t + area.h() / 2) as i16);
    let saved = x.pointer();
    let mut grab = || x.grab(area);
    let mut wheel = |n: i32| x.wheel_down(centre, n);
    let esc = || x.esc_down();
    let out = scrollcore::run(max_rows, sh, &mut Io { grab: &mut grab, wheel: &mut wheel, esc: &esc });
    if let Some((px, py)) = saved {
        x.move_to(px, py);
    }
    out
}

/// Starts the worker for `area` (root-window pixels). The resident process polls `Shared::done`.
pub fn spawn(area: Rect, max_rows: usize) -> Arc<Shared> {
    let sh = Arc::new(Shared::new());
    let sh2 = sh.clone();
    std::thread::spawn(move || {
        let out = work(area, max_rows, &sh2);
        sh2.finish(out);
    });
    sh
}
