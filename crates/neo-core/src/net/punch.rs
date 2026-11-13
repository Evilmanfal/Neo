//! Getting through strict routers without any relay.
//!
//! When a direct connection fails, both devices send small "punch" probes
//! (`NEOPUNCH` + an 8-byte token shared through the DHT knock) chosen from
//! both sides' NAT types (`nat.rs`):
//!
//! - **Easy/open peer:** aim at its known public address (plain hole punching).
//! - **Hard peer with predictable ports:** spray the ports its NAT will
//!   probably assign next (`last_port + step * k`).
//! - **Hard peer with random ports:** spray random ports on its IP, while the
//!   hard side opens 256 extra sockets toward us (the "birthday" technique),
//!   so each random probe has a 256/64512 chance of landing on an open
//!   mapping. Thousands of probes make that near-certain.
//! - **Hard on both sides with random ports:** attempted, bounded, but rarely
//!   works; the user then gets a clear message instead of a relay.
//!
//! The first probe that gets through, on any socket, is answered and
//! reported. QUIC is then brought up over exactly that socket and address
//! pair (`Found`). Everything is bounded: about 200 probes/s on the main
//! socket and 1 probe every 2 s per extra socket. Extra sockets open only
//! when the NAT types call for them (after 3 s), and all of them close as
//! soon as the session ends.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use super::nat::{predicted_ports, NatInfo};
use super::socket::NeoSocket;

pub const MAGIC: &[u8; 8] = b"NEOPUNCH";
pub const BIRTHDAY_SOCKETS: usize = 256;
/// Probes sprayed from the main socket at most this often.
const SPRAY_GAP_MS: u64 = 5;
const BASE_EVERY_MS: u64 = 500;
const EXTRA_START_MS: u64 = 3000;
const EXTRA_EVERY_MS: u64 = 2000;
const SPRAY_PORTS: usize = 4096;

pub fn packet(token: &[u8; 8]) -> [u8; 16] {
    let mut p = [0u8; 16];
    p[..8].copy_from_slice(MAGIC);
    p[8..].copy_from_slice(token);
    p
}

pub fn parse(data: &[u8]) -> Option<[u8; 8]> {
    (data.len() == 16 && &data[..8] == MAGIC).then(|| data[8..].try_into().unwrap())
}

/// What to send where during a punch session.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Plan {
    /// Known addresses of the peer, retried every 500 ms.
    pub base: Vec<SocketAddr>,
    /// Guessed addresses, sprayed from the main socket.
    pub spray: Vec<SocketAddr>,
    /// Extra local sockets to open (0 or `BIRTHDAY_SOCKETS`).
    pub extras: usize,
    /// Where each extra socket sends (cycled by socket index).
    pub extra_targets: Vec<SocketAddr>,
}

fn is_public_v4(a: &SocketAddr) -> bool {
    match a.ip() {
        IpAddr::V4(v4) => !(v4.is_private() || v4.is_loopback() || v4.is_link_local()),
        _ => false,
    }
}

/// Tiny deterministic generator (xorshift) for port guesses.
struct Rng(u64);
impl Rng {
    fn port(&mut self) -> u16 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        1024 + (self.0 % (65536 - 1024)) as u16
    }
}

