//! Congestion control on a bad link, simulated on loopback.
//!
//! A small UDP proxy sits between two real QUIC endpoints (Neo's TLS and
//! transport settings) and plays a bottleneck link: a bandwidth cap with a
//! drop-tail buffer of one bandwidth-delay product, a fixed delay each way,
//! and random loss both ways. The quick test checks a transfer survives 3%
//! loss byte-perfect; the ignored benchmark compares BBR and Cubic:
//!
//! cargo test --release --test cc -- --ignored --nocapture

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use neo_core::identity::Identity;
use neo_core::tls::{self, Cc};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

#[derive(Clone, Copy, Debug)]
struct Link {
    /// Bottleneck rate (client to server), bits per second.
    rate: f64,
    /// One-way delay.
    delay: Duration,
    /// Random loss, each way.
    loss: f64,
}

/// Lossy link: send to the returned address instead of `server`.
async fn proxy(server: SocketAddr, link: Link) -> SocketAddr {
    let front = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let addr = front.local_addr().unwrap();
    // Forward direction: one shared bottleneck queue, delivered in order.
    let (fwd_tx, mut fwd_rx) = mpsc::unbounded_channel::<(tokio::time::Instant, Vec<u8>, Arc<UdpSocket>)>();
    tokio::spawn(async move {
        while let Some((at, pkt, up)) = fwd_rx.recv().await {
            tokio::time::sleep_until(at).await;
            let _ = up.send_to(&pkt, server).await;
        }
    });
    let queue_cap = Duration::from_secs_f64((link.delay * 2).as_secs_f64().max(0.02)); // ~1 BDP of buffer
    tokio::spawn(async move {
        let mut ups: HashMap<SocketAddr, Arc<UdpSocket>> = HashMap::new();
        let mut next_free = tokio::time::Instant::now();
        let mut buf = vec![0u8; 65536];
        let mut seed = 0x9e3779b97f4a7c15u64;
        let mut rand = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 11) as f64 / (1u64 << 53) as f64
        };
        loop {
            let Ok((n, from)) = front.recv_from(&mut buf).await else { return };
            let up = match ups.get(&from) {
                Some(u) => u.clone(),
                None => {
                    let up = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
                    // Back direction (acks, mostly): delay and loss, no cap.
                    let (front, up2, delay, loss) = (front.clone(), up.clone(), link.delay, link.loss);
                    let (back_tx, mut back_rx) = mpsc::unbounded_channel::<(tokio::time::Instant, Vec<u8>)>();
                    tokio::spawn(async move {
                        while let Some((at, pkt)) = back_rx.recv().await {
                            tokio::time::sleep_until(at).await;
                            let _ = front.send_to(&pkt, from).await;
                        }
                    });
                    tokio::spawn(async move {
                        let mut b = vec![0u8; 65536];
                        let mut s = 0x2545f4914f6cdd1du64 ^ from.port() as u64;
                        while let Ok((n, _)) = up2.recv_from(&mut b).await {
                            s ^= s << 13;
                            s ^= s >> 7;
                            s ^= s << 17;
                            if ((s >> 11) as f64 / (1u64 << 53) as f64) < loss {
                                continue;
                            }
                            let _ = back_tx.send((tokio::time::Instant::now() + delay, b[..n].to_vec()));
                        }
                    });
                    ups.insert(from, up.clone());
                    up
                }
            };
            if rand() < link.loss {
                continue;
            }
            let now = tokio::time::Instant::now();
            let start = next_free.max(now);
            if start - now > queue_cap {
                continue; // buffer full: drop-tail
            }
            next_free = start + Duration::from_secs_f64(n as f64 * 8.0 / link.rate);
            let _ = fwd_tx.send((next_free + link.delay, buf[..n].to_vec(), up));
        }
    });
    addr
}

struct Ends {
    server: quinn::Endpoint,
    server_id: Identity,
    client_id: Identity,
}

fn ends(tag: &str) -> Ends {
    let dir = std::env::temp_dir().join(format!("neo-cc-{}-{tag}", std::process::id()));
    let server_id = Identity::load_or_create(&dir.join("s")).unwrap();
    let client_id = Identity::load_or_create(&dir.join("c")).unwrap();
    let server = quinn::Endpoint::server(tls::server_config(&server_id).unwrap(), "127.0.0.1:0".parse().unwrap()).unwrap();
    Ends { server, server_id, client_id }
}

/// One sender flow pushing as fast as its congestion controller allows.
async fn flow(e: &Ends, to: SocketAddr, cc: Cc, until: Instant) -> Result<(), String> {
    let mut client = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).map_err(|e| e.to_string())?;
    let mut cfg = tls::client_config(&e.client_id, e.server_id.id).map_err(|e| e.to_string())?;
    cfg.transport_config(tls::transport_for(cc));
    client.set_default_client_config(cfg);
    let conn = client.connect(to, tls::SERVER_NAME).map_err(|e| e.to_string())?.await.map_err(|e| e.to_string())?;
    let mut s = conn.open_uni().await.map_err(|e| e.to_string())?;
    let chunk = bytes::Bytes::from(vec![7u8; 64 << 10]);
    while Instant::now() < until {
        if tokio::time::timeout(Duration::from_millis(200), s.write_chunk(chunk.clone())).await.is_err() {
            continue;
        }
    }
    conn.close(0u32.into(), b"done");
    Ok(())
}

