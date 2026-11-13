//! LAN discovery over UDP multicast. Every instance announces
//! `{device id, name, port, account attestation}` every couple of seconds and
//! listens for others. Several instances on one machine share the port via
//! SO_REUSEADDR. Announcements are unauthenticated hints: the attestation is
//! checked here, and TLS proves the device id when we actually connect.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::{Duration, Instant};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;

use crate::identity::{Attest, DeviceId};
use crate::Peer;

const GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 77, 7);
const PORT: u16 = 47321;
const VERSION: u8 = 3;
const ANNOUNCE_EVERY: Duration = Duration::from_secs(2);
const EXPIRE_AFTER: Duration = Duration::from_secs(7);

#[derive(Serialize, Deserialize)]
struct Announce {
    v: u8,
    id: String,
    name: String,
    port: u16,
    attest: Option<Attest>,
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

/// Runs forever, calling `on_change` with the full peer list whenever it
/// changes. `me_now()` returns this device's current name and attestation.
pub async fn run(
    me: DeviceId,
    me_now: impl Fn() -> (String, Attest),
    quic_port: u16,
    on_change: impl Fn(Vec<Peer>),
) -> Result<()> {
    let sock = socket()?;
    let target = SocketAddr::V4(SocketAddrV4::new(GROUP, PORT));
    let mut peers: HashMap<DeviceId, (Peer, Instant)> = HashMap::new();
    let mut tick = tokio::time::interval(ANNOUNCE_EVERY);
    let mut buf = [0u8; 1500];
    loop {
        let changed = tokio::select! {
            _ = tick.tick() => {
                let (name, attest) = me_now();
                let announce = Announce { v: VERSION, id: me.to_hex(), name, port: quic_port, attest: Some(attest) };
                let _ = sock.send_to(&serde_json::to_vec(&announce)?, target).await;
                let before = peers.len();
                peers.retain(|_, (_, seen)| seen.elapsed() < EXPIRE_AFTER);
                peers.len() != before
            }
            r = sock.recv_from(&mut buf) => {
                let Ok((n, from)) = r else { continue };
                let Ok(a) = serde_json::from_slice::<Announce>(&buf[..n]) else { continue };
                let Some(id) = DeviceId::from_hex(&a.id) else { continue };
                if a.v != VERSION || id == me {
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
