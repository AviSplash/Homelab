//! Capture through the InputCapture portal and libei (GNOME 45+, KDE 6.1+).
//!
//! Skerry places pointer barriers along the screen edges that lead to
//! another computer. When the cursor hits one, the compositor activates the
//! capture, hides the cursor and streams all input to us over libei until we
//! release it.

use futures_util::StreamExt;
use reis::ei;
use reis::event::{DeviceCapability, EiEvent};
use skerry_core::geometry::{Desktop, Edge, EdgeSet, Rect};
use skerry_core::input::{BackendStatus, Capture, CaptureEvent, CaptureSender};
use skerry_core::keys::{Hotkey, HotkeyAction, HotkeyMatcher, KeyVerdict};
use std::collections::HashMap;
use std::num::NonZeroU32;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

use ashpd::desktop::input_capture::{
    ActivatedBarrier, Barrier, Capabilities, ConnectToEISOptions, CreateSessionOptions, EnableOptions, GetZonesOptions,
    InputCapture, ReleaseOptions, SetPointerBarriersOptions,
};
use ashpd::desktop::Session;

use super::ei_emulation::button_from_code;
use super::Displays;

/// Is an InputCapture portal reachable?
pub async fn available() -> bool {
    let probe = async {
        let ic = InputCapture::new().await?;
        ic.supported_capabilities().await
    };
    matches!(tokio::time::timeout(Duration::from_secs(3), probe).await, Ok(Ok(c)) if c.contains(Capabilities::Pointer))
}

enum Cmd {
    Edges(EdgeSet),
    Release(Option<(f64, f64)>),
}

struct Shared {
    tx: CaptureSender,
    displays: Displays,
    status: Mutex<BackendStatus>,
    hotkeys: Mutex<HotkeyMatcher>,
}

pub struct EiCapture {
    cmd: mpsc::UnboundedSender<Cmd>,
    shared: Arc<Shared>,
}

impl EiCapture {
    pub fn start(tx: CaptureSender) -> EiCapture {
        let shared = Arc::new(Shared {
            tx,
            displays: Displays::default(),
            status: Mutex::new(BackendStatus::NeedsPermission(
                "Waiting for permission to capture input. Approve the system dialog.".into(),
            )),
            hotkeys: Mutex::new(HotkeyMatcher::default()),
        });
        let (cmd, rx) = mpsc::unbounded_channel();
        let s = shared.clone();
        super::spawn_local_thread("skerry-ei-capture", move || async move {
            if let Err(e) = run(s.clone(), rx).await {
                tracing::warn!("input capture portal: {e:#}");
                let status = BackendStatus::Error(format!("Input capture stopped: {e}"));
                *s.status.lock().unwrap() = status.clone();
                let _ = s.tx.send(CaptureEvent::Status(status));
            }
        });
        EiCapture { cmd, shared }
    }

    pub fn displays_handle(&self) -> Displays {
        self.shared.displays.clone()
    }
}

impl Capture for EiCapture {
    fn set_edges(&self, edges: EdgeSet) {
        let _ = self.cmd.send(Cmd::Edges(edges));
    }
    fn set_hotkeys(&self, hotkeys: Vec<(Hotkey, HotkeyAction)>) {
        self.shared.hotkeys.lock().unwrap().set_bindings(hotkeys);
    }
    fn grab(&self) {
        // The portal only starts a capture at a barrier; hotkeys cannot be
        // seen while the cursor is local on Wayland anyway.
    }
    fn release(&self, warp: Option<(f64, f64)>) {
        let _ = self.cmd.send(Cmd::Release(warp));
    }
    fn displays(&self) -> Vec<Rect> {
        self.shared.displays.get()
    }
    fn status(&self) -> BackendStatus {
        self.shared.status.lock().unwrap().clone()
    }
}

/// Barrier coordinates for one edge of one zone (portal convention: the
/// barrier sits just outside the zone on the right/bottom).
fn barrier_line(r: &Rect, edge: Edge) -> (i32, i32, i32, i32) {
    match edge {
        Edge::Left => (r.x, r.y, r.x, r.bottom() - 1),
        Edge::Right => (r.right(), r.y, r.right(), r.bottom() - 1),
        Edge::Top => (r.x, r.y, r.right() - 1, r.y),
        Edge::Bottom => (r.x, r.bottom(), r.right() - 1, r.bottom()),
    }
}

