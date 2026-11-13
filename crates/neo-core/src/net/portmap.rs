//! Router port mapping besides UPnP: PCP (RFC 6887) and its predecessor
//! NAT-PMP (RFC 6886). Both are tiny UDP requests to the default gateway on
//! port 5351. Many routers that ignore UPnP answer one of them. A mapping
//! makes this device directly reachable, no hole punching needed.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use anyhow::{anyhow, bail, Result};

pub const PORT: u16 = 5351;
const PROTO_UDP: u8 = 17;

/// The IPv4 default gateway (the router), if the OS can tell us.
pub fn default_gateway() -> Option<Ipv4Addr> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::NetworkManagement::IpHelper::{GetBestRoute, MIB_IPFORWARDROW};
        let mut row: MIB_IPFORWARDROW = unsafe { std::mem::zeroed() };
        let dest = u32::from_ne_bytes([8, 8, 8, 8]);
        if unsafe { GetBestRoute(dest, 0, &mut row) } != 0 {
            return None;
        }
        let hop = Ipv4Addr::from(row.dwForwardNextHop.to_ne_bytes());
        (!hop.is_unspecified()).then_some(hop)
    }
    #[cfg(not(windows))]
    {
        let table = std::fs::read_to_string("/proc/net/route").ok()?;
        table.lines().skip(1).find_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            (f.get(1) == Some(&"00000000")).then(|| u32::from_str_radix(f.get(2)?, 16).ok()).flatten()
        })
        .map(|g| Ipv4Addr::from(g.to_le_bytes()))
    }
}

// ------------------------------------------------------------------ PCP

/// A PCP MAP request for a UDP port (60 bytes).
pub fn pcp_map_request(client: Ipv4Addr, internal_port: u16, lifetime: u32, nonce: [u8; 12]) -> Vec<u8> {
    let mut p = Vec::with_capacity(60);
    p.push(2); // version
    p.push(1); // R=0, opcode MAP
    p.extend_from_slice(&[0, 0]);
    p.extend_from_slice(&lifetime.to_be_bytes());
    p.extend_from_slice(&client.to_ipv6_mapped().octets());
    p.extend_from_slice(&nonce);
    p.push(PROTO_UDP);
    p.extend_from_slice(&[0, 0, 0]);
    p.extend_from_slice(&internal_port.to_be_bytes());
    p.extend_from_slice(&internal_port.to_be_bytes()); // suggest the same external port
    p.extend_from_slice(&Ipv4Addr::UNSPECIFIED.to_ipv6_mapped().octets());
    p
}

/// What a router answered.
#[derive(Debug, PartialEq)]
pub enum Reply {
    Mapped { external: SocketAddr, lifetime: u32 },
    /// The router speaks only NAT-PMP (version 0) or an older PCP.
    WrongVersion,
    Error(u8),
}

pub fn parse_pcp_reply(data: &[u8], nonce: &[u8; 12]) -> Option<Reply> {
    if data.len() >= 4 && data[0] == 0 {
        return Some(Reply::WrongVersion); // a NAT-PMP-only router
    }
    if data.len() < 60 || data[0] != 2 || data[1] != 0x81 {
        return None;
    }
    let result = data[3];
    if result == 1 {
        return Some(Reply::WrongVersion);
    }
    if result != 0 {
        return Some(Reply::Error(result));
    }
    if &data[24..36] != nonce {
        return None;
    }
    let lifetime = u32::from_be_bytes(data[4..8].try_into().ok()?);
    let port = u16::from_be_bytes(data[42..44].try_into().ok()?);
    let ip6 = Ipv6Addr::from(<[u8; 16]>::try_from(&data[44..60]).ok()?);
    let ip: IpAddr = ip6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(ip6));
    Some(Reply::Mapped { external: SocketAddr::new(ip, port), lifetime })
}

// -------------------------------------------------------------- NAT-PMP

pub fn natpmp_address_request() -> [u8; 2] {
    [0, 0]
}