/// Picks a strategy from both NAT types. `peer_addrs` are its published
/// candidates; `seed` varies the random guesses.
pub fn plan(me: &NatInfo, peer: &NatInfo, peer_addrs: &[SocketAddr], seed: u64) -> Plan {
    let mut plan = Plan::default();
    for a in peer_addrs {
        if !plan.base.contains(a) {
            plan.base.push(*a);
        }
    }
    let peer_ip = peer.ip.or_else(|| peer_addrs.iter().find(|a| is_public_v4(a)).map(|a| a.ip()));
    if peer.is_hard() {
        // Its published public port only ever worked toward the STUN
        // servers; probing it just wastes a mapping on our side and shifts
        // the port sequence both sides are predicting.
        plan.base.retain(|a| !is_public_v4(a));
    }
    let Some(peer_ip) = peer_ip else { return plan };
    let mut rng = Rng(seed | 1);

    if peer.predictable() {
        plan.spray = predicted_ports(peer, 256).into_iter().map(|p| SocketAddr::new(peer_ip, p)).collect();
    } else if peer.is_hard() {
        // The peer will open birthday sockets toward us; find one of them.
        plan.spray = (0..SPRAY_PORTS).map(|_| SocketAddr::new(peer_ip, rng.port())).collect();
    }

    if me.is_hard() && !me.predictable() {
        // Our mappings are random: open many, so the peer's random probes
        // can find one. Aim them at the peer's stable address if it has one,
        // else at random ports too (hard on both sides).
        plan.extras = BIRTHDAY_SOCKETS;
        let stable = (!peer.is_hard()).then(|| peer_addrs.iter().find(|a| is_public_v4(a)).copied()).flatten();
        plan.extra_targets = match stable {
            Some(a) => vec![a],
            None if peer.predictable() => plan.spray.clone(),
            None => (0..BIRTHDAY_SOCKETS).map(|_| SocketAddr::new(peer_ip, rng.port())).collect(),
        };
    }
    plan
}

/// Which local socket sends a probe.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum From {
    Main,
    Extra(usize),
}

/// The plan as a timeline (milliseconds from start), shared by the real
/// runner and the NAT simulator in tests.
pub fn schedule(plan: &Plan, duration_ms: u64) -> Vec<(u64, From, SocketAddr)> {
    let mut out = Vec::new();
    let mut t = 0;
    while t < duration_ms {
        for a in &plan.base {
            out.push((t, From::Main, *a));
        }
        t += BASE_EVERY_MS;
    }
    if !plan.spray.is_empty() {
        let mut t = 0;
        let mut i = 0;
        while t < duration_ms {
            out.push((t, From::Main, plan.spray[i % plan.spray.len()]));
            i += 1;
            t += SPRAY_GAP_MS;
        }
    }
    if plan.extras > 0 && !plan.extra_targets.is_empty() {
        for s in 0..plan.extras {
            let mut t = EXTRA_START_MS + s as u64 * 4;
            let target = plan.extra_targets[s % plan.extra_targets.len()];
            while t < duration_ms {
                out.push((t, From::Extra(s), target));
                t += EXTRA_EVERY_MS;
            }
        }
    }
    out.sort_by_key(|e| e.0);
    out
}

/// A working path: which local socket, and the peer address it reached.
#[derive(Debug)]
pub enum Found {
    Main(SocketAddr),
    Extra(std::net::UdpSocket, SocketAddr),
}

/// Runs a punch session for up to `duration`. Returns the first working
/// path, or None. All extra sockets except a winning one are closed.
pub async fn run(main: &Arc<NeoSocket>, token: [u8; 8], plan: &Plan, duration: Duration) -> Option<Found> {
    let probe = packet(&token);
    let mut main_hits = main.watch_punch(token);
    let (hit_tx, mut hit_rx) = tokio::sync::mpsc::unbounded_channel::<(usize, SocketAddr)>();

    // Extra sockets (birthday), only if the plan wants them.
    let mut extras: Vec<Option<std::net::UdpSocket>> = Vec::new();
    let mut tasks = tokio::task::JoinSet::new();
    let mut senders: Vec<Option<Arc<tokio::net::UdpSocket>>> = Vec::new();
    for i in 0..plan.extras {
        let Ok(std_sock) = std::net::UdpSocket::bind("0.0.0.0:0") else { break };
        let _ = std_sock.set_nonblocking(true);
        let Ok(clone) = std_sock.try_clone() else { break };
        let Ok(tok) = tokio::net::UdpSocket::from_std(clone) else { break };
        let tok = Arc::new(tok);
        let (rx_sock, tx) = (tok.clone(), hit_tx.clone());
        tasks.spawn(async move {
            let mut buf = [0u8; 64];
            while let Ok((n, from)) = rx_sock.recv_from(&mut buf).await {
                if parse(&buf[..n]) == Some(token) {
                    let _ = rx_sock.send_to(&probe, from).await;
                    let _ = tx.send((i, from));
                    break;
                }
            }
        });
        extras.push(Some(std_sock));
        senders.push(Some(tok));
    }

    let events = schedule(plan, duration.as_millis() as u64);
    let start = tokio::time::Instant::now();
    let deadline = start + duration;
    let mut next = 0;
    let found = loop {
        let wake = events.get(next).map(|e| start + Duration::from_millis(e.0)).unwrap_or(deadline).min(deadline);
        tokio::select! {
            Some(addr) = main_hits.recv() => break Some(Found::Main(addr)),
            Some((i, addr)) = hit_rx.recv() => {
                break extras.get_mut(i).and_then(Option::take).map(|s| Found::Extra(s, addr));
            }
            _ = tokio::time::sleep_until(wake) => {
                if tokio::time::Instant::now() >= deadline {
                    break None;
                }
                // Send everything that's due (in small bursts).
                while let Some(&(at, from, to)) = events.get(next) {
                    if start + Duration::from_millis(at) > tokio::time::Instant::now() {
                        break;
                    }
                    next += 1;
                    match from {
                        From::Main => main.send_raw(&probe, to),
                        From::Extra(i) => {
                            if let Some(Some(s)) = senders.get(i) {
                                let _ = s.try_send_to(&probe, to);
                            }
                        }
                    }
                }
            }
        }
    };
    tasks.abort_all();
    main.unwatch_punch(&token);
    drop(senders);
    drop(extras); // closes every other extra socket
    found
}