/// A barrier id, the edge it guards and its line (x1, y1, x2, y2).
type BarrierSpec = (u32, Edge, (i32, i32, i32, i32));

/// Barriers along the outer edges of the desktop for the enabled edges.
pub fn barriers_for(zones: &[Rect], edges: EdgeSet) -> Vec<BarrierSpec> {
    let b = Desktop::new(zones.to_vec()).bounds();
    let mut out = Vec::new();
    let mut id = 1;
    for z in zones {
        for edge in edges.iter() {
            let outer = match edge {
                Edge::Left => z.x == b.x,
                Edge::Right => z.right() == b.right(),
                Edge::Top => z.y == b.y,
                Edge::Bottom => z.bottom() == b.bottom(),
            };
            if outer {
                out.push((id, edge, barrier_line(z, edge)));
                id += 1;
            }
        }
    }
    out
}

struct State<'a> {
    ic: &'a InputCapture,
    session: &'a Session<InputCapture>,
    shared: &'a Shared,
    zones: Vec<Rect>,
    zone_set: u32,
    edges: EdgeSet,
    barrier_edges: HashMap<u32, Edge>,
    active: Option<u32>,
    scroll: (f32, f32),
    discrete_in_frame: bool,
}

impl State<'_> {
    async fn refresh_zones(&mut self) -> anyhow::Result<()> {
        let zones = self.ic.zones(self.session, GetZonesOptions::default()).await?.response()?;
        self.zone_set = zones.zone_set();
        self.zones = zones
            .regions()
            .iter()
            .map(|r| Rect::new(r.x_offset(), r.y_offset(), r.width() as i32, r.height() as i32))
            .collect();
        self.shared.displays.set(self.zones.clone());
        Ok(())
    }

    async fn apply_barriers(&mut self) -> anyhow::Result<()> {
        let list = barriers_for(&self.zones, self.edges);
        self.barrier_edges = list.iter().map(|(id, e, _)| (*id, *e)).collect();
        let barriers: Vec<Barrier> =
            list.iter().map(|(id, _, pos)| Barrier::new(NonZeroU32::new(*id).unwrap(), *pos)).collect();
        let resp = self
            .ic
            .set_pointer_barriers(self.session, &barriers, self.zone_set, SetPointerBarriersOptions::default())
            .await?
            .response()?;
        if !resp.failed_barriers().is_empty() {
            tracing::warn!("compositor rejected barriers {:?}", resp.failed_barriers());
        }
        self.ic.enable(self.session, EnableOptions::default()).await?;
        Ok(())
    }

    fn send(&self, ev: CaptureEvent) {
        let _ = self.shared.tx.send(ev);
    }

    fn on_ei(&mut self, context: &ei::Context, ev: EiEvent) {
        match ev {
            EiEvent::SeatAdded(e) => {
                e.seat.bind_capabilities(
                    DeviceCapability::Pointer
                        | DeviceCapability::PointerAbsolute
                        | DeviceCapability::Keyboard
                        | DeviceCapability::Scroll
                        | DeviceCapability::Button,
                );
                let _ = context.flush();
            }
            _ if self.active.is_none() => {}
            EiEvent::PointerMotion(m) => {
                self.send(CaptureEvent::Motion { dx: m.dx as f64, dy: m.dy as f64 });
            }
            EiEvent::Button(b) => {
                if let Some(button) = button_from_code(b.button) {
                    let pressed = b.state == ei::button::ButtonState::Press;
                    self.send(CaptureEvent::Button { button, pressed });
                }
            }
            EiEvent::ScrollDiscrete(s) => {
                self.discrete_in_frame = true;
                self.send(CaptureEvent::Scroll { x: s.discrete_dx, y: -s.discrete_dy });
            }
            EiEvent::ScrollDelta(s) => {
                self.scroll.0 += s.dx;
                self.scroll.1 += s.dy;
            }
            EiEvent::Frame(_) => {
                // Smooth scrolling (touchpads): ~15 logical pixels per notch.
                if !self.discrete_in_frame && (self.scroll.0 != 0.0 || self.scroll.1 != 0.0) {
                    let x = (self.scroll.0 * 8.0).round() as i32;
                    let y = (-self.scroll.1 * 8.0).round() as i32;
                    if x != 0 || y != 0 {
                        self.send(CaptureEvent::Scroll { x, y });
                    }
                }
                self.scroll = (0.0, 0.0);
                self.discrete_in_frame = false;
            }
            EiEvent::KeyboardKey(k) => {
                let pressed = k.state == ei::keyboard::KeyState::Press;
                match self.shared.hotkeys.lock().unwrap().on_key(k.key, pressed) {
                    KeyVerdict::Fire(a) => self.send(CaptureEvent::Hotkey(a)),
                    KeyVerdict::Swallow => {}
                    KeyVerdict::Pass => self.send(CaptureEvent::Key { code: k.key, pressed }),
                }
            }
            _ => {}
        }
    }
}