pub fn natpmp_map_request(internal_port: u16, lifetime: u32) -> [u8; 12] {
    let mut p = [0u8; 12];
    p[1] = 1; // map UDP
    p[4..6].copy_from_slice(&internal_port.to_be_bytes());
    p[6..8].copy_from_slice(&internal_port.to_be_bytes());
    p[8..12].copy_from_slice(&lifetime.to_be_bytes());
    p
}

pub fn parse_natpmp_address(data: &[u8]) -> Option<Result<Ipv4Addr, u16>> {
    if data.len() < 12 || data[0] != 0 || data[1] != 128 {
        return None;
    }
    let result = u16::from_be_bytes([data[2], data[3]]);
    Some(if result == 0 { Ok(Ipv4Addr::new(data[8], data[9], data[10], data[11])) } else { Err(result) })
}

/// (external port, lifetime) or the error code.
pub fn parse_natpmp_map(data: &[u8], internal_port: u16) -> Option<Result<(u16, u32), u16>> {
    if data.len() < 16 || data[0] != 0 || data[1] != 129 {
        return None;
    }
    let result = u16::from_be_bytes([data[2], data[3]]);
    if result != 0 {
        return Some(Err(result));
    }
    if u16::from_be_bytes([data[8], data[9]]) != internal_port {
        return None;
    }
    Some(Ok((u16::from_be_bytes([data[10], data[11]]), u32::from_be_bytes(data[12..16].try_into().ok()?))))
}

// -------------------------------------------------------------- exchange

/// Sends `request` and waits for a reply `parse` accepts, retransmitting
/// at 250 ms, 500 ms, 1 s (RFC 6886/6887 style), about 2 s in all.
async fn exchange<T>(gateway: SocketAddr, request: &[u8], mut parse: impl FnMut(&[u8]) -> Option<T>) -> Result<T> {
    let sock = tokio::net::UdpSocket::bind(if gateway.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" }).await?;
    sock.connect(gateway).await?;
    let mut buf = [0u8; 1100];
    for wait in [250u64, 500, 1000] {
        sock.send(request).await?;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(wait);
        while let Ok(Ok(n)) = tokio::time::timeout_at(deadline, sock.recv(&mut buf)).await {
            if let Some(t) = parse(&buf[..n]) {
                return Ok(t);
            }
        }
    }
    Err(anyhow!("no answer from the router"))
}

/// Asks for a UDP mapping with PCP. `client` is our LAN address facing the gateway.
pub async fn pcp_map(gateway: SocketAddr, client: Ipv4Addr, internal_port: u16, lifetime: u32) -> Result<Reply> {
    let mut nonce = [0u8; 12];
    let _ = ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut nonce);
    let req = pcp_map_request(client, internal_port, lifetime, nonce);
    exchange(gateway, &req, |d| parse_pcp_reply(d, &nonce)).await
}

/// Asks for a UDP mapping with NAT-PMP; returns the public address.
pub async fn natpmp_map(gateway: SocketAddr, internal_port: u16, lifetime: u32) -> Result<(SocketAddr, u32)> {
    let ip = match exchange(gateway, &natpmp_address_request(), parse_natpmp_address).await? {
        Ok(ip) => ip,
        Err(code) => bail!("router refused (NAT-PMP error {code})"),
    };
    match exchange(gateway, &natpmp_map_request(internal_port, lifetime), |d| parse_natpmp_map(d, internal_port)).await? {
        Ok((port, life)) => Ok((SocketAddr::new(ip.into(), port), life)),
        Err(code) => bail!("router refused (NAT-PMP error {code})"),
    }
}

