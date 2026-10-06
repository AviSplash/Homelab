# Skerry wire protocol (version 1)

This document describes how Skerry computers talk to each other. The
reference implementation is in `crates/skerry-core` (`transport.rs`,
`pairing.rs`, `proto.rs`, `engine.rs`).

## Discovery

Each computer advertises the DNS-SD service `_skerry._tcp.local.` with:

| TXT key | Value |
|---|---|
| `id` | device id (16 hex characters) |
| `name` | user-visible computer name |
| `os` | `windows`, `macos`, `linux` or `other` |
| `v` | Skerry version |

The instance name is the device id. Discovery only finds candidates;
nothing in a TXT record is trusted.

## Identity

Every install generates a long-term X25519 key pair (stored in
`identity.key`, readable only by the user on Unix).

- **device id**: first 8 bytes of SHA-256(public key), hex encoded.
- **fingerprint**: first 20 hex characters of SHA-256(public key), in groups
  of four, e.g. `1c84-3bbb-adba-346f-1f47`.

## Transport

TCP (default port 24870), `TCP_NODELAY`.

1. **Handshake:** `Noise_XX_25519_ChaChaPoly_BLAKE2s` with prologue
   `skerry/1`. Each handshake message is sent as a 2-byte big-endian
   length followed by the message. Handshake payloads are empty.
2. **Transport messages:** each Noise transport message is also
   length-prefixed (2 bytes). Nonces start at 0 in each direction and
   increase by one per message.
3. **Records:** the first plaintext byte of every transport message is a
   flag: `1` = more fragments of the same application message follow, `0` =
   last fragment. Fragments are concatenated; a reassembled message may be up
   to 1 MiB.
4. **Application messages** are `postcard`-encoded `Message` values (see
   `proto.rs`). New variants are only ever appended.

Both sides send `Hello` first:

```text
Hello { protocol: 1, device_id, name, os, app_version, port }
```

A peer whose `device_id` does not match the id derived from its Noise static
key is disconnected. Mismatched `protocol` versions are rejected.

Each connection has two send queues: control/input messages, and bulk data
(clipboard chunks). The writer always drains the control queue first.

`Ping`/`Pong` are exchanged every 2 s; a connection with no traffic for 10 s
is closed.

## Pairing

A connection between computers that don't know each other's key is
*unpaired*. Only `PairRequest`, `PairSpake`, `PairConfirm`, `PairResult`,
`Ping`, `Pong` and `Bye` are accepted on it.

```text
Initiator (types the code)                Responder (shows the code)
--------------------------                --------------------------
PairRequest                    ───────▶   generates a random 6-digit code and shows it
user types code
SPAKE2 start_a(code)
PairSpake { msg_a }            ───────▶   SPAKE2 start_b(code), finish(msg_a) → K
                               ◀───────   PairSpake { msg_b }
                               ◀───────   PairConfirm { HMAC(K, "skerry-pair-confirm-v1" ‖ 0x02 ‖ h) }
finish(msg_b) → K, verify
PairConfirm { HMAC(K, … ‖ 0x01 ‖ h) } ─▶  verify, store initiator's public key
                               ◀───────   PairResult { ok: true }
store responder's public key
```

- SPAKE2 uses the Ed25519 group with identities `skerry-pair-initiator` and
  `skerry-pair-responder`.
- `h` is the Noise handshake hash of this connection, so a correct code also
  proves there is no man in the middle on this connection.
- An attacker gets one guess (1 in 1,000,000) per attempt. The responder
  refuses further pairing requests after 5 wrong codes in 5 minutes.
- Pairing attempts expire after 3 minutes.

After pairing, both computers store each other's public key. Later
connections are trusted if the Noise static key matches.

## Control

Each computer describes its screen with `Screen { displays, neighbors,
available }`. `displays` are rectangles in the coordinate space the
computer's input replay uses. `neighbors` lists the device id at each edge
(left, right, top, bottom).

The computer whose physical mouse is in use (the *controller*) keeps a
virtual cursor on the target's desktop:

| Message | Meaning |
|---|---|
| `Enter { x, y }` | Control starts; place the cursor at (x, y) |
| `Motion { x, y }` | Absolute cursor position |
| `Button { button, pressed }` | `left`, `right`, `middle`, `back`, `forward` |
| `Scroll { x, y }` | 1/120ths of a wheel notch; +y is up, +x is right |
| `Key { code, pressed }` | Linux evdev key code (physical key position) |
| `Leave` | Control ends; release every held key and button |

When the virtual cursor crosses an edge of the target's desktop, the
controller looks up that edge in the target's `neighbors`. If it is the
controller itself, control returns home. If it is another connected computer,
the controller sends `Leave` to the current target and `Enter` to the next
one. If no neighbor is set on that edge but it is the edge the cursor came in
through, the cursor returns to where it came from.

Command/Control translation happens on the controller: when exactly one of
the two computers is a Mac, `LEFTMETA`↔`LEFTCTRL` and `RIGHTMETA`↔`RIGHTCTRL`
are swapped (if enabled).

`LayoutHint { edge }` asks the receiver to place the sender on `edge` (or
remove it with `None`). It's applied only if that edge is free.

## Clipboard

When control leaves a computer, that computer sends its clipboard to the
computer gaining focus, if the content's hash differs from what that peer is
known to have:

```text
ClipBegin { id, kind: Text | Png, len }
ClipData  { id, data }   (48 KiB chunks, sent through the bulk queue)
ClipEnd   { id }
```

Text is UTF-8. Images are PNG-encoded RGBA. Payloads larger than 64 MiB are
not sent. When a computer receives a clipboard while controlling a third
computer, it forwards it there.
