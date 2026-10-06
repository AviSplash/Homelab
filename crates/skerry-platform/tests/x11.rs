//! Exercises the X11 backend against a real X server.
//! Run with: Xvfb :99 -screen 0 1920x1080x24 & DISPLAY=:99 cargo test -p skerry-platform --test x11 -- --ignored
#![cfg(target_os = "linux")]

use std::time::Duration;

use skerry_core::geometry::{Edge, EdgeSet};
use skerry_core::input::CaptureEvent;
use skerry_core::keys::{code, Hotkey, HotkeyAction};
use skerry_core::proto::Button;
use tokio::sync::mpsc;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::protocol::xtest::ConnectionExt as _;

struct Xt {
    conn: x11rb::rust_connection::RustConnection,
    root: u32,
}

impl Xt {
    fn new() -> Xt {
        let (conn, s) = x11rb::connect(None).unwrap();
        let root = conn.setup().roots[s].root;
        Xt { conn, root }
    }
    fn rel(&self, dx: i16, dy: i16) {
        self.conn.xtest_fake_input(xproto::MOTION_NOTIFY_EVENT, 1, 0, x11rb::NONE, dx, dy, 0).unwrap();
        self.conn.flush().unwrap();
    }
    fn abs(&self, x: i16, y: i16) {
        self.conn.xtest_fake_input(xproto::MOTION_NOTIFY_EVENT, 0, 0, self.root, x, y, 0).unwrap();
        self.conn.flush().unwrap();
    }
    fn key(&self, evdev: u32, down: bool) {
        let t = if down { xproto::KEY_PRESS_EVENT } else { xproto::KEY_RELEASE_EVENT };
        self.conn.xtest_fake_input(t, (evdev + 8) as u8, 0, x11rb::NONE, 0, 0, 0).unwrap();
        self.conn.flush().unwrap();
    }
    fn button(&self, b: u8, down: bool) {
        let t = if down { xproto::BUTTON_PRESS_EVENT } else { xproto::BUTTON_RELEASE_EVENT };
        self.conn.xtest_fake_input(t, b, 0, x11rb::NONE, 0, 0, 0).unwrap();
        self.conn.flush().unwrap();
    }
    fn pointer(&self) -> (i16, i16) {
        let r = self.conn.query_pointer(self.root).unwrap().reply().unwrap();
        (r.root_x, r.root_y)
    }
}

async fn next(rx: &mut mpsc::UnboundedReceiver<CaptureEvent>, pred: impl Fn(&CaptureEvent) -> bool) -> CaptureEvent {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let ev = tokio::time::timeout_at(deadline, rx.recv()).await.expect("timed out waiting for capture event").unwrap();
        eprintln!("event: {ev:?}");
        if pred(&ev) {
            return ev;
        }
    }
}

#[tokio::test]
#[ignore = "needs an X server (Xvfb)"]
async fn x11_capture_and_emulation() {
    let xt = Xt::new();
    let platform = {
        let (tx, rx) = mpsc::unbounded_channel();
        std::env::set_var("SKERRY_BACKEND", "x11");
        (skerry_platform::create(tx).await.unwrap(), rx)
    };
    let (mut p, mut rx) = platform;
    assert_eq!(p.description, "X11");
    let displays = p.capture.displays();
    assert_eq!(displays.len(), 1);
    assert_eq!((displays[0].w, displays[0].h), (1920, 1080));

    // Emulation moves the real pointer.
    p.emulation.motion_abs(100.0, 200.0);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(xt.pointer(), (100, 200));

    // Pushing against an enabled edge starts capture.
    p.capture.set_edges([Edge::Right].into_iter().collect::<EdgeSet>());
    xt.abs(1919, 500);
    tokio::time::sleep(Duration::from_millis(50)).await;
    xt.rel(3, 0);
    let ev = next(&mut rx, |e| matches!(e, CaptureEvent::Begin { .. })).await;
    assert_eq!(ev, CaptureEvent::Begin { edge: Edge::Right, x: 1919.0, y: 500.0 });

    // While captured, motion, keys and buttons are reported.
    xt.rel(2, 1);
    match next(&mut rx, |e| matches!(e, CaptureEvent::Motion { dy, .. } if *dy != 0.0)).await {
        CaptureEvent::Motion { dx, dy } => assert!(dx > 0.0 && dy > 0.0, "{dx},{dy}"),
        _ => unreachable!(),
    }
    xt.key(code::A, true);
    assert_eq!(next(&mut rx, |e| matches!(e, CaptureEvent::Key { .. })).await, CaptureEvent::Key { code: code::A, pressed: true });
    xt.key(code::A, false);
    assert_eq!(next(&mut rx, |e| matches!(e, CaptureEvent::Key { .. })).await, CaptureEvent::Key { code: code::A, pressed: false });
    xt.button(3, true);
    assert_eq!(
        next(&mut rx, |e| matches!(e, CaptureEvent::Button { .. })).await,
        CaptureEvent::Button { button: Button::Right, pressed: true }
    );
    xt.button(3, false);
    xt.button(5, true);
    xt.button(5, false);
    assert_eq!(next(&mut rx, |e| matches!(e, CaptureEvent::Scroll { .. })).await, CaptureEvent::Scroll { x: 0, y: -120 });

    // Releasing warps the cursor back.
    p.capture.release(Some((1900.0, 400.0)));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(xt.pointer(), (1900, 400));

    // Hotkeys fire while not capturing.
    p.capture.set_hotkeys(vec![(Hotkey::parse("ctrl+alt+shift+right").unwrap(), HotkeyAction::Switch(Edge::Right))]);
    tokio::time::sleep(Duration::from_millis(100)).await;
    for k in [code::LEFTCTRL, code::LEFTALT, code::LEFTSHIFT] {
        xt.key(k, true);
    }
    xt.key(code::RIGHT, true);
    assert_eq!(
        next(&mut rx, |e| matches!(e, CaptureEvent::Hotkey(_))).await,
        CaptureEvent::Hotkey(HotkeyAction::Switch(Edge::Right))
    );
    xt.key(code::RIGHT, false);
    for k in [code::LEFTCTRL, code::LEFTALT, code::LEFTSHIFT] {
        xt.key(k, false);
    }

    // Emulated keys and scroll do not error.
    p.emulation.key(code::B, true);
    p.emulation.key(code::B, false);
    p.emulation.scroll(0, 240);
    p.emulation.button(Button::Left, true);
    p.emulation.button(Button::Left, false);
}
