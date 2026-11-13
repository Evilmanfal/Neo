//! The one UDP socket Neo uses for everything internet-facing.
//!
//! It is dual-stack (IPv6 + IPv4) and shared between:
//! - QUIC (all transfers, pairing, linking);
//! - STUN requests to public servers, to learn our public IPv4 address and
//!   port *as seen through the NAT for this very socket*;
//! - tiny "poke" packets sent to contacts' addresses, which keep a path open
//!   through both NATs so a contact can connect without port forwarding
//!   (UDP hole punching).
//!
//! STUN replies and pokes are filtered out before quinn sees them.

use std::fmt;
use std::io::{self, IoSliceMut};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6, UdpSocket};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use quinn::udp::{RecvMeta, Transmit};
use quinn::{AsyncUdpSocket, Runtime, UdpPoller};

pub const POKE: &[u8] = b"NEOPOKE1";
const STUN_MAGIC: u32 = 0x2112_A442;

pub struct NeoSocket {
    inner: Arc<dyn AsyncUdpSocket>,
    raw: UdpSocket,
    /// (transaction id, public address it reported)
    stun: Mutex<Vec<([u8; 12], Option<SocketAddr>)>>,
    /// Punch sessions in progress: token -> where to report a probe that got through.
    punch: Mutex<Vec<([u8; 8], tokio::sync::mpsc::UnboundedSender<SocketAddr>)>>,
}

impl fmt::Debug for NeoSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NeoSocket").finish_non_exhaustive()
    }
}

/// Kernel send/receive buffer for the QUIC socket.
const SOCKET_BUFFER: usize = 8 << 20;

impl NeoSocket {
    /// Binds `[::]:port` accepting IPv4 too (or `0.0.0.0` if IPv6 is unavailable).
    pub fn bind(runtime: &dyn Runtime, port: u16) -> io::Result<Arc<Self>> {
        use socket2::{Domain, Protocol, Socket, Type};
        let sock = match Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP)) {
            Ok(s) if s.set_only_v6(false).is_ok()
                && s.bind(&SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, port, 0, 0)).into()).is_ok() =>
            {
                s
            }
            _ => {
                let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
                s.bind(&SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port)).into())?;
                s
            }
        };
        // Windows gives UDP sockets 64 KB buffers: under a millisecond of a
        // gigabit transfer, so any scheduling hiccup drops packets and QUIC
        // backs off. 8 MB rides out bursts (memory is only used while
        // packets are actually queued).
        let _ = sock.set_recv_buffer_size(SOCKET_BUFFER);
        let _ = sock.set_send_buffer_size(SOCKET_BUFFER);
        let std_sock: UdpSocket = sock.into();
        let raw = std_sock.try_clone()?;
        let inner = runtime.wrap_udp_socket(std_sock)?;
        Ok(Arc::new(NeoSocket { inner, raw, stun: Mutex::new(Vec::new()), punch: Mutex::new(Vec::new()) }))
    }

    pub fn port(&self) -> u16 {
        self.inner.local_addr().map(|a| a.port()).unwrap_or(0)
    }

    fn target(&self, to: SocketAddr) -> SocketAddr {
        // A dual-stack socket needs IPv4 destinations written as v4-mapped.
        match (self.inner.local_addr(), to) {
            (Ok(SocketAddr::V6(_)), SocketAddr::V4(v4)) => {
                SocketAddr::V6(SocketAddrV6::new(v4.ip().to_ipv6_mapped(), v4.port(), 0, 0))
            }
            _ => to,
        }
    }

    /// Sends a keep-the-path-open packet (best effort).
    pub fn poke(&self, to: SocketAddr) {
        let _ = self.raw.send_to(POKE, self.target(to));
    }

    /// Sends a STUN Binding Request; the answer shows up in `stun_result`.
    pub fn stun_request(&self, server: SocketAddr) -> [u8; 12] {
        let mut id = [0u8; 12];
        let _ = ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut id);
        let mut req = Vec::with_capacity(20);
        req.extend_from_slice(&0x0001u16.to_be_bytes()); // Binding Request
        req.extend_from_slice(&0u16.to_be_bytes()); // no attributes
        req.extend_from_slice(&STUN_MAGIC.to_be_bytes());
        req.extend_from_slice(&id);
        {
            let mut stun = self.stun.lock().unwrap();
            if stun.len() > 16 {
                stun.remove(0);
            }
            stun.push((id, None));
        }
        let _ = self.raw.send_to(&req, self.target(server));
        id
    }

    pub fn stun_result(&self, id: &[u8; 12]) -> Option<SocketAddr> {
        self.stun.lock().unwrap().iter().find(|(t, _)| t == id).and_then(|(_, a)| *a)
    }

    /// Sends any small packet from this socket (punch probes).
    pub fn send_raw(&self, data: &[u8], to: SocketAddr) {
        let _ = self.raw.send_to(data, self.target(to));
    }

    /// Reports (on the returned channel) every address a punch probe with
    /// this token arrives from; such probes are also answered right away.
    pub fn watch_punch(&self, token: [u8; 8]) -> tokio::sync::mpsc::UnboundedReceiver<SocketAddr> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.punch.lock().unwrap().push((token, tx));
        rx
    }

    pub fn unwatch_punch(&self, token: &[u8; 8]) {
        self.punch.lock().unwrap().retain(|(t, _)| t != token);
    }

    /// Consumes packets that aren't QUIC. Returns true if `data` was ours.
    fn intercept(&self, data: &[u8], from: SocketAddr) -> bool {
        if data == POKE {
            return true;
        }
        if let Some(token) = super::punch::parse(data) {
            let watchers = self.punch.lock().unwrap();
            if let Some((_, tx)) = watchers.iter().find(|(t, _)| *t == token) {
                let _ = self.raw.send_to(data, from); // answer: opens our side too
                let _ = tx.send(canonical(from));
            }
            return true;
        }
        if let Some((id, addr)) = parse_stun_response(data) {
            let mut stun = self.stun.lock().unwrap();
            if let Some(slot) = stun.iter_mut().find(|(t, _)| *t == id) {
                slot.1 = Some(addr);
            }
            return true;
        }
        false
    }
}