#[cfg(test)]
mod sim {
    //! A model of NATs (mapping + port-restricted filtering) driven by the
    //! same `plan`/`schedule` as the real runner.

    use std::collections::{HashMap, HashSet};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    use super::*;
    use crate::net::nat::NatKind;

    #[derive(Clone, Copy)]
    enum Mapping {
        Easy,
        Sequential(i32),
        Random,
    }

    struct Nat {
        ip: IpAddr,
        kind: Mapping,
        next: i32,
        rng: Rng,
        /// (local socket, destination) -> public port (easy NATs ignore the destination)
        map: HashMap<(From, Option<SocketAddr>), u16>,
        /// (public port, remote address) pairs allowed back in
        allow: HashSet<(u16, SocketAddr)>,
    }

    impl Nat {
        fn new(ip: [u8; 4], kind: Mapping, seed: u64) -> Self {
            Nat { ip: IpAddr::V4(Ipv4Addr::from(ip)), kind, next: 20000, rng: Rng(seed), map: HashMap::new(), allow: HashSet::new() }
        }

        /// Outbound packet: returns the public source address.
        fn out(&mut self, from: From, to: SocketAddr) -> SocketAddr {
            let key = match self.kind {
                Mapping::Easy => (from, None),
                _ => (from, Some(to)),
            };
            let kind = self.kind;
            let port = match self.map.get(&key) {
                Some(p) => *p,
                None => {
                    let p = match kind {
                        Mapping::Easy => {
                            self.next += 1;
                            self.next as u16
                        }
                        Mapping::Sequential(step) => {
                            self.next += step;
                            self.next as u16
                        }
                        Mapping::Random => self.rng.port(),
                    };
                    self.map.insert(key, p);
                    p
                }
            };
            self.allow.insert((port, to));
            SocketAddr::new(self.ip, port)
        }

        fn lets_in(&self, port: u16, from: SocketAddr) -> bool {
            self.allow.contains(&(port, from))
        }

        /// What STUN would report: three servers, in order, from the main socket.
        fn stun(&mut self) -> NatInfo {
            let obs: Vec<SocketAddr> =
                (1..=3u8).map(|i| self.out(From::Main, SocketAddr::new(IpAddr::V4(Ipv4Addr::new(198, 51, 100, i)), 3478))).collect();
            crate::net::nat::classify(&obs, 1, &[])
        }
    }

