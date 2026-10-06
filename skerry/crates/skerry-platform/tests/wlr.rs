//! Exercises the wlroots virtual-input backend against a real compositor.
//! Run with a headless sway: `WLR_BACKENDS=headless sway -c /dev/null &`, then
//! `WAYLAND_DISPLAY=wayland-1 cargo test -p skerry-platform --test wlr -- --ignored`
#![cfg(target_os = "linux")]

use skerry_core::keys::code;
use skerry_core::proto::Button;
use tokio::sync::mpsc;

#[tokio::test]
#[ignore = "needs a wlroots compositor"]
async fn wlroots_virtual_input() {
    std::env::set_var("SKERRY_BACKEND", "wayland");
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut p = skerry_platform::create(tx).await.unwrap();
    assert!(p.description.contains("wlroots"), "{}", p.description);
    let displays = p.screen.displays();
    assert!(!displays.is_empty(), "outputs found");
    eprintln!("displays: {displays:?}");

    p.emulation.motion_abs(100.0, 100.0);
    p.emulation.button(Button::Left, true);
    p.emulation.button(Button::Left, false);
    p.emulation.scroll(0, -120);
    p.emulation.scroll(0, 30);
    p.emulation.key(code::LEFTSHIFT, true);
    p.emulation.key(code::A, true);
    p.emulation.key(code::A, false);
    p.emulation.key(code::LEFTSHIFT, false);
    // Capture is not available on wlroots; the backend says so.
    assert!(!p.capture.status().is_ok());
    std::thread::sleep(std::time::Duration::from_millis(200));
    // If sway is reachable over IPC, confirm it registered our virtual devices.
    if let Ok(out) = std::process::Command::new("swaymsg").args(["-t", "get_inputs", "-r"]).output() {
        let text = String::from_utf8_lossy(&out.stdout);
        if out.status.success() {
            assert!(text.contains("\"pointer\""), "virtual pointer registered: {text}");
            assert!(text.contains("\"keyboard\""), "virtual keyboard registered: {text}");
        }
    }
}