/// Parses a STUN Binding Success Response's XOR-MAPPED-ADDRESS (or MAPPED-ADDRESS).
pub fn parse_stun_response(data: &[u8]) -> Option<([u8; 12], SocketAddr)> {
    if data.len() < 20 || data[0] & 0xC0 != 0 {
        return None;
    }
    let msg_type = u16::from_be_bytes([data[0], data[1]]);
    let len = u16::from_be_bytes([data[2], data[3]]) as usize;
    if msg_type != 0x0101 || u32::from_be_bytes(data[4..8].try_into().ok()?) != STUN_MAGIC || data.len() < 20 + len {
        return None;
    }
    let id: [u8; 12] = data[8..20].try_into().ok()?;
    let mut pos = 20;
    let mut fallback = None;
    while pos + 4 <= 20 + len {
        let attr = u16::from_be_bytes([data[pos], data[pos + 1]]);
        let alen = u16::from_be_bytes([data[pos + 2], data[pos + 3]]) as usize;
        let v = data.get(pos + 4..pos + 4 + alen)?;
        let xor = attr == 0x0020;
        if (xor || attr == 0x0001) && v.len() >= 8 {
            let mut port = u16::from_be_bytes([v[2], v[3]]);
            if xor {
                port ^= (STUN_MAGIC >> 16) as u16;
            }
            let addr = match v[1] {
                0x01 => {
                    let mut ip: [u8; 4] = v[4..8].try_into().ok()?;
                    if xor {
                        for (b, m) in ip.iter_mut().zip(STUN_MAGIC.to_be_bytes()) {
                            *b ^= m;
                        }
                    }
                    SocketAddr::new(Ipv4Addr::from(ip).into(), port)
                }
                0x02 if v.len() >= 20 => {
                    let mut ip: [u8; 16] = v[4..20].try_into().ok()?;
                    if xor {
                        let mut mask = STUN_MAGIC.to_be_bytes().to_vec();
                        mask.extend_from_slice(&id);
                        for (b, m) in ip.iter_mut().zip(mask) {
                            *b ^= m;
                        }
                    }
                    SocketAddr::new(Ipv6Addr::from(ip).into(), port)
                }
                _ => return None,
            };
            if xor {
                return Some((id, addr));
            }
            fallback = Some(addr);
        }
        pos += 4 + ((alen + 3) & !3);
    }
    fallback.map(|a| (id, a))
}

impl AsyncUdpSocket for NeoSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        self.inner.clone().create_io_poller()
    }

    fn try_send(&self, transmit: &Transmit) -> io::Result<()> {
        self.inner.try_send(transmit)
    }

    fn poll_recv(&self, cx: &mut Context, bufs: &mut [IoSliceMut<'_>], meta: &mut [RecvMeta]) -> Poll<io::Result<usize>> {
        loop {
            let n = match self.inner.poll_recv(cx, bufs, meta) {
                Poll::Ready(Ok(n)) => n,
                other => return other,
            };
            // Drop our own packets, compacting the rest to the front.
            let mut kept = 0;
            for i in 0..n {
                let single = meta[i].len <= meta[i].stride;
                let ours = single && self.intercept(&bufs[i][..meta[i].len], meta[i].addr);
                if ours {
                    continue;
                }
                if kept != i {
                    let len = meta[i].len;
                    let (left, right) = bufs.split_at_mut(i);
                    left[kept][..len].copy_from_slice(&right[0][..len]);
                    meta[kept] = meta[i];
                }
                kept += 1;
            }
            if kept > 0 {
                return Poll::Ready(Ok(kept));
            }
            // Everything was ours: poll again (registers the waker if empty).
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    fn max_transmit_segments(&self) -> usize {
        self.inner.max_transmit_segments()
    }

    fn max_receive_segments(&self) -> usize {
        self.inner.max_receive_segments()
    }

    fn may_fragment(&self) -> bool {
        self.inner.may_fragment()
    }
}

/// Undoes v4-mapped IPv6 addresses (`::ffff:1.2.3.4`) so addresses compare
/// and display naturally.
pub fn canonical(addr: SocketAddr) -> SocketAddr {
    match addr {
        SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
            Some(v4) => SocketAddr::new(v4.into(), v6.port()),
            None => addr,
        },
        v4 => v4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_xor_mapped_address() {
        // Binding success: XOR-MAPPED-ADDRESS 203.0.113.7:51000
        let id = [7u8; 12];
        let mut m = vec![0x01, 0x01, 0x00, 0x0c];
        m.extend_from_slice(&STUN_MAGIC.to_be_bytes());
        m.extend_from_slice(&id);
        m.extend_from_slice(&[0x00, 0x20, 0x00, 0x08, 0x00, 0x01]);
        m.extend_from_slice(&(51000u16 ^ 0x2112).to_be_bytes());
        for (b, k) in [203u8, 0, 113, 7].iter().zip(STUN_MAGIC.to_be_bytes()) {
            m.push(b ^ k);
        }
        let (got_id, addr) = parse_stun_response(&m).unwrap();
        assert_eq!(got_id, id);
        assert_eq!(addr, "203.0.113.7:51000".parse().unwrap());
        assert!(parse_stun_response(POKE).is_none());
    }
}
