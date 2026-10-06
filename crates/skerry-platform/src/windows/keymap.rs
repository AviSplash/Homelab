//! Windows scan codes (PC/XT "set 1", as reported by low-level keyboard
//! hooks) <-> evdev key codes.
//!
//! For the first 88 keys the two are identical by design; the rest are
//! "extended" (E0-prefixed) keys or keys only identifiable by virtual key.

use skerry_core::keys::code as k;

/// Extended (E0) scan codes and their evdev equivalents.
const EXTENDED: &[(u16, u32)] = &[
    (0x1c, k::KPENTER),
    (0x1d, k::RIGHTCTRL),
    (0x35, k::KPSLASH),
    (0x37, k::SYSRQ),
    (0x38, k::RIGHTALT),
    (0x47, k::HOME),
    (0x48, k::UP),
    (0x49, k::PAGEUP),
    (0x4b, k::LEFT),
    (0x4d, k::RIGHT),
    (0x4f, k::END),
    (0x50, k::DOWN),
    (0x51, k::PAGEDOWN),
    (0x52, k::INSERT),
    (0x53, k::DELETE),
    (0x5b, k::LEFTMETA),
    (0x5c, k::RIGHTMETA),
    (0x5d, k::COMPOSE),
    (0x20, k::MUTE),
    (0x2e, k::VOLUMEDOWN),
    (0x30, k::VOLUMEUP),
    (0x22, k::PLAYPAUSE),
    (0x24, k::STOPCD),
    (0x10, k::PREVIOUSSONG),
    (0x19, k::NEXTSONG),
    (0x5e, k::POWER),
    (0x5f, k::SLEEP),
    (0x65, k::SEARCH),
    (0x66, k::BOOKMARKS),
    (0x67, k::REFRESH),
    (0x68, k::STOP),
    (0x69, k::FORWARD),
    (0x6a, k::BACK),
    (0x6b, k::COMPUTER),
    (0x6c, k::MAIL),
    (0x21, k::CALC),
    (0x32, k::HOMEPAGE),
];

/// Non-extended scan codes above 0x58.
const HIGH: &[(u16, u32)] = &[
    (0x54, k::SYSRQ),
    (0x59, k::KPEQUAL),
    (0x64, k::F13),
    (0x65, k::F14),
    (0x66, k::F15),
    (0x67, k::F16),
    (0x68, k::F17),
    (0x69, k::F18),
    (0x6a, k::F19),
    (0x6b, k::F20),
    (0x6c, k::F21),
    (0x6d, k::F22),
    (0x6e, k::F23),
    (0x76, k::F24),
    (0x70, k::KATAKANAHIRAGANA),
    (0x73, k::RO),
    (0x79, k::HENKAN),
    (0x7b, k::MUHENKAN),
    (0x7d, k::YEN),
    (0x7e, k::KPJPCOMMA),
    (0x71, k::HANJA),
    (0x72, k::HANGEUL),
];

const VK_PAUSE: u32 = 0x13;
const VK_NUMLOCK: u32 = 0x90;

/// Virtual keys some keyboards send without a scan code.
const VK_ONLY: &[(u32, u32)] = &[
    (0xad, k::MUTE),
    (0xae, k::VOLUMEDOWN),
    (0xaf, k::VOLUMEUP),
    (0xb0, k::NEXTSONG),
    (0xb1, k::PREVIOUSSONG),
    (0xb2, k::STOPCD),
    (0xb3, k::PLAYPAUSE),
    (0xa6, k::BACK),
    (0xa7, k::FORWARD),
    (0xa8, k::REFRESH),
    (0xaa, k::SEARCH),
    (0xac, k::HOMEPAGE),
];

pub fn to_evdev(scan: u32, extended: bool, vk: u32) -> Option<u32> {
    // Pause and Num Lock share scan code 0x45; only the virtual key tells them apart.
    if vk == VK_PAUSE {
        return Some(k::PAUSE);
    }
    if vk == VK_NUMLOCK {
        return Some(k::NUMLOCK);
    }
    let scan = scan as u16;
    if scan == 0 {
        return VK_ONLY.iter().find(|(v, _)| *v == vk).map(|(_, e)| *e);
    }
    if extended {
        return EXTENDED.iter().find(|(s, _)| *s == scan).map(|(_, e)| *e);
    }
    if (1..=0x58).contains(&scan) {
        return Some(scan as u32);
    }
    HIGH.iter().find(|(s, _)| *s == scan).map(|(_, e)| *e)
}

pub enum WinKey {
    Scan { scan: u16, extended: bool },
    /// Keys that must be sent by virtual key code.
    Vk(u16),
}

pub fn from_evdev(code: u32) -> Option<WinKey> {
    if code == k::PAUSE {
        return Some(WinKey::Vk(VK_PAUSE as u16));
    }
    if code == k::NUMLOCK {
        return Some(WinKey::Vk(VK_NUMLOCK as u16));
    }
    if (1..=0x58).contains(&code) {
        return Some(WinKey::Scan { scan: code as u16, extended: false });
    }
    if let Some((s, _)) = EXTENDED.iter().find(|(_, e)| *e == code) {
        return Some(WinKey::Scan { scan: *s, extended: true });
    }
    HIGH.iter().find(|(_, e)| *e == code).map(|(s, _)| WinKey::Scan { scan: *s, extended: false })
}
