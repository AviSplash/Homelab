//! Emulation through the RemoteDesktop portal and libei (GNOME, KDE).
//!
//! The portal asks the user for permission the first time; the restore
//! token it returns is saved so later runs do not ask again.

use futures_util::StreamExt;
use reis::ei;
use reis::event::{DeviceCapability, EiEvent};
use skerry_core::geometry::Rect;
use skerry_core::input::{BackendStatus, Emulation};
use skerry_core::proto::Button;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ashpd::desktop::remote_desktop::{ConnectToEISOptions, DeviceType, RemoteDesktop, SelectDevicesOptions, StartOptions};
use ashpd::desktop::{CreateSessionOptions, PersistMode};

use super::{monotonic_us, Displays};

pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;
pub const BTN_MIDDLE: u32 = 0x112;
pub const BTN_SIDE: u32 = 0x113;
pub const BTN_EXTRA: u32 = 0x114;
pub const BTN_FORWARD: u32 = 0x115;
pub const BTN_BACK: u32 = 0x116;

pub fn button_code(b: Button) -> u32 {
    match b {
        Button::Left => BTN_LEFT,
        Button::Right => BTN_RIGHT,
        Button::Middle => BTN_MIDDLE,
        Button::Back => BTN_SIDE,
        Button::Forward => BTN_EXTRA,
    }
}

pub fn button_from_code(c: u32) -> Option<Button> {
    match c {
        BTN_LEFT => Some(Button::Left),
        BTN_RIGHT => Some(Button::Right),
        BTN_MIDDLE => Some(Button::Middle),
        BTN_SIDE | BTN_BACK => Some(Button::Back),
        BTN_EXTRA | BTN_FORWARD => Some(Button::Forward),
        _ => None,
    }
}

/// Is a RemoteDesktop portal with pointer support reachable?
pub async fn available() -> bool {
    let probe = async {
        let rd = RemoteDesktop::new().await?;
        rd.available_device_types().await
    };
    matches!(tokio::time::timeout(Duration::from_secs(3), probe).await, Ok(Ok(t)) if t.contains(DeviceType::Pointer))
}

struct Live {
    context: ei::Context,
    serial: u32,
    sequence: u32,
    devices: Vec<(reis::event::Device, bool)>,
}

struct Shared {
    live: Mutex<Option<Live>>,
    status: Mutex<BackendStatus>,
    displays: Displays,
}

pub struct EiEmulation {
    shared: Arc<Shared>,
}

impl EiEmulation {
    pub fn start(token_path: Option<PathBuf>) -> EiEmulation {
        let shared = Arc::new(Shared {
            live: Mutex::new(None),
            status: Mutex::new(BackendStatus::NeedsPermission(
                "Waiting for permission to control this computer. Approve the system dialog.".into(),
            )),
            displays: Displays::default(),
        });
        let s = shared.clone();
        super::spawn_local_thread("skerry-ei-emulation", move || async move {
            if let Err(e) = run(s.clone(), token_path).await {
                tracing::warn!("remote desktop portal: {e:#}");
                *s.status.lock().unwrap() = BackendStatus::Error(format!("Remote control was not allowed: {e}"));
            }
            *s.live.lock().unwrap() = None;
        });
        EiEmulation { shared }
    }

    pub fn displays(&self) -> Displays {
        self.shared.displays.clone()
    }

    fn emit(&self, mut f: impl FnMut(&reis::event::Device) -> bool) {
        let mut guard = self.shared.live.lock().unwrap();
        let Some(live) = guard.as_mut() else { return };
        for (device, resumed) in &live.devices {
            if *resumed && f(device) {
                device.device().frame(live.serial, monotonic_us());
                let _ = live.context.flush();
                return;
            }
        }
    }
}

