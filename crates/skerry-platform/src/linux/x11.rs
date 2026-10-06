//! X11 backend.
//!
//! Capture: XInput2 raw motion events on the root window tell us when the
//! mouse moves. When the pointer sits on an enabled outer edge and keeps
//! pushing outward, we grab the pointer (with an invisible cursor) and the
//! keyboard; from then on raw motion deltas, button and key events are
//! forwarded until the engine releases the grab. Hotkeys use passive key
//! grabs while not capturing.
//!
//! Emulation: the XTest extension.
//!
//! Key codes: X11 keycodes are evdev codes plus 8 on every modern X server.

use anyhow::{bail, Context, Result};
use skerry_core::geometry::{Desktop, Edge, EdgeSet, Rect};
use skerry_core::input::{Capture, CaptureEvent, CaptureSender, Emulation};
use skerry_core::keys::{Hotkey, HotkeyAction, Mods};
use skerry_core::proto::Button;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xinput::{self, ConnectionExt as _};
use x11rb::protocol::xkb::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    self, ClientMessageEvent, ConnectionExt as _, CreateWindowAux, EventMask, GrabMode, GrabStatus, KeyButMask,
    ModMask, Window, WindowClass,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::{CURRENT_TIME, NONE};

const XI_ALL_MASTER_DEVICES: u16 = 1;

