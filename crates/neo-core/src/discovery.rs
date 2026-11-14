//! Finding your contacts and your own devices on the same network, privately.
//!
//! Every instance multicasts a small announcement every couple of seconds
//! (`{device id, name, port, account attestation}`) so contacts on the same
//! network connect directly, even with the internet down. The announcement is
//! encrypted (XChaCha20-Poly1305, random nonce) with a key derived from the
//! sender's account code, so only people who know that code can read it: your
//! contacts, your linked devices, and someone adding you by your code right
//! now. Everyone else on the network sees an unlinkable blob; Neo never lists
//! strangers. Several instances on one machine share the port via
//! SO_REUSEADDR. The attestation is checked here, and TLS proves the device
//! id when we actually connect.
//!
//! `open` (tests and `neo-cli --discover-all` only) also sends and accepts
//! the old readable announcement, so test devices find each other before
//! they're contacts.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::{Duration, Instant};

use anyhow::Result;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;

use crate::identity::{Attest, DeviceId};
use crate::Peer;

const GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 77, 7);
const PORT: u16 = 47321;
/// Readable announcements (tests only).
const OPEN_VERSION: u8 = 3;
/// Sealed announcements.
const SEALED_VERSION: u8 = 4;
const ANNOUNCE_EVERY: Duration = Duration::from_secs(2);
const EXPIRE_AFTER: Duration = Duration::from_secs(7);

/// What an announcement says (sealed, or open in tests).
#[derive(Serialize, Deserialize)]
struct Announce {
    v: u8,
    id: String,
    name: String,
    port: u16,
    attest: Option<Attest>,
}

/// On the wire when sealed: only a version and the ciphertext.
#[derive(Serialize, Deserialize)]
struct Sealed {
    v: u8,
    /// hex(nonce || ciphertext)
    s: String,
}

/// The key that opens announcements from the account with this code.
pub fn code_key(code: &str) -> [u8; 32] {
    blake3::derive_key("neo lan announce v1", crate::identity::normalize_code(code).as_bytes())
}

fn seal(key: &[u8; 32], plain: &[u8]) -> Option<Vec<u8>> {
    let mut nonce = [0u8; 24];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut nonce).ok()?;
    let mut out = nonce.to_vec();
    out.extend(XChaCha20Poly1305::new(key.into()).encrypt(XNonce::from_slice(&nonce), plain).ok()?);
    Some(out)
}

fn open_with(key: &[u8; 32], data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < 24 + 16 {
        return None;
    }
    XChaCha20Poly1305::new(key.into()).decrypt(XNonce::from_slice(&data[..24]), &data[24..]).ok()
}

fn socket() -> Result<UdpSocket> {
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    s.set_reuse_address(true)?;
    s.set_nonblocking(true)?;
    s.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, PORT).into())?;
    s.join_multicast_v4(&GROUP, &Ipv4Addr::UNSPECIFIED)?;
    s.set_multicast_loop_v4(true)?;
    s.set_multicast_ttl_v4(1)?;
    Ok(UdpSocket::from_std(s.into())?)
}

/// What this device announces, and which announcements it can read.
pub struct Me {
    pub name: String,
    pub attest: Attest,
    /// Our own account code's key (seals what we send).
    pub key: [u8; 32],
    /// Keys we try on what we receive (contacts, our account, codes being added).
    pub readable: Vec<[u8; 32]>,
}

/// Runs forever, calling `on_change` with the full peer list whenever it
/// changes. `me_now()` is asked again before every announcement.
pub async fn run(me: DeviceId, me_now: impl Fn() -> Me, quic_port: u16, open: bool, on_change: impl Fn(Vec<Peer>)) -> Result<()> {
    let sock = socket()?;
    let target = SocketAddr::V4(SocketAddrV4::new(GROUP, PORT));
    let mut peers: HashMap<DeviceId, (Peer, Instant)> = HashMap::new();
    let mut tick = tokio::time::interval(ANNOUNCE_EVERY);
    let mut buf = [0u8; 1500];
    let mut keys: Vec<[u8; 32]> = Vec::new();
    loop {
        let changed = tokio::select! {
            _ = tick.tick() => {
                let now = me_now();
                keys = now.readable;
                let announce = Announce { v: SEALED_VERSION, id: me.to_hex(), name: now.name, port: quic_port, attest: Some(now.attest) };
                let plain = serde_json::to_vec(&announce)?;
                if let Some(sealed) = seal(&now.key, &plain) {
                    let msg = Sealed { v: SEALED_VERSION, s: crate::identity::hex(&sealed) };
                    let _ = sock.send_to(&serde_json::to_vec(&msg)?, target).await;
                }
                if open {
                    let announce = Announce { v: OPEN_VERSION, ..announce };
                    let _ = sock.send_to(&serde_json::to_vec(&announce)?, target).await;
                }
                let before = peers.len();
                peers.retain(|_, (_, seen)| seen.elapsed() < EXPIRE_AFTER);
                peers.len() != before
            }
            r = sock.recv_from(&mut buf) => {
                let Ok((n, from)) = r else { continue };
                let a = if let Some(s) = serde_json::from_slice::<Sealed>(&buf[..n]).ok().filter(|s| s.v == SEALED_VERSION) {
                    let Some(data) = crate::identity::unhex(&s.s) else { continue };
                    let Some(plain) = keys.iter().find_map(|k| open_with(k, &data)) else { continue };
                    let Ok(a) = serde_json::from_slice::<Announce>(&plain) else { continue };
                    a
                } else if open {
                    let Ok(a) = serde_json::from_slice::<Announce>(&buf[..n]) else { continue };
                    if a.v != OPEN_VERSION {
                        continue;
                    }
                    a
                } else {
                    continue;
                };
                let Some(id) = DeviceId::from_hex(&a.id) else { continue };
                if id == me {
                    continue;
                }
                let addr = SocketAddr::new(from.ip(), a.port);
                let name: String = a.name.chars().take(64).collect();
                let account = a.attest.as_ref().and_then(|at| at.verify(id));
                let account_key = account.and(a.attest.as_ref()).and_then(|at| crate::identity::unhex(&at.account)).and_then(|k| k.try_into().ok());
                let peer = Peer { id, name, addr, account, account_key, alt: Vec::new(), remote: false, nat: None };
                let is_new = peers.get(&id).map(|(p, _)| *p != peer).unwrap_or(true);
                peers.insert(id, (peer, Instant::now()));
                is_new
            }
        };
        if changed {
            let mut list: Vec<Peer> = peers.values().map(|(p, _)| p.clone()).collect();
            list.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then(a.id.cmp(&b.id)));
            on_change(list);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_people_with_the_code_can_read_an_announcement() {
        let key = code_key("7KQ3-M2XA-9FTB");
        assert_eq!(key, code_key("7kq3 m2xa 9ftb"), "typed codes are forgiving");
        let sealed = seal(&key, b"hello").unwrap();
        assert_eq!(open_with(&key, &sealed).as_deref(), Some(&b"hello"[..]));
        assert!(open_with(&code_key("AAAA-BBBB-CCCC"), &sealed).is_none());
        assert_ne!(seal(&key, b"hello").unwrap(), sealed, "random nonce: announcements can't be linked");
    }
}