fn fine_timers() {
    #[cfg(windows)]
    unsafe {
        windows_sys::Win32::Media::timeBeginPeriod(1);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn bbr_carries_data_intact_through_loss_and_delay() {
    fine_timers();
    let e = ends("quick");
    let server_addr = e.server.local_addr().unwrap();
    let link = Link { rate: 40e6, delay: Duration::from_millis(50), loss: 0.03 };
    let to = proxy(SocketAddr::from(([127, 0, 0, 1], server_addr.port())), link).await;
    let data: Vec<u8> = (0..3_000_000u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
    let recv = {
        let server = e.server.clone();
        tokio::spawn(async move {
            let conn = server.accept().await.unwrap().await.unwrap();
            let mut s = conn.accept_uni().await.unwrap();
            s.read_to_end(10 << 20).await.unwrap()
        })
    };
    let mut client = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    let mut cfg = tls::client_config(&e.client_id, e.server_id.id).unwrap();
    cfg.transport_config(tls::transport_for(Cc::Bbr));
    client.set_default_client_config(cfg);
    let t = Instant::now();
    let conn = client.connect(to, tls::SERVER_NAME).unwrap().await.unwrap();
    let mut s = conn.open_uni().await.unwrap();
    s.write_all(&data).await.unwrap();
    s.finish().unwrap();
    let got = tokio::time::timeout(Duration::from_secs(60), recv).await.unwrap().unwrap();
    eprintln!("3 MB through 3% loss, 100 ms RTT, 40 Mbit/s: {:.2} s", t.elapsed().as_secs_f64());
    assert!(got == data, "arrived byte-identical");
}

/// Goodput (Mbit/s) of each flow, all through ONE bottleneck queue to one
/// server, measured over `secs` after a warm-up. Flows start 300 ms apart.
async fn run(link: Link, ccs: &[Cc], secs: u64) -> Vec<f64> {
    let e = ends(&format!("{ccs:?}-{}-{}", link.loss, link.delay.as_millis()));
    let server = e.server.local_addr().unwrap();
    // One counter per connection, in arrival order (= flow order).
    let counters: Arc<std::sync::Mutex<Vec<Arc<AtomicU64>>>> = Arc::default();
    {
        let (server_ep, counters) = (e.server.clone(), counters.clone());
        tokio::spawn(async move {
            while let Some(inc) = server_ep.accept().await {
                let g = Arc::new(AtomicU64::new(0));
                counters.lock().unwrap().push(g.clone());
                tokio::spawn(async move {
                    let Ok(conn) = inc.await else { return };
                    while let Ok(mut s) = conn.accept_uni().await {
                        let g = g.clone();
                        tokio::spawn(async move {
                            let mut buf = vec![0u8; 64 << 10];
                            while let Ok(Some(n)) = s.read(&mut buf).await {
                                g.fetch_add(n as u64, Ordering::Relaxed);
                            }
                        });
                    }
                });
            }
        });
    }
    let to = proxy(SocketAddr::from(([127, 0, 0, 1], server.port())), link).await;
    let warm = Duration::from_secs(3);
    let until = Instant::now() + warm + Duration::from_millis(300 * ccs.len() as u64) + Duration::from_secs(secs);
    let flows = ccs.iter().enumerate().map(|(i, cc)| {
        let e = &e;
        async move {
            tokio::time::sleep(Duration::from_millis(300 * i as u64)).await;
            flow(e, to, *cc, until).await
        }
    });
    let measure = async {
        tokio::time::sleep(warm + Duration::from_millis(300 * ccs.len() as u64)).await;
        let before: Vec<u64> = counters.lock().unwrap().iter().map(|c| c.load(Ordering::Relaxed)).collect();
        tokio::time::sleep(Duration::from_secs(secs)).await;
        let after: Vec<u64> = counters.lock().unwrap().iter().map(|c| c.load(Ordering::Relaxed)).collect();
        after.iter().zip(before.iter()).map(|(a, b)| (a - b) as f64 * 8.0 / secs as f64 / 1e6).collect::<Vec<f64>>()
    };
    let (rs, out) = tokio::join!(futures_util::future::join_all(flows), measure);
    for r in rs {
        r.unwrap();
    }
    out
}

fn jain(x: &[f64]) -> f64 {
    let s: f64 = x.iter().sum();
    let q: f64 = x.iter().map(|v| v * v).sum();
    if q == 0.0 {
        0.0
    } else {
        s * s / (x.len() as f64 * q)
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn bench_bbr_vs_cubic_under_loss() {
    fine_timers();
    let base = Link { rate: 50e6, delay: Duration::from_millis(50), loss: 0.0 };
    eprintln!("Single flow, 50 Mbit/s bottleneck, 1 BDP buffer, 8 s after warm-up (Mbit/s):");
    for (rtt, loss) in [(100, 0.0), (100, 0.01), (100, 0.03), (200, 0.01)] {
        let link = Link { delay: Duration::from_millis(rtt / 2), loss, ..base };
        let bbr = run(link, &[Cc::Bbr], 8).await[0];
        let cubic = run(link, &[Cc::Cubic], 8).await[0];
        eprintln!("  RTT {rtt} ms, loss {:>3.0}%:  BBR {bbr:5.1}   Cubic {cubic:5.1}", loss * 100.0);
    }
    eprintln!("Two flows sharing the bottleneck, RTT 100 ms, 10 s (Mbit/s each, Jain fairness):");
    for loss in [0.0, 0.01] {
        let link = Link { loss, ..base };
        for pair in [[Cc::Bbr, Cc::Bbr], [Cc::Cubic, Cc::Cubic], [Cc::Bbr, Cc::Cubic]] {
            let r = run(link, &pair, 10).await;
            eprintln!("  loss {:.0}%  {:?} vs {:?}: {:.1} / {:.1}  (fairness {:.2})", loss * 100.0, pair[0], pair[1], r[0], r.get(1).copied().unwrap_or(0.0), jain(&r));
        }
    }
}
