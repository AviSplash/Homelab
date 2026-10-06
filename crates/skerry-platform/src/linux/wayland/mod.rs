//! Wayland backends.
//!
//! Wayland compositors do not let ordinary clients see or inject global
//! input, so Skerry goes through the interfaces compositors provide for it:
//!
//! - Being controlled (emulation):
//!   1. wlroots virtual pointer/keyboard protocols (Sway, Hyprland, river,
//!      Wayfire...), no permission prompt;
//!   2. otherwise the RemoteDesktop portal with libei (GNOME 45+, KDE
//!      Plasma 6.1+), which asks the user once and remembers the answer.
//! - Controlling others (capture): the InputCapture portal with libei
//!   (GNOME 45+, KDE Plasma 6.1+). Other compositors can be controlled but
//!   cannot yet share their own mouse and keyboard.

mod ei_capture;
mod ei_emulation;
mod wlr;

use anyhow::Result;
use skerry_core::geometry::Rect;
use skerry_core::input::{BackendStatus, Capture, CaptureSender, Emulation, NullCapture, NullEmulation, ScreenSource};
use std::sync::{Arc, Mutex};

type Parts = (Box<dyn Capture>, Box<dyn Emulation>, Box<dyn ScreenSource>, String);

/// Shared, updatable list of displays.
#[derive(Clone, Default)]
pub struct Displays(pub Arc<Mutex<Vec<Rect>>>);

impl Displays {
    pub fn set(&self, d: Vec<Rect>) {
        *self.0.lock().unwrap() = d;
    }
    pub fn get(&self) -> Vec<Rect> {
        self.0.lock().unwrap().clone()
    }
}

struct FirstNonEmpty(Vec<Displays>);

impl ScreenSource for FirstNonEmpty {
    fn displays(&self) -> Vec<Rect> {
        self.0.iter().map(Displays::get).find(|d| !d.is_empty()).unwrap_or_default()
    }
}

/// Run a future on its own thread with a single-threaded tokio runtime
/// (libei event streams are not `Send`).
pub fn spawn_local_thread<F>(name: &str, fut: impl FnOnce() -> F + Send + 'static)
where
    F: std::future::Future<Output = ()> + 'static,
{
    let name = name.to_string();
    let _ =
        std::thread::Builder::new().name(name.clone()).spawn(
            move || match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt.block_on(fut()),
                Err(e) => tracing::error!("{name}: cannot start runtime: {e}"),
            },
        );
}

/// Monotonic clock in microseconds (libei and Wayland timestamps).
pub fn monotonic_us() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1_000
}

pub async fn create(tx: CaptureSender) -> Result<Parts> {
    let token = skerry_core::config::Paths::default_location().ok().map(|p| p.dir.join("remote-desktop.token"));

    let mut sources = Vec::new();
    let (emulation, emu_name): (Box<dyn Emulation>, &str) = match wlr::WlrEmulation::new() {
        Ok(w) => {
            sources.push(w.displays());
            (Box::new(w), "wlroots virtual input")
        }
        Err(wlr_err) => {
            tracing::debug!("wlroots virtual input unavailable: {wlr_err:#}");
            if ei_emulation::available().await {
                let e = ei_emulation::EiEmulation::start(token);
                sources.push(e.displays());
                (Box::new(e), "RemoteDesktop portal")
            } else {
                let status = BackendStatus::Unsupported(
                    "This Wayland desktop offers no way for other computers to control it \
                     (needs GNOME 45+, KDE Plasma 6.1+, or a wlroots compositor)."
                        .into(),
                );
                (Box::new(NullEmulation { status }), "no emulation")
            }
        }
    };

    let (capture, cap_name): (Box<dyn Capture>, &str) = if ei_capture::available().await {
        let c = ei_capture::EiCapture::start(tx);
        sources.push(c.displays_handle());
        (Box::new(c), "InputCapture portal")
    } else {
        let status = BackendStatus::Unsupported(
            "This Wayland desktop cannot share its mouse and keyboard yet (needs GNOME 45+ or KDE Plasma 6.1+). \
             Other computers can still control it."
                .into(),
        );
        let displays = FirstNonEmpty(sources.clone()).displays();
        (Box::new(NullCapture { displays, status }), "no capture")
    };

    Ok((capture, emulation, Box::new(FirstNonEmpty(sources)), format!("Wayland ({emu_name}, {cap_name})")))
}