    /// Runs both sides' schedules through the NAT models; returns the ms of
    /// the first probe that got through, if any.
    fn attempt(a_kind: Mapping, b_kind: Mapping, seed: u64) -> Option<u64> {
        let mut a = Nat::new([203, 0, 113, 1], a_kind, seed ^ 0xa5a5);
        let mut b = Nat::new([198, 18, 0, 1], b_kind, seed ^ 0x5a5a);
        let (a_info, b_info) = (a.stun(), b.stun());
        let addr = |n: &Nat, i: &NatInfo| vec![SocketAddr::new(n.ip, i.last_port)];
        let a_plan = plan(&a_info, &b_info, &addr(&b, &b_info), seed);
        let b_plan = plan(&b_info, &a_info, &addr(&a, &a_info), seed.wrapping_mul(7));
        let mut events: Vec<(u64, bool, From, SocketAddr)> = schedule(&a_plan, 20_000)
            .into_iter()
            .map(|(t, f, to)| (t, true, f, to))
            .chain(schedule(&b_plan, 20_000).into_iter().map(|(t, f, to)| (t, false, f, to)))
            .collect();
        events.sort_by_key(|e| e.0);
        for (t, from_a, from, to) in events {
            let (src, dst) = if from_a { (&mut a, &mut b) } else { (&mut b, &mut a) };
            let public_src = src.out(from, to);
            if to.ip() == dst.ip && dst.lets_in(to.port(), public_src) {
                return Some(t);
            }
        }
        None
    }

    fn rate(a: Mapping, b: Mapping) -> usize {
        (0..40u64).filter(|s| attempt(a, b, 1000 + s).is_some()).count()
    }

    #[test]
    fn easy_and_easy_connect_at_once() {
        assert_eq!(attempt(Mapping::Easy, Mapping::Easy, 1), Some(0));
    }

    #[test]
    fn predictable_hard_nats_are_reached() {
        assert_eq!(rate(Mapping::Sequential(1), Mapping::Easy), 40);
        assert_eq!(rate(Mapping::Easy, Mapping::Sequential(2)), 40);
        assert_eq!(rate(Mapping::Sequential(1), Mapping::Sequential(1)), 40);
    }

    #[test]
    fn random_hard_nat_against_easy_uses_the_birthday_trick() {
        let ok = rate(Mapping::Random, Mapping::Easy);
        assert!(ok >= 38, "birthday success {ok}/40");
        let ok = rate(Mapping::Easy, Mapping::Random);
        assert!(ok >= 38, "birthday success {ok}/40");
    }

    #[test]
    fn random_hard_nats_on_both_sides_mostly_fail_and_give_up() {
        // No relay by design: this is the case that gets the clear message.
        let ok = rate(Mapping::Random, Mapping::Random);
        assert!(ok <= 4, "unexpected success {ok}/40");
    }

    /// `cargo test -- --ignored --nocapture punch_rates` prints success
    /// rates and median time-to-connect over 500 runs per NAT pairing.
    #[test]
    #[ignore]
    fn punch_rates() {
        let kinds = [
            ("easy", Mapping::Easy),
            ("hard-seq(+1)", Mapping::Sequential(1)),
            ("hard-seq(+2)", Mapping::Sequential(2)),
            ("hard-random", Mapping::Random),
        ];
        for (an, a) in kinds {
            for (bn, b) in kinds {
                let mut times: Vec<u64> = (0..500u64).filter_map(|s| attempt(a, b, 77 + s)).collect();
                times.sort();
                let median = times.get(times.len() / 2).map(|t| format!("{:.1}s", *t as f64 / 1000.0)).unwrap_or("-".into());
                println!("{an:>13} <-> {bn:<13} {:>5.1}%  median {median}", times.len() as f64 / 5.0);
            }
        }
    }

    #[test]
    fn plans_stay_bounded() {
        let hard = NatInfo { kind: NatKind::Hard, ip: Some("100.64.0.1".parse().unwrap()), last_port: 30000, step: 0 };
        let p = plan(&hard, &hard, &["100.64.0.2:4000".parse().unwrap()], 9);
        let s = schedule(&p, 20_000);
        assert!(p.extras <= BIRTHDAY_SOCKETS);
        assert!(s.len() < 20_000 / SPRAY_GAP_MS as usize + BIRTHDAY_SOCKETS * 10 + 100, "{} probes", s.len());
    }
}
