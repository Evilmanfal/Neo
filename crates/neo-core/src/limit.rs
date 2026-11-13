//! Speed limits and pausing.
//!
//! - **Limits:** one shared bucket per direction (`Shared::limits`), so the
//!   cap is the total across every transfer at once. Each piece of data
//!   reserves its slot on a virtual clock and waits until then, so the pace is
//!   smooth (pieces of 16–256 KiB, a few dozen to a few hundred a second) with
//!   no saved-up bursts. "Unlimited" costs one atomic load per block.
//!   Receiving is limited by reading the QUIC streams more slowly, so QUIC
//!   flow control slows the sender down and nothing is dropped.
//! - **Pause:** every transfer has a `Gate` with a local and a remote flag.
//!   Data waits at the gate while either is set; the connection stays up on
//!   QUIC keepalives. Changes are emitted to the UI as `Event::Paused`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::Result;
use bytes::Bytes;
use tokio::sync::{watch, Notify};

/// One direction's shared limit, in bytes per second (0 = unlimited).
pub(crate) struct Bucket {
    rate: AtomicU64,
    /// When the next piece may go (virtual clock).
    next: Mutex<Option<Instant>>,
    changed: Notify,
}

impl Bucket {
    pub fn new(rate: u64) -> Self {
        Bucket { rate: AtomicU64::new(rate), next: Mutex::new(None), changed: Notify::new() }
    }

    pub fn rate(&self) -> u64 {
        self.rate.load(Ordering::Relaxed)
    }

    pub fn limited(&self) -> bool {
        self.rate() > 0
    }

    /// Takes effect at once, also for pieces already waiting.
    pub fn set_rate(&self, rate: u64) {
        if self.rate.swap(rate, Ordering::Relaxed) != rate {
            *self.next.lock().unwrap() = None;
            self.changed.notify_waiters();
        }
    }

    /// How big each piece should be at the current rate: about 50 per second,
    /// within 16–256 KiB.
    pub fn piece(&self) -> usize {
        ((self.rate() / 50) as usize).clamp(16 << 10, 256 << 10)
    }

    /// Waits until `n` more bytes may move.
    pub async fn take(&self, n: usize) {
        loop {
            let rate = self.rate();
            if rate == 0 {
                return;
            }
            let notified = self.changed.notified();
            let at = {
                let mut next = self.next.lock().unwrap();
                let now = Instant::now();
                let start = next.map_or(now, |t| t.max(now));
                *next = Some(start + Duration::from_secs_f64(n as f64 / rate as f64));
                start
            };
            if at <= Instant::now() {
                return;
            }
            tokio::select! {
                _ = tokio::time::sleep_until(at.into()) => return,
                // The limit changed: reserve again at the new rate.
                _ = notified => {}
            }
        }
    }
}

pub(crate) struct Limits {
    pub up: Bucket,
    pub down: Bucket,
    /// `NEO_SEND_LIMIT_MBPS` (benchmarks): caps only what the original sender
    /// uploads, not blocks recipients forward to each other.
    pub bench: Bucket,
}

impl Limits {
    pub fn new(up: u64, down: u64) -> Self {
        let bench = std::env::var("NEO_SEND_LIMIT_MBPS").ok().and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
        Limits { up: Bucket::new(up), down: Bucket::new(down), bench: Bucket::new((bench * 1e6) as u64) }
    }
}

/// MB/s (as typed in Settings) to bytes per second; None or ≤ 0 = unlimited.
pub fn to_rate(mb_per_s: Option<f64>) -> u64 {
    mb_per_s.filter(|v| v.is_finite() && *v > 0.0).map(|v| (v * 1e6) as u64).unwrap_or(0)
}

/// Pause check between pieces: the transfer's gate and its connection.
pub(crate) type Hold<'a> = Option<(&'a Gate, &'a quinn::Connection)>;

/// Pieces stay this small (or smaller, under a limit) so a pause lands
/// within a few milliseconds instead of after a whole 1 MiB block.
const PIECE_MAX: usize = 256 << 10;