async fn run(shared: Arc<Shared>, mut cmds: mpsc::UnboundedReceiver<Cmd>) -> anyhow::Result<()> {
    let ic = InputCapture::new().await?;
    let (session, _caps) = ic
        .create_session(None, CreateSessionOptions::default().set_capabilities(Capabilities::Keyboard | Capabilities::Pointer))
        .await?;
    let fd = ic.connect_to_eis(&session, ConnectToEISOptions::default()).await?;
    let context = ei::Context::new(UnixStream::from(fd))?;
    let (_connection, mut ei_events) = context.handshake_tokio("skerry", ei::handshake::ContextType::Receiver).await?;
    let mut activated = ic.receive_activated().await?;
    let mut deactivated = ic.receive_deactivated().await?;
    let mut disabled = ic.receive_disabled().await?;
    let mut zones_changed = ic.receive_zones_changed().await?;

    let mut st = State {
        ic: &ic,
        session: &session,
        shared: &shared,
        zones: Vec::new(),
        zone_set: 0,
        edges: EdgeSet::empty(),
        barrier_edges: HashMap::new(),
        active: None,
        scroll: (0.0, 0.0),
        discrete_in_frame: false,
    };
    st.refresh_zones().await?;
    *shared.status.lock().unwrap() = BackendStatus::Ok;
    let _ = shared.tx.send(CaptureEvent::Status(BackendStatus::Ok));
    tracing::info!("connected to the InputCapture portal");

    loop {
        tokio::select! {
            cmd = cmds.recv() => match cmd {
                None => break,
                Some(Cmd::Edges(e)) => {
                    if e != st.edges {
                        st.edges = e;
                        st.apply_barriers().await?;
                    }
                }
                Some(Cmd::Release(warp)) => {
                    if let Some(id) = st.active.take() {
                        let opts = ReleaseOptions::default().set_activation_id(id).set_cursor_position(warp);
                        ic.release(&session, opts).await?;
                    }
                }
            },
            Some(a) = activated.next() => {
                let edge = match a.barrier_id() {
                    Some(ActivatedBarrier::Barrier(b)) => st.barrier_edges.get(&b.get()).copied(),
                    _ => None,
                };
                let id = a.activation_id();
                match (edge, id, a.cursor_position()) {
                    (Some(edge), Some(id), Some((x, y))) if st.edges.contains(edge) => {
                        st.active = Some(id);
                        shared.hotkeys.lock().unwrap().reset();
                        st.send(CaptureEvent::Begin { edge, x: x as f64, y: y as f64 });
                    }
                    _ => {
                        let opts = ReleaseOptions::default().set_activation_id(id);
                        ic.release(&session, opts).await?;
                    }
                }
            },
            Some(_) = deactivated.next() => {
                if st.active.take().is_some() {
                    // The compositor ended the capture on its own.
                    st.send(CaptureEvent::Hotkey(HotkeyAction::ReturnHome));
                }
            },
            Some(_) = disabled.next() => {
                st.active = None;
                st.apply_barriers().await?;
            },
            Some(_) = zones_changed.next() => {
                st.refresh_zones().await?;
                st.apply_barriers().await?;
            },
            ev = ei_events.next() => match ev {
                Some(Ok(ev)) => st.on_ei(&context, ev),
                Some(Err(e)) => return Err(e.into()),
                None => anyhow::bail!("libei connection closed"),
            },
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn barriers_only_on_outer_edges() {
        let zones = vec![Rect::new(0, 0, 1920, 1080), Rect::new(1920, 0, 1280, 1024)];
        let edges: EdgeSet = [Edge::Right, Edge::Left].into_iter().collect();
        let b = barriers_for(&zones, edges);
        assert_eq!(b.len(), 2);
        assert!(b.contains(&(1, Edge::Left, (0, 0, 0, 1079))));
        assert!(b.contains(&(2, Edge::Right, (3200, 0, 3200, 1023))));
    }
}