async fn run(shared: Arc<Shared>, token_path: Option<PathBuf>) -> anyhow::Result<()> {
    let rd = RemoteDesktop::new().await?;
    let session = rd.create_session(CreateSessionOptions::default()).await?;
    let token = token_path.as_ref().and_then(|p| std::fs::read_to_string(p).ok());
    rd.select_devices(
        &session,
        SelectDevicesOptions::default()
            .set_devices(DeviceType::Keyboard | DeviceType::Pointer)
            .set_persist_mode(PersistMode::ExplicitlyRevoked)
            .set_restore_token(token.as_deref()),
    )
    .await?
    .response()?;
    let selected = rd.start(&session, None, StartOptions::default()).await?.response()?;
    if let (Some(path), Some(t)) = (&token_path, selected.restore_token()) {
        let _ = std::fs::write(path, t);
    }
    let fd = rd.connect_to_eis(&session, ConnectToEISOptions::default()).await?;
    let context = ei::Context::new(UnixStream::from(fd))?;
    let (connection, mut events) = context.handshake_tokio("skerry", ei::handshake::ContextType::Sender).await?;
    *shared.live.lock().unwrap() =
        Some(Live { context: context.clone(), serial: connection.serial(), sequence: 0, devices: Vec::new() });
    *shared.status.lock().unwrap() = BackendStatus::Ok;
    tracing::info!("connected to the RemoteDesktop portal");

    while let Some(ev) = events.next().await {
        let ev = ev?;
        let mut guard = shared.live.lock().unwrap();
        let Some(live) = guard.as_mut() else { break };
        match ev {
            EiEvent::SeatAdded(e) => {
                e.seat.bind_capabilities(
                    DeviceCapability::Pointer
                        | DeviceCapability::PointerAbsolute
                        | DeviceCapability::Keyboard
                        | DeviceCapability::Scroll
                        | DeviceCapability::Button,
                );
                let _ = live.context.flush();
            }
            EiEvent::DeviceAdded(e) => {
                if e.device.has_capability(DeviceCapability::PointerAbsolute) {
                    let regions = e
                        .device
                        .regions()
                        .iter()
                        .map(|r| Rect::new(r.x as i32, r.y as i32, r.width as i32, r.height as i32))
                        .collect();
                    shared.displays.set(regions);
                }
                live.devices.push((e.device, false));
            }
            EiEvent::DeviceRemoved(e) => live.devices.retain(|(d, _)| *d != e.device),
            EiEvent::DeviceResumed(e) => {
                live.serial = e.serial;
                live.sequence += 1;
                e.device.device().start_emulating(e.serial, live.sequence);
                let _ = live.context.flush();
                if let Some(d) = live.devices.iter_mut().find(|(d, _)| *d == e.device) {
                    d.1 = true;
                }
            }
            EiEvent::DevicePaused(e) => {
                live.serial = e.serial;
                if let Some(d) = live.devices.iter_mut().find(|(d, _)| *d == e.device) {
                    d.1 = false;
                }
            }
            EiEvent::Disconnected(d) => {
                anyhow::bail!("the compositor ended the session: {:?}", d.explanation);
            }
            _ => {}
        }
    }
    drop((rd, session));
    Ok(())
}

impl Emulation for EiEmulation {
    fn motion_abs(&mut self, x: f64, y: f64) {
        self.emit(|d| match d.interface::<ei::PointerAbsolute>() {
            Some(p) => {
                p.motion_absolute(x as f32, y as f32);
                true
            }
            None => false,
        });
    }

    fn button(&mut self, button: Button, pressed: bool) {
        let state = if pressed { ei::button::ButtonState::Press } else { ei::button::ButtonState::Released };
        self.emit(|d| match d.interface::<ei::Button>() {
            Some(b) => {
                b.button(button_code(button), state);
                true
            }
            None => false,
        });
    }

    fn scroll(&mut self, x: i32, y: i32) {
        // libei: positive values scroll down/right, in 1/120 of a detent.
        self.emit(|d| match d.interface::<ei::Scroll>() {
            Some(s) => {
                s.scroll_discrete(x, -y);
                true
            }
            None => false,
        });
    }

    fn key(&mut self, code: u32, pressed: bool) {
        let state = if pressed { ei::keyboard::KeyState::Press } else { ei::keyboard::KeyState::Released };
        self.emit(|d| match d.interface::<ei::Keyboard>() {
            Some(k) => {
                k.key(code, state);
                true
            }
            None => false,
        });
    }

    fn os_key_repeat(&self) -> bool {
        true
    }

    fn status(&self) -> BackendStatus {
        self.shared.status.lock().unwrap().clone()
    }
}
