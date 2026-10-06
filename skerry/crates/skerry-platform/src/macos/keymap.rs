//! macOS virtual key codes (`kVK_*` from HIToolbox Events.h) <-> evdev.

use skerry_core::keys::code as k;

const TABLE: &[(u16, u32)] = &[
    (0x00, k::A),
    (0x01, k::S),
    (0x02, k::D),
    (0x03, k::F),
    (0x04, k::H),
    (0x05, k::G),
    (0x06, k::Z),
    (0x07, k::X),
    (0x08, k::C),
    (0x09, k::V),
    (0x0a, k::KEY_102ND),
    (0x0b, k::B),
    (0x0c, k::Q),
    (0x0d, k::W),
    (0x0e, k::E),
    (0x0f, k::R),
    (0x10, k::Y),
    (0x11, k::T),
    (0x12, k::KEY_1),
    (0x13, k::KEY_1 + 1),
    (0x14, k::KEY_1 + 2),
    (0x15, k::KEY_1 + 3),
    (0x16, k::KEY_1 + 5),
    (0x17, k::KEY_1 + 4),
    (0x18, k::EQUAL),
    (0x19, k::KEY_1 + 8),
    (0x1a, k::KEY_1 + 6),
    (0x1b, k::MINUS),
    (0x1c, k::KEY_1 + 7),
    (0x1d, k::KEY_0),
    (0x1e, k::RIGHTBRACE),
    (0x1f, k::O),
    (0x20, k::U),
    (0x21, k::LEFTBRACE),
    (0x22, k::I),
    (0x23, k::P),
    (0x24, k::ENTER),
    (0x25, k::L),
    (0x26, k::J),
    (0x27, k::APOSTROPHE),
    (0x28, k::K),
    (0x29, k::SEMICOLON),
    (0x2a, k::BACKSLASH),
    (0x2b, k::COMMA),
    (0x2c, k::SLASH),
    (0x2d, k::N),
    (0x2e, k::M),
    (0x2f, k::DOT),
    (0x30, k::TAB),
    (0x31, k::SPACE),
    (0x32, k::GRAVE),
    (0x33, k::BACKSPACE),
    (0x35, k::ESC),
    (0x36, k::RIGHTMETA),
    (0x37, k::LEFTMETA),
    (0x38, k::LEFTSHIFT),
    (0x39, k::CAPSLOCK),
    (0x3a, k::LEFTALT),
    (0x3b, k::LEFTCTRL),
    (0x3c, k::RIGHTSHIFT),
    (0x3d, k::RIGHTALT),
    (0x3e, k::RIGHTCTRL),
    (0x40, k::F17),
    (0x41, k::KPDOT),
    (0x43, k::KPASTERISK),
    (0x45, k::KPPLUS),
    (0x47, k::NUMLOCK),
    (0x48, k::VOLUMEUP),
    (0x49, k::VOLUMEDOWN),
    (0x4a, k::MUTE),
    (0x4b, k::KPSLASH),
    (0x4c, k::KPENTER),
    (0x4e, k::KPMINUS),
    (0x4f, k::F18),
    (0x50, k::F19),
    (0x51, k::KPEQUAL),
    (0x52, k::KP0),
    (0x53, k::KP1),
    (0x54, k::KP2),
    (0x55, k::KP3),
    (0x56, k::KP4),
    (0x57, k::KP5),
    (0x58, k::KP6),
    (0x59, k::KP7),
    (0x5a, k::F20),
    (0x5b, k::KP8),
    (0x5c, k::KP9),
    (0x5d, k::YEN),
    (0x5e, k::RO),
    (0x5f, k::KPJPCOMMA),
    (0x60, k::F5),
    (0x61, k::F6),
    (0x62, k::F7),
    (0x63, k::F3),
    (0x64, k::F8),
    (0x65, k::F9),
    (0x66, k::MUHENKAN),
    (0x67, k::F11),
    (0x68, k::HENKAN),
    (0x69, k::F13),
    (0x6a, k::F16),
    (0x6b, k::F14),
    (0x6d, k::F10),
    (0x6e, k::COMPOSE),
    (0x6f, k::F12),
    (0x71, k::F15),
    (0x72, k::INSERT),
    (0x73, k::HOME),
    (0x74, k::PAGEUP),
    (0x75, k::DELETE),
    (0x76, k::F4),
    (0x77, k::END),
    (0x78, k::F2),
    (0x79, k::PAGEDOWN),
    (0x7a, k::F1),
    (0x7b, k::LEFT),
    (0x7c, k::RIGHT),
    (0x7d, k::DOWN),
    (0x7e, k::UP),
];

/// PC keys without a Mac key of their own, sent as the keys Macs
/// conventionally use in their place.
const PC_EXTRAS: &[(u32, u16)] = &[(k::SYSRQ, 0x69), (k::SCROLLLOCK, 0x6b), (k::PAUSE, 0x71)];

pub fn to_evdev(mac: u16) -> Option<u32> {
    TABLE.iter().find(|(m, _)| *m == mac).map(|(_, e)| *e)
}

pub fn from_evdev(code: u32) -> Option<u16> {
    TABLE
        .iter()
        .find(|(_, e)| *e == code)
        .map(|(m, _)| *m)
        .or_else(|| PC_EXTRAS.iter().find(|(e, _)| *e == code).map(|(_, m)| *m))
}

/// Device-dependent flag bit for a modifier key (from IOLLEvent.h), used to
/// tell press from release in FlagsChanged events.
pub fn modifier_mask(mac: u16) -> Option<u64> {
    Some(match mac {
        0x3b => 0x0000_0001, // left control
        0x38 => 0x0000_0002, // left shift
        0x3c => 0x0000_0004, // right shift
        0x37 => 0x0000_0008, // left command
        0x36 => 0x0000_0010, // right command
        0x3a => 0x0000_0020, // left option
        0x3d => 0x0000_0040, // right option
        0x3e => 0x0000_2000, // right control
        _ => return None,
    })
}
