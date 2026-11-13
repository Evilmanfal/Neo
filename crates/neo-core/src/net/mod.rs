//! Internet reachability with no server, VPN or port forwarding:
//!
//! - **IPv6**: global addresses are directly reachable on most connections.
//! - **Router port mapping** (UPnP IGD): asked for automatically; when the
//!   router allows it, contacts can connect straight in.
//! - **STUN**: learns the public IPv4 address/port of our socket from public
//!   STUN servers, for UDP hole punching with contacts (see `socket::POKE`).
//! - **LAN addresses**: also published, for networks where multicast
//!   discovery is blocked.
//!
//! The result is a list of candidate addresses that `presence` publishes.

pub mod nat;
pub mod portmap;
pub mod presence;
pub mod punch;
pub mod socket;

use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use socket::NeoSocket;

const STUN_SERVERS: &[&str] =
    &["stun.cloudflare.com:3478", "stun.l.google.com:19302", "stun1.l.google.com:19302", "stun2.l.google.com:19302"];
const MAPPING_LEASE: u32 = 3600;

/// What we know about how to reach this device.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Reachability {
    /// Public address the router forwards to us (UPnP), if any.
    pub mapped: Option<SocketAddr>,
    /// Public address seen by STUN servers (needs hole punching).
    pub public: Option<SocketAddr>,
    /// Global IPv6 addresses.
    pub ipv6: Vec<SocketAddr>,
    /// Private LAN addresses.
    pub lan: Vec<SocketAddr>,
    /// The DHT is reachable (presence can be published/looked up).
    pub dht: bool,
    /// NAT type learned from STUN (easy / hard / predictable).
    pub nat: nat::NatInfo,
    /// How `mapped` was obtained ("UPnP", "PCP", "NAT-PMP").
    pub mapped_via: Option<&'static str>,
}

impl Reachability {
    /// Candidate addresses, best first.
    pub fn candidates(&self, include_lan: bool) -> Vec<SocketAddr> {
        let mut out = Vec::new();
        out.extend(self.mapped);
        out.extend(self.ipv6.iter().copied());
        out.extend(self.public.filter(|p| Some(*p) != self.mapped));
        if include_lan {
            out.extend(self.lan.iter().copied());
        }
        out.truncate(6);
        out
    }

    /// Others can connect in without any help.
    pub fn directly_reachable(&self) -> bool {
        self.mapped.is_some() || !self.ipv6.is_empty()
    }
}

pub struct Reach {
    pub socket: Arc<NeoSocket>,
    pub state: Mutex<Reachability>,
}

impl Reach {
    pub fn new(socket: Arc<NeoSocket>) -> Arc<Self> {
        Arc::new(Reach { socket, state: Mutex::new(Reachability::default()) })
    }

    pub fn get(&self) -> Reachability {
        self.state.lock().unwrap().clone()
    }

    /// Applies a change; returns true if anything is different.
    fn update(&self, f: impl FnOnce(&mut Reachability)) -> bool {
        let mut s = self.state.lock().unwrap();
        let before = s.clone();
        f(&mut s);
        *s != before
    }

    pub fn set_dht(&self, ok: bool) -> bool {
        self.update(|s| s.dht = ok)
    }

    /// Re-reads interface addresses (IPv6 + LAN).
    pub fn refresh_interfaces(&self) -> bool {
        let port = self.socket.port();
        let mut ipv6 = Vec::new();
        let mut lan = Vec::new();
        for iface in if_addrs::get_if_addrs().unwrap_or_default() {
            if iface.is_loopback() {
                continue;
            }
            match iface.ip() {
                IpAddr::V6(v6) if is_global_v6(&v6) => ipv6.push(SocketAddr::new(v6.into(), port)),
                IpAddr::V4(v4) if v4.is_private() => lan.push(SocketAddr::new(v4.into(), port)),
                _ => {}
            }
        }
        ipv6.sort();
        ipv6.dedup();
        ipv6.truncate(2);
        lan.sort();
        lan.dedup();
        lan.truncate(2);
        self.update(|s| {
            s.ipv6 = ipv6;
            s.lan = lan;
        })
    }