/// Tries PCP, then NAT-PMP. Returns the public address that now forwards
/// to `internal_port`.
pub async fn map_port(gateway: SocketAddr, client: Ipv4Addr, internal_port: u16, lifetime: u32) -> Result<(SocketAddr, &'static str)> {
    match pcp_map(gateway, client, internal_port, lifetime).await {
        Ok(Reply::Mapped { external, .. }) if !external.ip().is_unspecified() => return Ok((external, "PCP")),
        Ok(Reply::Mapped { external, .. }) => {
            // PCP mapped the port but didn't say the address: ask NAT-PMP-style.
            if let Ok(Ok(ip)) = exchange(gateway, &natpmp_address_request(), parse_natpmp_address).await {
                return Ok((SocketAddr::new(ip.into(), external.port()), "PCP"));
            }
        }
        Ok(Reply::Error(code)) => crate::debug(&format!("PCP refused (code {code})")),
        Ok(Reply::WrongVersion) | Err(_) => {}
    }
    let (external, _) = natpmp_map(gateway, internal_port, lifetime).await?;
    Ok((external, "NAT-PMP"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcp_packets() {
        let nonce = [9u8; 12];
        let req = pcp_map_request("192.168.1.20".parse().unwrap(), 4000, 3600, nonce);
        assert_eq!(req.len(), 60);
        assert_eq!(&req[..2], &[2, 1]);
        assert_eq!(&req[24..36], &nonce);
        assert_eq!(req[36], 17);
        // Build the router's answer from the request.
        let mut resp = req.clone();
        resp[1] = 0x81;
        resp[3] = 0;
        resp[42..44].copy_from_slice(&50000u16.to_be_bytes());
        resp[44..60].copy_from_slice(&"203.0.113.5".parse::<Ipv4Addr>().unwrap().to_ipv6_mapped().octets());
        assert_eq!(
            parse_pcp_reply(&resp, &nonce),
            Some(Reply::Mapped { external: "203.0.113.5:50000".parse().unwrap(), lifetime: 3600 })
        );
        assert_eq!(parse_pcp_reply(&resp, &[0u8; 12]), None, "other nonce: not ours");
        assert_eq!(parse_pcp_reply(&[0, 128, 0, 1], &nonce), Some(Reply::WrongVersion));
    }

    #[test]
    fn natpmp_packets() {
        assert_eq!(natpmp_map_request(4000, 7200), [0, 1, 0, 0, 0x0f, 0xa0, 0x0f, 0xa0, 0, 0, 0x1c, 0x20]);
        let addr = [0, 128, 0, 0, 0, 0, 0, 1, 198, 51, 100, 7];
        assert_eq!(parse_natpmp_address(&addr), Some(Ok("198.51.100.7".parse().unwrap())));
        let map = [0, 129, 0, 0, 0, 0, 0, 1, 0x0f, 0xa0, 0xc3, 0x50, 0, 0, 0x0e, 0x10];
        assert_eq!(parse_natpmp_map(&map, 4000), Some(Ok((50000, 3600))));
        assert_eq!(parse_natpmp_map(&[0, 129, 0, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], 4000), Some(Err(3)));
    }

    /// A fake NAT-PMP-only router on loopback: PCP gets "wrong version",
    /// then NAT-PMP maps the port.
    #[tokio::test]
    async fn falls_back_from_pcp_to_natpmp() {
        let router = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let gateway = router.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 1100];
            loop {
                let (n, from) = router.recv_from(&mut buf).await.unwrap();
                let reply: Vec<u8> = match (buf[0], n) {
                    (2, _) => vec![0, 128 + buf[1], 0, 1, 0, 0, 0, 0], // PCP: unsupported version
                    (0, 2) => vec![0, 128, 0, 0, 0, 0, 0, 1, 198, 51, 100, 7],
                    (0, 12) => {
                        let mut r = vec![0, 129, 0, 0, 0, 0, 0, 1];
                        r.extend_from_slice(&buf[4..6]);
                        r.extend_from_slice(&41000u16.to_be_bytes());
                        r.extend_from_slice(&buf[8..12]);
                        r
                    }
                    _ => continue,
                };
                router.send_to(&reply, from).await.unwrap();
            }
        });
        let (external, via) = map_port(gateway, "192.168.1.20".parse().unwrap(), 4000, 3600).await.unwrap();
        assert_eq!((external, via), ("198.51.100.7:41000".parse().unwrap(), "NAT-PMP"));
    }
}