/// Writes one block in pieces, stopping between them while paused and
/// pacing them when a limit is set.
pub(crate) async fn write(stream: &mut quinn::SendStream, data: Bytes, buckets: &[&Bucket], hold: Hold<'_>) -> Result<()> {
    let piece = buckets.iter().filter(|b| b.limited()).map(|b| b.piece()).min().unwrap_or(PIECE_MAX);
    let mut at = 0;
    while at < data.len() {
        let end = (at + piece).min(data.len());
        if let Some((gate, conn)) = hold {
            gate.wait_conn(conn).await?;
        }
        for b in buckets {
            b.take(end - at).await;
        }
        stream.write_chunk(data.slice(at..end)).await?;
        at = end;
    }
    Ok(())
}

/// Fills `buf` from the stream in pieces, stopping between them while
/// paused and pacing them when a limit is set.
pub(crate) async fn read(stream: &mut quinn::RecvStream, buf: &mut [u8], bucket: &Bucket, hold: Hold<'_>) -> Result<()> {
    let piece = if bucket.limited() { bucket.piece() } else { PIECE_MAX };
    let mut at = 0;
    while at < buf.len() {
        let end = (at + piece).min(buf.len());
        if let Some((gate, conn)) = hold {
            gate.wait_conn(conn).await?;
        }
        bucket.take(end - at).await;
        stream.read_exact(&mut buf[at..end]).await?;
        at = end;
    }
    Ok(())
}

/// Pause state of one transfer: (paused here, paused by the other side).
pub(crate) struct Gate {
    tx: watch::Sender<(bool, bool)>,
}

impl Default for Gate {
    fn default() -> Self {
        Gate { tx: watch::channel((false, false)).0 }
    }
}

impl Gate {
    pub fn get(&self) -> (bool, bool) {
        *self.tx.borrow()
    }

    pub fn paused(&self) -> bool {
        let (a, b) = self.get();
        a || b
    }

    pub fn local(&self) -> bool {
        self.get().0
    }

    /// Returns whether anything changed.
    pub fn set_local(&self, on: bool) -> bool {
        self.tx.send_if_modified(|s| std::mem::replace(&mut s.0, on) != on)
    }

    pub fn set_remote(&self, on: bool) -> bool {
        self.tx.send_if_modified(|s| std::mem::replace(&mut s.1, on) != on)
    }

    pub fn subscribe(&self) -> watch::Receiver<(bool, bool)> {
        self.tx.subscribe()
    }

    /// Wakes everyone waiting (e.g. on cancel) without changing anything.
    pub fn poke(&self) {
        self.tx.send_modify(|_| {});
    }

    /// Resolves once neither side has it paused.
    pub async fn wait(&self) {
        if !self.paused() {
            return;
        }
        let mut rx = self.subscribe();
        let _ = rx.wait_for(|(a, b)| !a && !b).await;
    }

    /// Like `wait`, but gives up when the connection closes.
    pub async fn wait_conn(&self, conn: &quinn::Connection) -> Result<()> {
        if !self.paused() {
            return Ok(());
        }
        tokio::select! {
            _ = self.wait() => Ok(()),
            e = conn.closed() => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bucket_paces_to_its_rate_and_follows_changes() {
        let b = Bucket::new(2_000_000);
        let t = Instant::now();
        for _ in 0..40 {
            b.take(50_000).await; // 2 MB in total
        }
        let secs = t.elapsed().as_secs_f64();
        assert!((0.9..1.1).contains(&secs), "took {secs}");

        // Raising the limit wakes a long wait right away.
        let slow = Bucket::new(1_000);
        slow.take(1_000).await;
        let t = Instant::now();
        let waiter = async { slow.take(10_000).await }; // ~1 s at the old rate
        let change = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            slow.set_rate(0);
        };
        tokio::join!(waiter, change);
        assert!(t.elapsed() < Duration::from_millis(500));
    }

    #[tokio::test]
    async fn gate_waits_for_both_sides() {
        let g = std::sync::Arc::new(Gate::default());
        assert!(g.set_local(true));
        assert!(!g.set_local(true));
        g.set_remote(true);
        let g2 = g.clone();
        let w = tokio::spawn(async move { g2.wait().await });
        tokio::time::sleep(Duration::from_millis(30)).await;
        g.set_local(false);
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!w.is_finished());
        g.set_remote(false);
        tokio::time::timeout(Duration::from_secs(1), w).await.unwrap().unwrap();
    }
}
