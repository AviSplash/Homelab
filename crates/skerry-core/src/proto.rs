//! Messages exchanged between Skerry peers.
//!
//! Every message is serialised with `postcard` and sent inside the Noise
//! encrypted channel (see [`crate::transport`]). New variants must only be
//! appended to keep older peers able to decode the ones they know.

use serde::{Deserialize, Serialize};

use crate::geometry::{Edge, Rect};
use crate::keys::OsKind;

/// Bumped whenever a change would break older peers.
pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u32,
    pub device_id: String,
    pub name: String,
    pub os: OsKind,
    pub app_version: String,
    /// TCP port this peer accepts connections on.
    pub port: u16,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ScreenInfo {
    /// Displays in the coordinate space used for `Enter` / `Motion`.
    pub displays: Vec<Rect>,
    /// The device id placed at each edge (left, right, top, bottom).
    pub neighbors: [Option<String>; 4],
    /// False while this peer has sharing paused.
    pub available: bool,
}

impl ScreenInfo {
    pub fn neighbor(&self, edge: Edge) -> Option<&str> {
        self.neighbors[edge.index()].as_deref()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Button {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipKind {
    Text,
    Png,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Message {
    Hello(Hello),

    // Pairing (only these are accepted before a peer is trusted).
    PairRequest,
    PairSpake { msg: Vec<u8> },
    PairConfirm { mac: Vec<u8> },
    PairResult { ok: bool, reason: String },

    // Layout and screen description.
    Screen(ScreenInfo),
    /// "Please place me on this edge of your screen" (or remove me).
    LayoutHint { edge: Option<Edge> },

    // Input, sent by the computer whose mouse and keyboard are in use.
    Enter { x: f64, y: f64 },
    Leave,
    Motion { x: f64, y: f64 },
    Button { button: Button, pressed: bool },
    /// Scroll amounts in 1/120ths of a wheel notch. Positive y scrolls up,
    /// positive x scrolls right.
    Scroll { x: i32, y: i32 },
    Key { code: u32, pressed: bool },

    // Clipboard transfer, sent in chunks so input is never stuck behind it.
    ClipBegin { id: u64, kind: ClipKind, len: u64 },
    ClipData { id: u64, data: Vec<u8> },
    ClipEnd { id: u64 },

    Ping(u64),
    Pong(u64),
    Bye,
}

impl Message {
    pub fn encode(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("message serialisation cannot fail")
    }

    pub fn decode(bytes: &[u8]) -> Result<Message, postcard::Error> {
        postcard::from_bytes(bytes)
    }

    /// Messages an untrusted (not yet paired) peer may send.
    pub fn allowed_before_pairing(&self) -> bool {
        matches!(
            self,
            Message::PairRequest
                | Message::PairSpake { .. }
                | Message::PairConfirm { .. }
                | Message::PairResult { .. }
                | Message::Ping(_)
                | Message::Pong(_)
                | Message::Bye
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let msgs = vec![
            Message::Hello(Hello {
                protocol: PROTOCOL_VERSION,
                device_id: "abc".into(),
                name: "Desk".into(),
                os: OsKind::Linux,
                app_version: "0.1.0".into(),
                port: 24870,
            }),
            Message::Motion { x: 12.5, y: -3.0 },
            Message::Key { code: 30, pressed: true },
            Message::Screen(ScreenInfo {
                displays: vec![Rect::new(0, 0, 10, 10)],
                neighbors: [None, Some("x".into()), None, None],
                available: true,
            }),
        ];
        for m in msgs {
            assert_eq!(Message::decode(&m.encode()).unwrap(), m);
        }
    }

    #[test]
    fn motion_is_small() {
        // Mouse motion is the hot path; keep it tiny on the wire.
        assert!(Message::Motion { x: 1920.0, y: 1080.0 }.encode().len() <= 20);
    }
}
