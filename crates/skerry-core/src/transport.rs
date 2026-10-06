//! Encrypted connections between peers.
//!
//! Each TCP connection starts with a `Noise_XX_25519_ChaChaPoly_BLAKE2s`
//! handshake. Both sides prove possession of their long-term key, and the
//! resulting session keys encrypt and authenticate everything that follows.
//!
//! On the wire every Noise message is prefixed with a 2-byte big-endian
//! length. Inside, the first plaintext byte says whether more fragments of
//! the same application message follow, so messages larger than one Noise
//! frame (64 KiB) can be sent.
//!
//! Each live connection has two outgoing queues: input and control messages
//! always go first, and bulk data (clipboard contents) is sent between them
//! so typing never waits behind a large image.

use anyhow::{bail, Context, Result};
use snow::StatelessTransportState;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::proto::Message;

pub const NOISE_PARAMS: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";
const PROLOGUE: &[u8] = b"skerry/1";
const MAX_NOISE: usize = 65535;
const TAG_LEN: usize = 16;
const MAX_FRAGMENT: usize = MAX_NOISE - TAG_LEN - 1;
/// Upper bound for a single reassembled message.
pub const MAX_MESSAGE: usize = 1 << 20;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

pub type ConnId = u64;

async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, data: &[u8]) -> std::io::Result<()> {
    let mut buf = Vec::with_capacity(2 + data.len());
    buf.extend_from_slice(&(data.len() as u16).to_be_bytes());
    buf.extend_from_slice(data);
    w.write_all(&buf).await
}

async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Vec<u8>> {
    let mut len = [0u8; 2];
    r.read_exact(&mut len).await?;
    let n = u16::from_be_bytes(len) as usize;
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf).await?;
    Ok(buf)
}

pub async fn connect(addr: SocketAddr) -> Result<TcpStream> {
    let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
        .await
        .with_context(|| format!("timed out connecting to {addr}"))?
        .with_context(|| format!("connecting to {addr}"))?;
    Ok(stream)
}

pub struct SecureReader {
    r: OwnedReadHalf,
    st: Arc<StatelessTransportState>,
    nonce: u64,
    partial: Vec<u8>,
}

impl SecureReader {
    pub async fn recv(&mut self) -> Result<Message> {
        loop {
            let frame = read_frame(&mut self.r).await?;
            let mut plain = vec![0u8; frame.len()];
            let n = self
                .st
                .read_message(self.nonce, &frame, &mut plain)
                .map_err(|e| anyhow::anyhow!("decrypt failed: {e}"))?;
            self.nonce += 1;
            if n == 0 {
                bail!("empty frame");
            }
            let more = plain[0] != 0;
            self.partial.extend_from_slice(&plain[1..n]);
            if self.partial.len() > MAX_MESSAGE {
                bail!("message too large");
            }
            if !more {
                let msg = Message::decode(&self.partial).context("malformed message")?;
                self.partial.clear();
                return Ok(msg);
            }
        }
    }
}

pub struct SecureWriter {
    w: OwnedWriteHalf,
    st: Arc<StatelessTransportState>,
    nonce: u64,
}

impl SecureWriter {
    pub async fn send(&mut self, msg: &Message) -> Result<()> {
        let plain = msg.encode();
        if plain.len() > MAX_MESSAGE {
            bail!("message too large ({} bytes)", plain.len());
        }
        let count = plain.len().div_ceil(MAX_FRAGMENT).max(1);
        let mut out = Vec::with_capacity(plain.len() + count * (2 + 1 + TAG_LEN));
        let mut frag = Vec::with_capacity(plain.len().min(MAX_FRAGMENT) + 1);
        let mut cipher = vec![0u8; MAX_NOISE];
        for i in 0..count {
            let start = i * MAX_FRAGMENT;
            let end = (start + MAX_FRAGMENT).min(plain.len());
            frag.clear();
            frag.push(if i + 1 < count { 1 } else { 0 });
            frag.extend_from_slice(&plain[start..end]);
            let n = self
                .st
                .write_message(self.nonce, &frag, &mut cipher)
                .map_err(|e| anyhow::anyhow!("encrypt failed: {e}"))?;
            self.nonce += 1;
            out.extend_from_slice(&(n as u16).to_be_bytes());
            out.extend_from_slice(&cipher[..n]);
        }
        self.w.write_all(&out).await?;
        Ok(())
    }

