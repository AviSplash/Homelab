//! Pairing two computers with a 6-digit code.
//!
//! The Noise handshake tells each side the other's public key, but not
//! whether that key belongs to the computer the user meant. To check, the
//! computer being paired shows a short code, the user types it on the other
//! one, and both run SPAKE2 with it. SPAKE2 gives an attacker at most one
//! guess per attempt (no offline brute force), and both sides then MAC the
//! Noise handshake hash with the SPAKE2 key, so a matching code also proves
//! nobody sat in the middle of this particular connection.

use anyhow::{anyhow, Result};
use hmac::{Hmac, Mac};
use rand::Rng;
use sha2::Sha256;
use spake2::{Ed25519Group, Identity, Password, Spake2};

const ID_INITIATOR: &[u8] = b"skerry-pair-initiator";
const ID_RESPONDER: &[u8] = b"skerry-pair-responder";
const CONFIRM_LABEL: &[u8] = b"skerry-pair-confirm-v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The computer where the user typed the code.
    Initiator,
    /// The computer that displayed the code.
    Responder,
}

pub fn generate_code() -> String {
    let n: u32 = rand::rng().random_range(0..1_000_000);
    format!("{n:06}")
}

/// Keep only the digits of what the user typed ("123 456" -> "123456").
pub fn normalize_code(input: &str) -> String {
    input.chars().filter(|c| c.is_ascii_digit()).collect()
}

pub struct Spake(Spake2<Ed25519Group>);

/// Begin SPAKE2. Returns the state and the message to send to the peer.
pub fn start(role: Role, code: &str) -> (Spake, Vec<u8>) {
    let pw = Password::new(code.as_bytes());
    let (a, b) = (Identity::new(ID_INITIATOR), Identity::new(ID_RESPONDER));
    let (state, msg) = match role {
        Role::Initiator => Spake2::<Ed25519Group>::start_a(&pw, &a, &b),
        Role::Responder => Spake2::<Ed25519Group>::start_b(&pw, &a, &b),
    };
    (Spake(state), msg)
}

impl Spake {
    pub fn finish(self, peer_msg: &[u8]) -> Result<Vec<u8>> {
        self.0.finish(peer_msg).map_err(|e| anyhow!("pairing exchange failed: {e:?}"))
    }
}

/// MAC proving knowledge of the SPAKE2 key, bound to this connection.
pub fn confirmation(key: &[u8], role: Role, handshake_hash: &[u8]) -> Vec<u8> {
    mac(key, role, handshake_hash).finalize().into_bytes().to_vec()
}

pub fn verify_confirmation(key: &[u8], role: Role, handshake_hash: &[u8], received: &[u8]) -> bool {
    mac(key, role, handshake_hash).verify_slice(received).is_ok()
}

fn mac(key: &[u8], role: Role, handshake_hash: &[u8]) -> Hmac<Sha256> {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC accepts any key length");
    m.update(CONFIRM_LABEL);
    m.update(&[match role {
        Role::Initiator => 1,
        Role::Responder => 2,
    }]);
    m.update(handshake_hash);
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(code_a: &str, code_b: &str) -> bool {
        let h = b"handshake-hash";
        let (sa, ma) = start(Role::Initiator, code_a);
        let (sb, mb) = start(Role::Responder, code_b);
        let ka = sa.finish(&mb).unwrap();
        let kb = sb.finish(&ma).unwrap();
        let from_b = confirmation(&kb, Role::Responder, h);
        let from_a = confirmation(&ka, Role::Initiator, h);
        verify_confirmation(&ka, Role::Responder, h, &from_b) && verify_confirmation(&kb, Role::Initiator, h, &from_a)
    }

    #[test]
    fn matching_codes_confirm() {
        assert!(run("123456", "123456"));
    }

    #[test]
    fn wrong_code_fails() {
        assert!(!run("123456", "123457"));
    }

    #[test]
    fn confirmation_is_bound_to_handshake() {
        let (sa, ma) = start(Role::Initiator, "000001");
        let (sb, mb) = start(Role::Responder, "000001");
        let ka = sa.finish(&mb).unwrap();
        let kb = sb.finish(&ma).unwrap();
        let mac = confirmation(&kb, Role::Responder, b"one");
        assert!(!verify_confirmation(&ka, Role::Responder, b"two", &mac));
        assert!(!verify_confirmation(&ka, Role::Initiator, b"one", &mac), "role is bound");
    }

    #[test]
    fn codes() {
        let c = generate_code();
        assert_eq!(c.len(), 6);
        assert!(c.chars().all(|c| c.is_ascii_digit()));
        assert_eq!(normalize_code(" 12-34 56 "), "123456");
    }
}