pub fn displays(conn: &RustConnection, screen: usize) -> Vec<Rect> {
    let s = &conn.setup().roots[screen];
    let monitors = conn
        .randr_get_monitors(s.root, true)
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| {
            r.monitors
                .iter()
                .map(|m| Rect::new(m.x as i32, m.y as i32, m.width as i32, m.height as i32))
                .filter(|r| !r.is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if monitors.is_empty() {
        vec![Rect::new(0, 0, s.width_in_pixels as i32, s.height_in_pixels as i32)]
    } else {
        monitors
    }
}

fn button_from_x(detail: u8) -> Option<Button> {
    match detail {
        1 => Some(Button::Left),
        2 => Some(Button::Middle),
        3 => Some(Button::Right),
        8 => Some(Button::Back),
        9 => Some(Button::Forward),
        _ => None,
    }
}

fn button_to_x(b: Button) -> u8 {
    match b {
        Button::Left => 1,
        Button::Middle => 2,
        Button::Right => 3,
        Button::Back => 8,
        Button::Forward => 9,
    }
}

fn mods_from_state(state: KeyButMask) -> Mods {
    let mut m = Mods::NONE;
    if state.contains(KeyButMask::CONTROL) {
        m = m.union(Mods::CTRL);
    }
    if state.contains(KeyButMask::MOD1) {
        m = m.union(Mods::ALT);
    }
    if state.contains(KeyButMask::SHIFT) {
        m = m.union(Mods::SHIFT);
    }
    if state.contains(KeyButMask::MOD4) {
        m = m.union(Mods::META);
    }
    m
}

fn mods_to_x(m: Mods) -> ModMask {
    let mut x = ModMask::from(0u16);
    if m.contains(Mods::CTRL) {
        x |= ModMask::CONTROL;
    }
    if m.contains(Mods::ALT) {
        x |= ModMask::M1;
    }
    if m.contains(Mods::SHIFT) {
        x |= ModMask::SHIFT;
    }
    if m.contains(Mods::META) {
        x |= ModMask::M4;
    }
    x
}

fn fp(v: &xinput::Fp3232) -> f64 {
    v.integral as f64 + v.frac as f64 / 4_294_967_296.0
}

/// Relative x/y from a raw motion event (axes 0 and 1), using the
/// server-processed values so pointer acceleration matches the local feel.
fn raw_delta(e: &xinput::RawMotionEvent) -> (f64, f64) {
    let mut values = e.axisvalues.iter();
    let (mut dx, mut dy) = (0.0, 0.0);
    for axis in 0..2u32 {
        let word = (axis / 32) as usize;
        let set = e.valuator_mask.get(word).is_some_and(|m| m & (1 << (axis % 32)) != 0);
        if set {
            let v = values.next().map(fp).unwrap_or(0.0);
            if axis == 0 {
                dx = v;
            } else {
                dy = v;
            }
        }
    }
    (dx, dy)
}

enum Cmd {
    Grab,
    Release(Option<(f64, f64)>),
    Hotkeys(Vec<(Hotkey, HotkeyAction)>),
}

struct Shared {
    edges: AtomicU8,
    block_drag: AtomicBool,
}

pub struct X11Capture {
    conn: Arc<RustConnection>,
    screen: usize,
    wake: Window,
    cmds: Mutex<std_mpsc::Sender<Cmd>>,
    shared: Arc<Shared>,
}

impl X11Capture {
    pub fn new(tx: CaptureSender) -> Result<X11Capture> {
        let (conn, screen) = x11rb::connect(None).context("cannot connect to the X server")?;
        let conn = Arc::new(conn);
        let root = conn.setup().roots[screen].root;

        let xi = conn.xinput_xi_query_version(2, 2)?.reply().context("XInput2 is not available")?;
        if xi.major_version < 2 {
            bail!("XInput 2 is required");
        }

        let wake = conn.generate_id()?;
        conn.create_window(
            0,
            wake,
            root,
            -10,
            -10,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new().override_redirect(1),
        )?;

        // An invisible cursor shown while the pointer is grabbed.
        let pix = conn.generate_id()?;
        conn.create_pixmap(1, pix, root, 1, 1)?;
        let cursor = conn.generate_id()?;
        conn.create_cursor(cursor, pix, pix, 0, 0, 0, 0, 0, 0, 0, 0)?;
        conn.free_pixmap(pix)?;

        conn.xinput_xi_select_events(
            root,
            &[xinput::EventMask { deviceid: XI_ALL_MASTER_DEVICES, mask: vec![xinput::XIEventMask::RAW_MOTION] }],
        )?;

        // Report key auto-repeat as press, press, ..., release.
        if conn.xkb_use_extension(1, 0).ok().and_then(|c| c.reply().ok()).is_some_and(|r| r.supported) {
            let flag = xkb::PerClientFlag::DETECTABLE_AUTO_REPEAT;
            let _ = conn
                .xkb_per_client_flags(xkb::ID::USE_CORE_KBD.into(), flag, flag, 0u32.into(), 0u32.into(), 0u32.into())
                .map(|c| c.reply());
        }
        conn.flush()?;

        // The master pointer is grabbed through XInput2 so raw motion keeps
        // flowing to us while we hold the grab (a core grab would stop it).
        let pointer_id = conn
            .xinput_xi_query_device(XI_ALL_MASTER_DEVICES)?
            .reply()?
            .infos
            .iter()
            .find(|d| d.type_ == xinput::DeviceType::MASTER_POINTER)
            .map(|d| d.deviceid)
            .unwrap_or(2);

        let shared = Arc::new(Shared { edges: AtomicU8::new(0), block_drag: AtomicBool::new(true) });
        let (cmd_tx, cmd_rx) = std_mpsc::channel();
        let mut worker = Worker {
            conn: conn.clone(),
            screen,
            root,
            cursor,
            pointer_id,
            tx,
            shared: shared.clone(),
            grabbed: false,
            hotkeys: Vec::new(),
            swallowed: HashSet::new(),
            desktop: Desktop::new(displays(&conn, screen)),
            desktop_at: Instant::now(),
        };
        std::thread::Builder::new().name("skerry-x11-capture".into()).spawn(move || {
            if let Err(e) = worker.run(cmd_rx) {
                tracing::error!("X11 capture stopped: {e:#}");
                let _ = worker.tx.send(CaptureEvent::Status(skerry_core::input::BackendStatus::Error(format!(
                    "Lost connection to the X server: {e}"
                ))));
            }
        })?;

        Ok(X11Capture { conn, screen, wake, cmds: Mutex::new(cmd_tx), shared })
    }

    fn send(&self, cmd: Cmd) {
        let _ = self.cmds.lock().unwrap().send(cmd);
        let ev = ClientMessageEvent::new(32, self.wake, xproto::AtomEnum::NONE, [0u32; 5]);
        let _ = self.conn.send_event(false, self.wake, EventMask::NO_EVENT, ev);
        let _ = self.conn.flush();
    }
}

impl Capture for X11Capture {
    fn set_edges(&self, edges: EdgeSet) {
        self.shared.edges.store(edges.bits(), Ordering::Relaxed);
    }
    fn set_hotkeys(&self, hotkeys: Vec<(Hotkey, HotkeyAction)>) {
        self.send(Cmd::Hotkeys(hotkeys));
    }
    fn set_block_while_dragging(&self, block: bool) {
        self.shared.block_drag.store(block, Ordering::Relaxed);
    }
    fn grab(&self) {
        self.send(Cmd::Grab);
    }
    fn release(&self, warp: Option<(f64, f64)>) {
        self.send(Cmd::Release(warp));
    }
    fn displays(&self) -> Vec<Rect> {
        displays(&self.conn, self.screen)
    }
}

struct Worker {
    conn: Arc<RustConnection>,
    screen: usize,
    root: Window,
    cursor: u32,
    pointer_id: u16,
    tx: CaptureSender,
    shared: Arc<Shared>,
    grabbed: bool,
    hotkeys: Vec<(Hotkey, HotkeyAction)>,
    swallowed: HashSet<u8>,
    desktop: Desktop,
    desktop_at: Instant,
}

impl Worker {
    fn run(&mut self, cmds: std_mpsc::Receiver<Cmd>) -> Result<()> {
        loop {
            let event = self.conn.wait_for_event()?;
            while let Ok(cmd) = cmds.try_recv() {
                self.on_cmd(cmd)?;
            }
            self.on_event(event)?;
        }
    }

    fn on_cmd(&mut self, cmd: Cmd) -> Result<()> {
        match cmd {
            Cmd::Grab => {
                if !self.grabbed && self.grab()? {
                    self.grabbed = true;
                }
            }
            Cmd::Release(warp) => {
                if self.grabbed {
                    self.conn.xinput_xi_ungrab_device(CURRENT_TIME, self.pointer_id)?;
                    self.conn.ungrab_keyboard(CURRENT_TIME)?;
                    self.grabbed = false;
                }
                if let Some((x, y)) = warp {
                    self.conn.warp_pointer(NONE, self.root, 0, 0, 0, 0, x.round() as i16, y.round() as i16)?;
                }
                self.conn.flush()?;
            }
            Cmd::Hotkeys(hk) => {
                for (old, _) in &self.hotkeys {
                    for extra in lock_variants() {
                        self.conn.ungrab_key((old.key + 8) as u8, self.root, mods_to_x(old.mods) | extra)?;
                    }
                }
                for (new, _) in &hk {
                    for extra in lock_variants() {
                        self.conn.grab_key(
                            false,
                            self.root,
                            mods_to_x(new.mods) | extra,
                            (new.key + 8) as u8,
                            GrabMode::ASYNC,
                            GrabMode::ASYNC,
                        )?;
                    }
                }
                self.conn.flush()?;
                self.hotkeys = hk;
            }
        }
        Ok(())
    }

    fn grab(&self) -> Result<bool> {
        let mask =
            xinput::XIEventMask::RAW_MOTION | xinput::XIEventMask::BUTTON_PRESS | xinput::XIEventMask::BUTTON_RELEASE;
        let p = self
            .conn
            .xinput_xi_grab_device(
                self.root,
                CURRENT_TIME,
                self.cursor,
                self.pointer_id,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
                xinput::GrabOwner::NO_OWNER,
                &[u32::from(mask)],
            )?
            .reply()?;
        if p.status != GrabStatus::SUCCESS {
            tracing::debug!("pointer grab failed: {:?}", p.status);
            return Ok(false);
        }
        let k = self.conn.grab_keyboard(false, self.root, CURRENT_TIME, GrabMode::ASYNC, GrabMode::ASYNC)?.reply()?;
        if k.status != GrabStatus::SUCCESS {
            tracing::debug!("keyboard grab failed: {:?}", k.status);
            self.conn.xinput_xi_ungrab_device(CURRENT_TIME, self.pointer_id)?;
            self.conn.flush()?;
            return Ok(false);
        }
        Ok(true)
    }

    fn hotkey_for(&self, keycode: u8, state: KeyButMask) -> Option<HotkeyAction> {
        let code = keycode as u32 - 8;
        let mods = mods_from_state(state);
        self.hotkeys.iter().find(|(hk, _)| hk.key == code && hk.mods == mods).map(|(_, a)| *a)
    }

    fn on_event(&mut self, event: Event) -> Result<()> {
        match event {
            Event::XinputRawMotion(e) => {
                let (dx, dy) = raw_delta(&e);
                if dx == 0.0 && dy == 0.0 {
                    return Ok(());
                }
                if self.grabbed {
                    let _ = self.tx.send(CaptureEvent::Motion { dx, dy });
                } else {
                    self.maybe_begin(dx, dy)?;
                }
            }
            Event::XinputButtonPress(e) if self.grabbed => {
                let scroll = match e.detail {
                    4 => Some((0, 120)),
                    5 => Some((0, -120)),
                    6 => Some((-120, 0)),
                    7 => Some((120, 0)),
                    _ => None,
                };
                if let Some((x, y)) = scroll {
                    let _ = self.tx.send(CaptureEvent::Scroll { x, y });
                } else if let Some(button) = button_from_x(e.detail as u8) {
                    let _ = self.tx.send(CaptureEvent::Button { button, pressed: true });
                }
            }
            Event::XinputButtonRelease(e) if self.grabbed => {
                if let Some(button) = button_from_x(e.detail as u8) {
                    let _ = self.tx.send(CaptureEvent::Button { button, pressed: false });
                }
            }
            Event::KeyPress(e) => {
                if let Some(action) = self.hotkey_for(e.detail, e.state) {
                    self.swallowed.insert(e.detail);
                    let _ = self.tx.send(CaptureEvent::Hotkey(action));
                } else if self.swallowed.contains(&e.detail) {
                    // auto-repeat of a hotkey
                } else if self.grabbed && e.detail >= 8 {
                    let _ = self.tx.send(CaptureEvent::Key { code: e.detail as u32 - 8, pressed: true });
                }
            }
            Event::KeyRelease(e) => {
                if self.swallowed.remove(&e.detail) {
                    return Ok(());
                }
                if self.grabbed && e.detail >= 8 {
                    let _ = self.tx.send(CaptureEvent::Key { code: e.detail as u32 - 8, pressed: false });
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn maybe_begin(&mut self, dx: f64, dy: f64) -> Result<()> {
        let edges = EdgeSet::from_bits(self.shared.edges.load(Ordering::Relaxed));
        if edges.is_empty() {
            return Ok(());
        }
        let ptr = self.conn.query_pointer(self.root)?.reply()?;
        let buttons = KeyButMask::BUTTON1 | KeyButMask::BUTTON2 | KeyButMask::BUTTON3;
        if self.shared.block_drag.load(Ordering::Relaxed) && u16::from(ptr.mask) & u16::from(buttons) != 0 {
            return Ok(());
        }
        if self.desktop_at.elapsed() > Duration::from_secs(3) {
            self.desktop = Desktop::new(displays(&self.conn, self.screen));
            self.desktop_at = Instant::now();
        }
        let (x, y) = (ptr.root_x as f64, ptr.root_y as f64);
        let Some(edge) = self.desktop.edge_at(x, y) else { return Ok(()) };
        let pushing = match edge {
            Edge::Left => dx < 0.0,
            Edge::Right => dx > 0.0,
            Edge::Top => dy < 0.0,
            Edge::Bottom => dy > 0.0,
        };
        if !edges.contains(edge) || !pushing {
            return Ok(());
        }
        if self.grab()? {
            self.grabbed = true;
            let _ = self.tx.send(CaptureEvent::Begin { edge, x, y });
        }
        Ok(())
    }
}

fn lock_variants() -> [ModMask; 4] {
    let none = ModMask::from(0u16);
    [none, ModMask::LOCK, ModMask::M2, ModMask::LOCK | ModMask::M2]
}

/// Replays input with the XTest extension.
pub struct X11Emulation {
    conn: RustConnection,
    root: Window,
    scroll: (i32, i32),
}

impl X11Emulation {
    pub fn new() -> Result<X11Emulation> {
        let (conn, screen) = x11rb::connect(None).context("cannot connect to the X server")?;
        let root = conn.setup().roots[screen].root;
        conn.xtest_get_version(2, 2)?.reply().context("the XTest extension is not available")?;
        Ok(X11Emulation { conn, root, scroll: (0, 0) })
    }

    fn fake(&self, kind: u8, detail: u8, x: i16, y: i16) {
        let root = if kind == xproto::MOTION_NOTIFY_EVENT { self.root } else { NONE };
        if let Err(e) = self.conn.xtest_fake_input(kind, detail, CURRENT_TIME, root, x, y, 0) {
            tracing::warn!("XTest failed: {e}");
        }
    }

    fn click(&self, button: u8) {
        self.fake(xproto::BUTTON_PRESS_EVENT, button, 0, 0);
        self.fake(xproto::BUTTON_RELEASE_EVENT, button, 0, 0);
    }

    fn flush(&self) {
        let _ = self.conn.flush();
    }
}

impl Emulation for X11Emulation {
    fn motion_abs(&mut self, x: f64, y: f64) {
        self.fake(xproto::MOTION_NOTIFY_EVENT, 0, x.round() as i16, y.round() as i16);
        self.flush();
    }

    fn button(&mut self, button: Button, pressed: bool) {
        let kind = if pressed { xproto::BUTTON_PRESS_EVENT } else { xproto::BUTTON_RELEASE_EVENT };
        self.fake(kind, button_to_x(button), 0, 0);
        self.flush();
    }

    fn scroll(&mut self, x: i32, y: i32) {
        self.scroll.0 += x;
        self.scroll.1 += y;
        while self.scroll.1 >= 120 {
            self.click(4);
            self.scroll.1 -= 120;
        }
        while self.scroll.1 <= -120 {
            self.click(5);
            self.scroll.1 += 120;
        }
        while self.scroll.0 >= 120 {
            self.click(7);
            self.scroll.0 -= 120;
        }
        while self.scroll.0 <= -120 {
            self.click(6);
            self.scroll.0 += 120;
        }
        self.flush();
    }

    fn key(&mut self, code: u32, pressed: bool) {
        if code + 8 > 255 {
            return;
        }
        let kind = if pressed { xproto::KEY_PRESS_EVENT } else { xproto::KEY_RELEASE_EVENT };
        self.fake(kind, (code + 8) as u8, 0, 0);
        self.flush();
    }

    fn os_key_repeat(&self) -> bool {
        // The X server auto-repeats held XTest keys itself.
        true
    }
}

/// Advertised screen layout for X11.
pub struct X11Screen {
    conn: RustConnection,
    screen: usize,
}

impl X11Screen {
    pub fn new() -> Result<X11Screen> {
        let (conn, screen) = x11rb::connect(None).context("cannot connect to the X server")?;
        Ok(X11Screen { conn, screen })
    }
}

impl skerry_core::input::ScreenSource for X11Screen {
    fn displays(&self) -> Vec<Rect> {
        displays(&self.conn, self.screen)
    }
}