    pub async fn shutdown(&mut self) {
        let _ = self.w.shutdown().await;
    }
}

/// An established, encrypted connection that has not been handed to its
/// background tasks yet.
pub struct Session {
    pub remote_static: [u8; 32],
    pub handshake_hash: Vec<u8>,
    pub initiator: bool,
    pub peer_addr: SocketAddr,
    reader: SecureReader,
    writer: SecureWriter,
}

impl Session {
    pub async fn send(&mut self, msg: &Message) -> Result<()> {
        self.writer.send(msg).await
    }

    pub async fn recv(&mut self) -> Result<Message> {
        self.reader.recv().await
    }

    /// Hand the connection to background reader/writer tasks.
    pub fn spawn(self, id: ConnId, events: mpsc::UnboundedSender<ConnEvent>) -> ConnHandle {
        let Session { mut reader, mut writer, .. } = self;
        let (hi_tx, mut hi_rx) = mpsc::unbounded_channel::<Message>();
        let (bulk_tx, mut bulk_rx) = mpsc::channel::<Message>(4);

        let write_task = tokio::spawn(async move {
            loop {
                let msg = tokio::select! {
                    biased;
                    m = hi_rx.recv() => m,
                    m = bulk_rx.recv() => m,
                };
                let Some(msg) = msg else { break };
                let bye = matches!(msg, Message::Bye);
                if let Err(e) = writer.send(&msg).await {
                    tracing::debug!(conn = id, "write failed: {e:#}");
                    break;
                }
                if bye {
                    writer.shutdown().await;
                    break;
                }
            }
        });

        let read_task = tokio::spawn(async move {
            loop {
                match reader.recv().await {
                    Ok(msg) => {
                        if events.send(ConnEvent::Message(id, msg)).is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        tracing::debug!(conn = id, "connection closed: {e:#}");
                        break;
                    }
                }
            }
            let _ = events.send(ConnEvent::Closed(id));
        });

        ConnHandle { id, hi: hi_tx, bulk: bulk_tx, _tasks: Arc::new(AbortOnDrop(vec![write_task, read_task])) }
    }
}

/// Run the Noise XX handshake on a fresh TCP stream.
pub async fn handshake(stream: TcpStream, private_key: &[u8; 32], initiator: bool) -> Result<Session> {
    tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake_inner(stream, private_key, initiator))
        .await
        .context("handshake timed out")?
}

async fn handshake_inner(mut stream: TcpStream, private_key: &[u8; 32], initiator: bool) -> Result<Session> {
    stream.set_nodelay(true)?;
    let peer_addr = stream.peer_addr()?;
    let builder = snow::Builder::new(NOISE_PARAMS.parse()?).local_private_key(private_key)?.prologue(PROLOGUE)?;
    let mut hs = if initiator { builder.build_initiator()? } else { builder.build_responder()? };
    let mut buf = vec![0u8; MAX_NOISE];

    if initiator {
        let n = hs.write_message(&[], &mut buf)?; // -> e
        write_frame(&mut stream, &buf[..n]).await?;
        let f = read_frame(&mut stream).await?; // <- e, ee, s, es
        hs.read_message(&f, &mut buf)?;
        let n = hs.write_message(&[], &mut buf)?; // -> s, se
        write_frame(&mut stream, &buf[..n]).await?;
    } else {
        let f = read_frame(&mut stream).await?;
        hs.read_message(&f, &mut buf)?;
        let n = hs.write_message(&[], &mut buf)?;
        write_frame(&mut stream, &buf[..n]).await?;
        let f = read_frame(&mut stream).await?;
        hs.read_message(&f, &mut buf)?;
    }

    let remote = hs.get_remote_static().context("peer sent no static key")?;
    let mut remote_static = [0u8; 32];
    remote_static.copy_from_slice(remote);
    let handshake_hash = hs.get_handshake_hash().to_vec();
    let st = Arc::new(hs.into_stateless_transport_mode()?);
    let (r, w) = stream.into_split();
    Ok(Session {
        remote_static,
        handshake_hash,
        initiator,
        peer_addr,
        reader: SecureReader { r, st: st.clone(), nonce: 0, partial: Vec::new() },
        writer: SecureWriter { w, st, nonce: 0 },
    })
}

