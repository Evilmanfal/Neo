//! What kind of NAT is this device behind? Learned from STUN by asking
//! several servers, one after another, through Neo's own socket:
//!
//! - **Open**: the address servers see is our own (no NAT).
//! - **Easy** (endpoint-independent mapping): every server sees the same
//!   public port, so any contact can aim at it.
//! - **Hard** (symmetric, typical of carrier-grade NAT): each destination
//!   gets a different public port. If those ports go up in even steps
//!   (`step`), the next ones can be predicted; otherwise they're random.
//!
//! The result is published (encrypted) with presence so both sides of a
//! connection attempt can pick a punching strategy (`punch.rs`).

use std::net::{IpAddr, SocketAddr};

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NatKind {
    Open,
    Easy,
    Hard,
    #[default]
    Unknown,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NatInfo {
    #[serde(rename = "k", default)]
    pub kind: NatKind,
    /// Public IPv4 address (as seen by STUN).
    #[serde(rename = "i", default)]
    pub ip: Option<IpAddr>,
    /// Last public port observed.
    #[serde(rename = "p", default)]
    pub last_port: u16,
    /// For hard NATs: the observed increment between new mappings
    /// (0 = no usable pattern).
    #[serde(rename = "s", default)]
    pub step: i32,
}

impl NatInfo {
    pub fn is_hard(&self) -> bool {
        self.kind == NatKind::Hard
    }

    pub fn predictable(&self) -> bool {
        self.is_hard() && self.step != 0
    }
}

/// Largest port increment still treated as a sequence.
const MAX_STEP: i32 = 32;

/// Classifies STUN observations made in order through one socket.
pub fn classify(observed: &[SocketAddr], local_port: u16, local_ips: &[IpAddr]) -> NatInfo {
    let Some(last) = observed.last() else { return NatInfo::default() };
    let mut info = NatInfo { ip: Some(last.ip()), last_port: last.port(), ..Default::default() };
    if observed.len() < 2 {
        return info; // one answer: can't tell easy from hard
    }
    if observed.iter().all(|a| a.port() == last.port()) {
        info.kind = if local_ips.contains(&last.ip()) && last.port() == local_port { NatKind::Open } else { NatKind::Easy };
        return info;
    }
    info.kind = NatKind::Hard;
    let deltas: Vec<i32> = observed.windows(2).map(|w| w[1].port() as i32 - w[0].port() as i32).collect();
    let first = deltas[0];
    if first != 0 && first.abs() <= MAX_STEP && deltas.iter().all(|d| *d == first) {
        info.step = first;
    }
    info
}

/// Ports a hard NAT will probably hand out next, nearest first.
pub fn predicted_ports(info: &NatInfo, count: usize) -> Vec<u16> {
    if !info.predictable() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(count);
    let mut port = info.last_port as i32;
    while out.len() < count {
        port += info.step;
        if !(1024..=65535).contains(&port) {
            port = if info.step > 0 { 1024 } else { 65535 };
        }
        out.push(port as u16);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn tells_nat_types_apart() {
        let lan = ["192.168.1.5".parse().unwrap()];
        let easy = classify(&[a("203.0.113.9:40000"), a("203.0.113.9:40000"), a("203.0.113.9:40000")], 5000, &lan);
        assert_eq!(easy.kind, NatKind::Easy);
        let open = classify(&[a("192.168.1.5:5000"), a("192.168.1.5:5000")], 5000, &lan);
        assert_eq!(open.kind, NatKind::Open);
        let seq = classify(&[a("100.64.0.1:30000"), a("100.64.0.1:30002"), a("100.64.0.1:30004")], 5000, &lan);
        assert_eq!((seq.kind, seq.step, seq.last_port), (NatKind::Hard, 2, 30004));
        assert_eq!(predicted_ports(&seq, 3), vec![30006, 30008, 30010]);
        let random = classify(&[a("100.64.0.1:30000"), a("100.64.0.1:51234"), a("100.64.0.1:12011")], 5000, &lan);
        assert_eq!((random.kind, random.step), (NatKind::Hard, 0));
        assert!(predicted_ports(&random, 5).is_empty());
        assert_eq!(classify(&[a("203.0.113.9:1")], 5000, &lan).kind, NatKind::Unknown);
    }
}
