//! Emulation with the wlroots virtual pointer and virtual keyboard
//! protocols (Sway, Hyprland, river, Wayfire, ...).

use anyhow::{bail, Context, Result};
use skerry_core::geometry::{Desktop, Rect};
use skerry_core::input::Emulation;
use skerry_core::keys::code as k;
use skerry_core::proto::Button;
use std::io::Write;
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use wayland_client::protocol::{wl_keyboard, wl_output, wl_pointer, wl_registry, wl_seat};
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, QueueHandle, WEnum};
use wayland_protocols::xdg::xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{zwp_virtual_keyboard_manager_v1, zwp_virtual_keyboard_v1};
use wayland_protocols_wlr::virtual_pointer::v1::client::{zwlr_virtual_pointer_manager_v1, zwlr_virtual_pointer_v1};

use super::ei_emulation::button_code;
use super::{monotonic_us, Displays};

/// US keymap (compiled from xkeyboard-config, MIT licensed) used when the
/// seat has no physical keyboard to copy a keymap from.
const DEFAULT_KEYMAP: &[u8] = include_bytes!("us.xkb");

fn default_keymap() -> Result<(OwnedFd, u32)> {
    let fd = unsafe { libc::memfd_create(c"skerry-keymap".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut file = std::fs::File::from(fd.try_clone()?);
    file.write_all(DEFAULT_KEYMAP)?;
    file.write_all(&[0])?;
    Ok((fd, DEFAULT_KEYMAP.len() as u32 + 1))
}

#[derive(Default, Clone, Copy)]
struct OutputGeom {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    logical: Option<(i32, i32, i32, i32)>,
}

#[derive(Default)]
struct State {
    seat: Option<wl_seat::WlSeat>,
    pointer_mgr: Option<zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1>,
    keyboard_mgr: Option<zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1>,
    xdg_mgr: Option<zxdg_output_manager_v1::ZxdgOutputManagerV1>,
    outputs: Vec<(wl_output::WlOutput, OutputGeom)>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    keymap: Option<(OwnedFd, u32)>,
}

impl State {
    fn rects(&self) -> Vec<Rect> {
        self.outputs
            .iter()
            .map(|(_, g)| match g.logical {
                Some((x, y, w, h)) => Rect::new(x, y, w, h),
                None => Rect::new(g.x, g.y, g.w, g.h),
            })
            .filter(|r| !r.is_empty())
            .collect()
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(state: &mut Self, reg: &wl_registry::WlRegistry, ev: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_registry::Event::Global { name, interface, version } = ev {
            match interface.as_str() {
                "wl_seat" if state.seat.is_none() => state.seat = Some(reg.bind(name, version.min(7), qh, ())),
                "zwlr_virtual_pointer_manager_v1" => state.pointer_mgr = Some(reg.bind(name, version.min(2), qh, ())),
                "zwp_virtual_keyboard_manager_v1" => state.keyboard_mgr = Some(reg.bind(name, 1, qh, ())),
                "zxdg_output_manager_v1" => state.xdg_mgr = Some(reg.bind(name, version.min(3), qh, ())),
                "wl_output" => {
                    let idx = state.outputs.len();
                    let o = reg.bind(name, version.min(4), qh, idx);
                    state.outputs.push((o, OutputGeom::default()));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(state: &mut Self, seat: &wl_seat::WlSeat, ev: wl_seat::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_seat::Event::Capabilities { capabilities: WEnum::Value(caps) } = ev {
            if caps.contains(wl_seat::Capability::Keyboard) && state.keyboard.is_none() {
                state.keyboard = Some(seat.get_keyboard(qh, ()));
            }
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(state: &mut Self, _: &wl_keyboard::WlKeyboard, ev: wl_keyboard::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_keyboard::Event::Keymap { format: WEnum::Value(wl_keyboard::KeymapFormat::XkbV1), fd, size } = ev {
            state.keymap = Some((fd, size));
        }
    }
}

impl Dispatch<wl_output::WlOutput, usize> for State {
    fn event(state: &mut Self, _: &wl_output::WlOutput, ev: wl_output::Event, idx: &usize, _: &Connection, _: &QueueHandle<Self>) {
        let Some((_, g)) = state.outputs.get_mut(*idx) else { return };
        match ev {
            wl_output::Event::Geometry { x, y, .. } => {
                g.x = x;
                g.y = y;
            }
            wl_output::Event::Mode { flags: WEnum::Value(f), width, height, .. } if f.contains(wl_output::Mode::Current) => {
                g.w = width;
                g.h = height;
            }
            _ => {}
        }
    }
}

impl Dispatch<zxdg_output_v1::ZxdgOutputV1, usize> for State {
    fn event(state: &mut Self, _: &zxdg_output_v1::ZxdgOutputV1, ev: zxdg_output_v1::Event, idx: &usize, _: &Connection, _: &QueueHandle<Self>) {
        let Some((_, g)) = state.outputs.get_mut(*idx) else { return };
        let mut l = g.logical.unwrap_or((g.x, g.y, g.w, g.h));
        match ev {
            zxdg_output_v1::Event::LogicalPosition { x, y } => {
                l.0 = x;
                l.1 = y;
            }
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                l.2 = width;
                l.3 = height;
            }
            _ => return,
        }
        g.logical = Some(l);
    }
}

delegate_noop!(State: ignore zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1);
delegate_noop!(State: ignore zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1);
delegate_noop!(State: ignore zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1);
delegate_noop!(State: ignore zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1);
delegate_noop!(State: ignore zxdg_output_manager_v1::ZxdgOutputManagerV1);

pub struct WlrEmulation {
    conn: Connection,
    _queue: EventQueue<State>,
    pointer: zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
    keyboard: Option<zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1>,
    displays: Displays,
    held_mods: u32,
    locked_mods: u32,
}

impl WlrEmulation {
    pub fn new() -> Result<WlrEmulation> {
        let conn = Connection::connect_to_env().context("cannot connect to the Wayland compositor")?;
        let mut queue = conn.new_event_queue::<State>();
        let qh = queue.handle();
        conn.display().get_registry(&qh, ());
        let mut state = State::default();
        queue.roundtrip(&mut state)?;
        let Some(pointer_mgr) = state.pointer_mgr.clone() else {
            bail!("the compositor does not offer zwlr_virtual_pointer_manager_v1");
        };
        if let Some(xdg) = state.xdg_mgr.clone() {
            for (i, (o, _)) in state.outputs.iter().enumerate() {
                xdg.get_xdg_output(o, &qh, i);
            }
        }
        queue.roundtrip(&mut state)?;
        queue.roundtrip(&mut state)?;

        let pointer = pointer_mgr.create_virtual_pointer(state.seat.as_ref(), &qh, ());
        if state.keymap.is_none() {
            state.keymap = default_keymap().map_err(|e| tracing::warn!("default keymap: {e:#}")).ok();
        }
        let keyboard = match (&state.keyboard_mgr, &state.seat, &state.keymap) {
            (Some(mgr), Some(seat), Some((fd, size))) => {
                let kb = mgr.create_virtual_keyboard(seat, &qh, ());
                kb.keymap(wl_keyboard::KeymapFormat::XkbV1 as u32, fd.as_fd(), *size);
                Some(kb)
            }
            _ => {
                tracing::warn!("no virtual keyboard available; keys will not be replayed");
                None
            }
        };
        if let Some(kbd) = state.keyboard.take() {
            kbd.release();
        }
        conn.flush()?;
        let displays = Displays::default();
        displays.set(state.rects());
        Ok(WlrEmulation { conn, _queue: queue, pointer, keyboard, displays, held_mods: 0, locked_mods: 0 })
    }

    pub fn displays(&self) -> Displays {
        self.displays.clone()
    }

    fn time(&self) -> u32 {
        (monotonic_us() / 1000) as u32
    }

    fn flush(&self) {
        if let Err(e) = self.conn.flush() {
            tracing::warn!("wayland flush failed: {e}");
        }
    }
}

/// Real modifier bits of the standard XKB keymaps.
fn modifier_bit(code: u32) -> u32 {
    match code {
        k::LEFTSHIFT | k::RIGHTSHIFT => 1,
        k::LEFTCTRL | k::RIGHTCTRL => 4,
        k::LEFTALT => 8,
        k::LEFTMETA | k::RIGHTMETA => 64,
        k::RIGHTALT => 128, // AltGr (ISO_Level3_Shift) on most layouts
        _ => 0,
    }
}

impl Emulation for WlrEmulation {
    fn motion_abs(&mut self, x: f64, y: f64) {
        let b = Desktop::new(self.displays.get()).bounds();
        let rx = (x - b.x as f64).clamp(0.0, (b.w - 1).max(0) as f64) as u32;
        let ry = (y - b.y as f64).clamp(0.0, (b.h - 1).max(0) as f64) as u32;
        self.pointer.motion_absolute(self.time(), rx, ry, b.w.max(1) as u32, b.h.max(1) as u32);
        self.pointer.frame();
        self.flush();
    }

    fn button(&mut self, button: Button, pressed: bool) {
        let state = if pressed { wl_pointer::ButtonState::Pressed } else { wl_pointer::ButtonState::Released };
        self.pointer.button(self.time(), button_code(button), state);
        self.pointer.frame();
        self.flush();
    }

    fn scroll(&mut self, x: i32, y: i32) {
        let t = self.time();
        self.pointer.axis_source(wl_pointer::AxisSource::Wheel);
        // Wayland: positive values scroll down/right; ~15 units per notch.
        for (axis, v) in [(wl_pointer::Axis::VerticalScroll, -y), (wl_pointer::Axis::HorizontalScroll, x)] {
            if v == 0 {
                continue;
            }
            let value = v as f64 / 8.0;
            if v % 120 == 0 {
                self.pointer.axis_discrete(t, axis, value, v / 120);
            } else {
                self.pointer.axis(t, axis, value);
            }
        }
        self.pointer.frame();
        self.flush();
    }

    fn key(&mut self, code: u32, pressed: bool) {
        let Some(kb) = &self.keyboard else { return };
        kb.key(self.time(), code, if pressed { 1 } else { 0 });
        let bit = modifier_bit(code);
        let mut changed = bit != 0;
        if pressed {
            self.held_mods |= bit;
        } else {
            self.held_mods &= !bit;
        }
        if code == k::CAPSLOCK && pressed {
            self.locked_mods ^= 2;
            changed = true;
        }
        if changed {
            kb.modifiers(self.held_mods, 0, self.locked_mods, 0);
        }
        self.flush();
    }

    fn os_key_repeat(&self) -> bool {
        true
    }
}