struct AbortOnDrop(Vec<JoinHandle<()>>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        for t in &self.0 {
            t.abort();
        }
    }
}

#[derive(Debug)]
pub enum ConnEvent {
    Message(ConnId, Message),
    Closed(ConnId),
}

/// Sending side of a live connection. Dropping the last clone closes it.
#[derive(Clone)]
pub struct ConnHandle {
    pub id: ConnId,
    hi: mpsc::UnboundedSender<Message>,
    bulk: mpsc::Sender<Message>,
    _tasks: Arc<AbortOnDrop>,
}

impl ConnHandle {
    /// Queue a message ahead of any bulk data.
    pub fn send(&self, msg: Message) {
        let _ = self.hi.send(msg);
    }

    /// Queue bulk data, waiting while the connection is busy.
    pub async fn send_bulk(&self, msg: Message) -> bool {
        self.bulk.send(msg).await.is_ok()
    }

    pub fn is_closed(&self) -> bool {
        self.hi.is_closed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Identity;
    use tokio::net::TcpListener;

    async fn pair_sessions() -> (Session, Session, Identity, Identity) {
        let a = Identity::generate().unwrap();
        let b = Identity::generate().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let bk = b.private;
        let server = tokio::spawn(async move {
            let (s, _) = listener.accept().await.unwrap();
            handshake(s, &bk, false).await.unwrap()
        });
        let client = handshake(connect(addr).await.unwrap(), &a.private, true).await.unwrap();
        let server = server.await.unwrap();
        (client, server, a, b)
    }

    #[tokio::test]
    async fn handshake_authenticates_both_keys() {
        let (c, s, a, b) = pair_sessions().await;
        assert_eq!(c.remote_static, b.public);
        assert_eq!(s.remote_static, a.public);
        assert_eq!(c.handshake_hash, s.handshake_hash);
    }

    #[tokio::test]
    async fn messages_roundtrip_including_large() {
        let (mut c, mut s, _, _) = pair_sessions().await;
        c.send(&Message::Ping(7)).await.unwrap();
        assert_eq!(s.recv().await.unwrap(), Message::Ping(7));
        let big = Message::ClipData { id: 1, data: vec![0xab; 200_000] };
        s.send(&big).await.unwrap();
        assert_eq!(c.recv().await.unwrap(), big);
    }

    #[tokio::test]
    async fn spawned_connection_prioritises_input() {
        let (c, s, _, _) = pair_sessions().await;
        let (tx_c, _rx_c) = mpsc::unbounded_channel();
        let (tx_s, mut rx_s) = mpsc::unbounded_channel();
        let hc = c.spawn(1, tx_c);
        let _hs = s.spawn(2, tx_s);
        hc.send(Message::Key { code: 30, pressed: true });
        assert!(hc.send_bulk(Message::ClipData { id: 1, data: vec![1; 1000] }).await);
        let mut got = Vec::new();
        while got.len() < 2 {
            match rx_s.recv().await.unwrap() {
                ConnEvent::Message(_, m) => got.push(m),
                ConnEvent::Closed(_) => panic!("closed"),
            }
        }
        assert_eq!(got[0], Message::Key { code: 30, pressed: true });
        drop(hc);
        loop {
            if let ConnEvent::Closed(id) = rx_s.recv().await.unwrap() {
                assert_eq!(id, 2);
                break;
            }
        }
    }
}
