//! Linux: X11 or Wayland, chosen from the session type.

use anyhow::{bail, Result};
use skerry_core::input::{Capture, CaptureSender, Emulation, ScreenSource};

pub mod x11;

type Parts = (Box<dyn Capture>, Box<dyn Emulation>, Box<dyn ScreenSource>, String);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Session {
    X11,
    Wayland,
}

fn session() -> Option<Session> {
    match std::env::var("SKERRY_BACKEND").ok().as_deref() {
        Some("x11") => return Some(Session::X11),
        Some("wayland") => return Some(Session::Wayland),
        _ => {}
    }
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var("XDG_SESSION_TYPE").is_ok_and(|t| t == "wayland");
    if wayland {
        Some(Session::Wayland)
    } else if std::env::var_os("DISPLAY").is_some() {
        Some(Session::X11)
    } else {
        None
    }
}

pub async fn create(tx: CaptureSender) -> Result<Parts> {
    match session() {
        Some(Session::X11) => {
            let capture = x11::X11Capture::new(tx)?;
            let emulation = x11::X11Emulation::new()?;
            let screen = x11::X11Screen::new()?;
            Ok((Box::new(capture), Box::new(emulation), Box::new(screen), "X11".into()))
        }
        Some(Session::Wayland) => bail!("Wayland support is not built yet"),
        None => bail!("no graphical session found (neither WAYLAND_DISPLAY nor DISPLAY is set)"),
    }
}