    /// Asks several public STUN servers, one after another through our
    /// socket, for our public IPv4 address/port, and classifies the NAT from
    /// how the answers differ (`nat.rs`).
    pub async fn refresh_stun(&self) -> bool {
        let mut servers: Vec<SocketAddr> = Vec::new();
        for server in STUN_SERVERS {
            if let Ok(addrs) = tokio::net::lookup_host(server).await {
                if let Some(addr) = addrs.filter(|a| a.is_ipv4()).find(|a| !servers.iter().any(|s| s.ip() == a.ip())) {
                    servers.push(addr);
                }
            }
        }
        let mut observed = Vec::new();
        for server in servers.iter().take(3) {
            let id = self.socket.stun_request(*server);
            for _ in 0..12 {
                tokio::time::sleep(Duration::from_millis(80)).await;
                if let Some(a) = self.socket.stun_result(&id) {
                    observed.push(socket::canonical(a));
                    break;
                }
            }
        }
        let local_ips: Vec<IpAddr> = if_addrs::get_if_addrs().unwrap_or_default().into_iter().map(|i| i.ip()).collect();
        let info = nat::classify(&observed, self.socket.port(), &local_ips);
        crate::debug(&format!("STUN saw {observed:?}: {info:?}"));
        let public = observed.last().copied();
        self.update(|s| {
            s.public = public;
            s.nat = info;
        })
    }

    /// Asks the router (UPnP IGD) to forward our UDP port. Returns true if
    /// the result changed. Safe to call again to renew the lease.
    pub async fn refresh_mapping(&self) -> bool {
        let (mapped, via) = match self.try_mapping().await {
            Ok((a, via)) => (Some(a), Some(via)),
            Err(e) => {
                crate::debug(&format!("router port mapping unavailable: {e:#}"));
                (None, None)
            }
        };
        self.update(|s| {
            s.mapped = mapped;
            s.mapped_via = via;
        })
    }

    /// UPnP first, then PCP, then NAT-PMP.
    async fn try_mapping(&self) -> anyhow::Result<(SocketAddr, &'static str)> {
        let upnp = match self.try_upnp().await {
            Ok(a) => return Ok((a, "UPnP")),
            Err(e) => e,
        };
        let gateway = portmap::default_gateway().ok_or_else(|| anyhow::anyhow!("UPnP: {upnp:#}; no default gateway"))?;
        let probe = std::net::UdpSocket::bind("0.0.0.0:0")?;
        probe.connect((gateway, portmap::PORT))?;
        let IpAddr::V4(local_ip) = probe.local_addr()?.ip() else { anyhow::bail!("no IPv4 route to the gateway") };
        let (external, via) = portmap::map_port(SocketAddr::new(gateway.into(), portmap::PORT), local_ip, self.socket.port(), MAPPING_LEASE)
            .await
            .map_err(|e| anyhow::anyhow!("UPnP: {upnp:#}; PCP/NAT-PMP: {e:#}"))?;
        check_public(external.ip())?;
        Ok((external, via))
    }

    async fn try_upnp(&self) -> anyhow::Result<SocketAddr> {
        use igd_next::{aio::tokio::search_gateway, PortMappingProtocol, SearchOptions};
        let mut options = SearchOptions::default();
        options.timeout = Some(Duration::from_secs(3));
        let gateway = search_gateway(options).await?;
        // Which local address faces the gateway?
        let probe = std::net::UdpSocket::bind("0.0.0.0:0")?;
        probe.connect(gateway.addr)?;
        let IpAddr::V4(local_ip) = probe.local_addr()?.ip() else { anyhow::bail!("no IPv4 route to the gateway") };
        let port = self.socket.port();
        let local = SocketAddr::V4(SocketAddrV4::new(local_ip, port));
        let external_port = match gateway.add_port(PortMappingProtocol::UDP, port, local, MAPPING_LEASE, "Neo").await {
            Ok(()) => port,
            Err(_) => gateway.add_any_port(PortMappingProtocol::UDP, local, MAPPING_LEASE, "Neo").await?,
        };
        let external = gateway.get_external_ip().await?;
        check_public(external)?;
        Ok(SocketAddr::new(external, external_port))
    }
}

/// A router's "external" address must really be public: behind another NAT
/// (e.g. carrier-grade), it isn't reachable from the internet.
fn check_public(ip: IpAddr) -> anyhow::Result<()> {
    if let IpAddr::V4(v4) = ip {
        let cgnat = v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 64;
        if v4.is_private() || cgnat || v4 == Ipv4Addr::UNSPECIFIED {
            anyhow::bail!("router is behind another NAT");
        }
    }
    Ok(())
}

fn is_global_v6(ip: &std::net::Ipv6Addr) -> bool {
    let s = ip.segments();
    // 2000::/3 global unicast, excluding documentation 2001:db8::/32.
    (s[0] & 0xe000) == 0x2000 && !(s[0] == 0x2001 && s[1] == 0x0db8)
}
