//! Swarm fan-out: when one batch goes to several devices, recipients pass
//! blocks on to each other so the sender's upload is spent (about) once
//! instead of once per recipient.
//!
//! How it works (fully automatic, used whenever a send has 2+ recipients):
//!
//! - The **sender** reads and hashes every block once and gives it to one
//!   recipient: whichever recipient's connection has room next (work
//!   stealing), so fast recipients take more. Every block's BLAKE3 hash goes
//!   straight from the sender to *every* recipient on a small hash stream.
//! - A recipient that gets a block from the sender **forwards** it to the other
//!   recipients it's linked with. Blocks from peers are only accepted if they
//!   match the sender's hash, and are never forwarded again (no loops).
//! - **Links** between recipients use the normal pinned QUIC/TLS connections.
//!   The sender gives each recipient a roster (device ids and addresses only:
//!   no names, no accounts) plus a per-batch secret. A link is accepted only
//!   with a proof derived from that secret, and only carries this batch.
//! - **Fallbacks:** each recipient tells the sender which peers it is linked
//!   with. For pairs that aren't linked, the sender sends the block directly
//!   as well. A push to a slow peer is dropped rather than waited for, so a
//!   slow recipient never holds anyone up. At the end ("endgame"), recipients
//!   ask the sender for any block they still miss.
//! - **Resume:** blocks may arrive out of order, but the resume record keeps
//!   the contiguous verified prefix of each file, as in direct sends.
//! - **Compression:** the sender compresses each block once (see `wire`);
//!   recipients forward it still compressed and only decompress their own copy.
//!   Delta ("send only what changed") is for direct sends only: a swarm's
//!   recipients may each hold a different old version.
//! - **Pause:** a paused recipient gets no new blocks from the sender (the
//!   others take them), and pushes to it are dropped like pushes to a slow
//!   peer, so nobody else waits. It asks for what it missed after resuming.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, Notify};

use crate::identity::{hex, unhex, DeviceId};
use crate::proto::{read_frame, write_frame, FileMeta, Msg, BLOCK};
use crate::transfer::{self, fatal, BatchState};
use crate::limit::{self, Gate, Limits};
use crate::wire::{self, Decoder, Encoder, RAW};
use crate::{partial, Direction, Peer, Shared, TransferInfo, TransferState};

const STREAM_DATA: u8 = 1;
const STREAM_HASHES: u8 = 2;
const STREAM_PUSH: u8 = 3;
/// Blocks queued toward one peer before further pushes to it are dropped.
const PUSH_QUEUE: usize = 8;
/// Peer blocks held while their hash from the sender is still on its way.
const PENDING_MAX: usize = 16;
/// Recently read blocks kept for unlinked recipients that need them too.
const CACHE_BLOCKS: usize = 16;

/// Swarming is on by default; `NEO_SWARM=0` forces direct sends (benchmarks).
pub(crate) fn enabled() -> bool {
    std::env::var("NEO_SWARM").map(|v| v != "0").unwrap_or(true)
}

/// Test knob: `NEO_SWARM_NO_LINKS=1` makes this device refuse and skip
/// links with co-recipients, as if no path existed (exercises the fallback).
fn links_disabled() -> bool {
    std::env::var_os("NEO_SWARM_NO_LINKS").is_some()
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RosterEntry {
    pub id: DeviceId,
    pub addrs: Vec<SocketAddr>,
}

fn nblocks(size: u64) -> u64 {
    size.div_ceil(BLOCK).max(1)
}

fn block_len(size: u64, b: u64) -> usize {
    size.saturating_sub(b * BLOCK).min(BLOCK) as usize
}

fn proof(secret: &[u8; 32], dialer: DeviceId, accepter: DeviceId) -> String {
    let mut m = b"neo-swarm-hello".to_vec();
    m.extend_from_slice(&dialer.0);
    m.extend_from_slice(&accepter.0);
    hex(blake3::keyed_hash(secret, &m).as_bytes())
}

fn random<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    let _ = ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut out);
    out
}

// ------------------------------------------------------------------ sender

#[derive(Default)]
struct Recipient {
    accepted: bool,
    done: bool,
    /// Peers this recipient says it's linked with (it forwards to them).
    linked: HashSet<DeviceId>,
    /// Blocks only the sender can get to this recipient.
    direct: VecDeque<(u32, u64)>,
    queued: HashSet<(u32, u64)>,
    hashes: Option<mpsc::UnboundedSender<(u32, u64, [u8; 32])>>,
    wake: Arc<Notify>,
    told_all_assigned: bool,
}

struct Coordinator {
    files: Arc<Vec<(PathBuf, FileMeta)>>,
    nblocks: Vec<u64>,
    hashes: Mutex<Vec<Vec<Option<[u8; 32]>>>>,
    /// Next block nobody has been given yet.
    cursor: Mutex<(usize, u64)>,
    recipients: Mutex<HashMap<DeviceId, Recipient>>,
    /// Recently sent blocks as they went on the wire: (tag, payload, original length).
    cache: Mutex<VecDeque<((u32, u64), (u8, Bytes, usize))>>,
    /// One compressor per file (blocks of one file compress one at a time).
    encoders: Mutex<HashMap<usize, Arc<Mutex<Encoder>>>>,
    handles: Mutex<HashMap<usize, Arc<std::fs::File>>>,
    batch: [u8; 16],
    secret: [u8; 32],
    roster: Vec<RosterEntry>,
}

impl Coordinator {
    fn wake(&self, me: DeviceId) -> Arc<Notify> {
        self.recipients.lock().unwrap().entry(me).or_default().wake.clone()
    }

    /// Blocks handed out so far, in order.
    fn assigned_so_far(&self) -> Vec<(u32, u64)> {
        let (cf, cb) = *self.cursor.lock().unwrap();
        let mut out = Vec::new();
        for (f, &n) in self.nblocks.iter().enumerate() {
            let upto = if f < cf { n } else if f == cf { cb } else { 0 };
            out.extend((0..upto).map(|b| (f as u32, b)));
        }
        out
    }

    /// A recipient accepted (again): it needs every already-distributed
    /// block past its resume point straight from us; peers cover the rest.
    fn on_accept(&self, me: DeviceId, offsets: &[u64], have: &[u32]) {
        let missed: Vec<(u32, u64)> = self
            .assigned_so_far()
            .into_iter()
            .filter(|(f, b)| !have.contains(f) && *b * BLOCK >= offsets.get(*f as usize).copied().unwrap_or(0))
            .collect();
        let mut rs = self.recipients.lock().unwrap();
        let r = rs.entry(me).or_default();
        r.accepted = true;
        r.done = false;
        r.linked.clear();
        r.told_all_assigned = false;
        for blk in missed {
            if r.queued.insert(blk) {
                r.direct.push_back(blk);
            }
        }
    }

    fn on_disconnect(&self, me: DeviceId) {
        let mut rs = self.recipients.lock().unwrap();
        if let Some(r) = rs.get_mut(&me) {
            r.accepted = false;
            r.linked.clear();
            r.hashes = None;
            r.direct.clear();
            r.queued.clear();
        }
    }

    fn register_hashes(&self, me: DeviceId, tx: mpsc::UnboundedSender<(u32, u64, [u8; 32])>) {
        let hashes = self.hashes.lock().unwrap();
        for (f, list) in hashes.iter().enumerate() {
            for (b, h) in list.iter().enumerate() {
                if let Some(h) = h {
                    let _ = tx.send((f as u32, b as u64, *h));
                }
            }
        }
        self.recipients.lock().unwrap().entry(me).or_default().hashes = Some(tx);
    }

    fn set_linked(&self, me: DeviceId, ids: Vec<DeviceId>) {
        if let Some(r) = self.recipients.lock().unwrap().get_mut(&me) {
            r.linked = ids.into_iter().collect();
        }
    }

    fn need(&self, me: DeviceId, file: u32, blocks: Vec<u64>) {
        let mut rs = self.recipients.lock().unwrap();
        if let Some(r) = rs.get_mut(&me) {
            for b in blocks {
                if (file as usize) < self.nblocks.len() && b < self.nblocks[file as usize] && r.queued.insert((file, b)) {
                    r.direct.push_back((file, b));
                }
            }
            r.wake.notify_one();
        }
    }

    fn finished(&self, me: DeviceId) {
        if let Some(r) = self.recipients.lock().unwrap().get_mut(&me) {
            r.done = true;
        }
    }

    /// What this recipient's connection should carry next.
    fn assign(&self, me: DeviceId) -> Option<(u32, u64)> {
        {
            let mut rs = self.recipients.lock().unwrap();
            if let Some(blk) = rs.get_mut(&me).and_then(|r| r.direct.pop_front()) {
                rs.get_mut(&me).unwrap().queued.remove(&blk);
                return Some(blk);
            }
        }
        let blk = {
            let mut cur = self.cursor.lock().unwrap();
            loop {
                let (f, b) = *cur;
                if f >= self.nblocks.len() {
                    return None;
                }
                if b < self.nblocks[f] {
                    cur.1 += 1;
                    break (f as u32, b);
                }
                *cur = (f + 1, 0);
            }
        };
        // Recipients `me` can't forward to get it from us directly.
        let mut rs = self.recipients.lock().unwrap();
        let linked = rs.get(&me).map(|r| r.linked.clone()).unwrap_or_default();
        for (id, r) in rs.iter_mut() {
            if *id != me && r.accepted && !r.done && !linked.contains(id) && r.queued.insert(blk) {
                r.direct.push_back(blk);
                r.wake.notify_one();
            }
        }
        Some(blk)
    }

    fn all_assigned(&self) -> bool {
        self.cursor.lock().unwrap().0 >= self.nblocks.len()
    }

    /// Reads a block (or takes it from the cache), hashing it the first time
    /// and announcing the hash to every recipient.
    /// Returns (tag, payload, hash, original length), compressed when worthwhile.
    async fn block(&self, f: u32, b: u64) -> Result<(u8, Bytes, [u8; 32], usize)> {
        if let Some((_, (tag, data, n))) = self.cache.lock().unwrap().iter().find(|(k, _)| *k == (f, b)) {
            let h = self.hashes.lock().unwrap()[f as usize][b as usize];
            if let Some(h) = h {
                return Ok((*tag, data.clone(), h, *n));
            }
        }
        let (path, meta) = self.files[f as usize].clone();
        let handle = {
            let mut handles = self.handles.lock().unwrap();
            match handles.get(&(f as usize)) {
                Some(h) => h.clone(),
                None => {
                    let h = Arc::new(
                        std::fs::File::open(&path).map_err(|e| fatal(format!("Can't open {}: {e}", path.display())))?,
                    );
                    handles.insert(f as usize, h.clone());
                    h
                }
            }
        };
        let len = block_len(meta.size, b);
        let enc = self
            .encoders
            .lock()
            .unwrap()
            .entry(f as usize)
            .or_insert_with(|| Arc::new(Mutex::new(Encoder::new(&meta.name))))
            .clone();
        let (tag, data, hash) = tokio::task::spawn_blocking(move || -> Result<(u8, Bytes, [u8; 32])> {
            let mut buf = vec![0u8; len];
            read_at(&handle, &mut buf, b * BLOCK).map_err(|_| fatal(format!("{} changed while sending", path.display())))?;
            let hash = *blake3::hash(&buf).as_bytes();
            let (tag, data) = enc.lock().unwrap().block(Bytes::from(buf));
            Ok((tag, data, hash))
        })
        .await??;
        let fresh = {
            let mut hashes = self.hashes.lock().unwrap();
            let slot = &mut hashes[f as usize][b as usize];
            match slot {
                Some(known) if *known != hash => return Err(fatal(format!("{} changed while sending", meta.name))),
                Some(_) => false,
                None => {
                    *slot = Some(hash);
                    true
                }
            }
        };
        if fresh {
            for r in self.recipients.lock().unwrap().values() {
                if let Some(tx) = &r.hashes {
                    let _ = tx.send((f, b, hash));
                }
            }
        }
        let mut cache = self.cache.lock().unwrap();
        if cache.len() >= CACHE_BLOCKS {
            cache.pop_front();
        }
        cache.push_back(((f, b), (tag, data.clone(), len)));
        Ok((tag, data, hash, len))
    }
}

#[cfg(windows)]
fn read_at(file: &std::fs::File, buf: &mut [u8], mut offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut done = 0;
    while done < buf.len() {
        let n = file.seek_read(&mut buf[done..], offset)?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        done += n;
        offset += n as u64;
    }
    Ok(())
}

#[cfg(unix)]
fn read_at(file: &std::fs::File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}

#[cfg(windows)]
fn write_at(file: &std::fs::File, buf: &[u8], mut offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut done = 0;
    while done < buf.len() {
        let n = file.seek_write(&buf[done..], offset)?;
        done += n;
        offset += n as u64;
    }
    Ok(())
}

#[cfg(unix)]
fn write_at(file: &std::fs::File, buf: &[u8], offset: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, buf, offset)
}

/// Sends one batch to several recipients as a swarm.
pub(crate) async fn send_job(
    shared: Arc<Shared>,
    files: Arc<Vec<(PathBuf, FileMeta)>>,
    metas: Arc<Vec<FileMeta>>,
    total: u64,
    peers: Vec<Peer>,
) {
    let job = shared.next_id();
    let nblocks: Vec<u64> = metas.iter().map(|m| nblocks(m.size)).collect();
    let coord = Arc::new(Coordinator {
        hashes: Mutex::new(nblocks.iter().map(|&n| vec![None; n as usize]).collect()),
        nblocks,
        files,
        cursor: Mutex::new((0, 0)),
        recipients: Mutex::new(HashMap::new()),
        cache: Mutex::new(VecDeque::new()),
        encoders: Mutex::new(HashMap::new()),
        handles: Mutex::new(HashMap::new()),
        batch: random(),
        secret: random(),
        roster: peers
            .iter()
            .map(|p| {
                let mut addrs = vec![p.addr];
                addrs.extend(p.alt.iter().copied().filter(|a| *a != p.addr));
                RosterEntry { id: p.id, addrs }
            })
            .collect(),
    });
    for peer in peers {
        let id = shared.next_id();
        let info = TransferInfo {
            id,
            job,
            direction: Direction::Outgoing,
            peer_id: peer.id,
            peer_name: peer.name.clone(),
            peer_account: peer.account,
            files: metas.clone(),
            total,
        };
        let progress = shared.register(info, true);
        let (shared, coord, metas) = (shared.clone(), coord.clone(), metas.clone());
        tokio::spawn(async move {
            let result = send_with_retry(&shared, &coord, id, &peer, metas, total, progress).await;
            coord.finished(peer.id);
            transfer::report(&shared, id, result);
        });
    }
}

async fn send_with_retry(
    shared: &Shared,
    coord: &Arc<Coordinator>,
    id: u64,
    peer: &Peer,
    metas: Arc<Vec<FileMeta>>,
    total: u64,
    progress: Arc<AtomicU64>,
) -> Result<TransferState> {
    let mut target = peer.clone();
    let mut accepted = false;
    let mut backoff = Duration::from_secs(1);
    let mut deadline = Instant::now() + Duration::from_secs(300);
    loop {
        transfer::hold_while_paused(shared, id).await;
        shared.set_remote_pause(id, false);
        let before = progress.load(Ordering::Relaxed);
        let result = send_attempt(shared, coord, id, &target, &metas, total, &progress, &mut accepted).await;
        coord.on_disconnect(peer.id);
        let err = match result {
            Ok(state) => return Ok(state),
            Err(e) => e,
        };
        if progress.load(Ordering::Relaxed) > before || shared.gate(id).paused() {
            deadline = Instant::now() + Duration::from_secs(300);
            backoff = Duration::from_secs(1);
        }
        let retry = matches!(transfer::classify(&err), transfer::Outcome::Retry(_)) && accepted && Instant::now() < deadline;
        if !retry || shared.is_gone(id) {
            return Err(err);
        }
        shared.set_state(id, TransferState::Interrupted);
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(15));
        if shared.is_gone(id) {
            return Err(err);
        }
        if let Some(p) = shared.peer(peer.id) {
            target = p;
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn send_attempt(
    shared: &Shared,
    coord: &Arc<Coordinator>,
    id: u64,
    peer: &Peer,
    metas: &Arc<Vec<FileMeta>>,
    total: u64,
    progress: &Arc<AtomicU64>,
    accepted: &mut bool,
) -> Result<TransferState> {
    if !*accepted {
        shared.set_state(id, TransferState::Connecting);
    }
    let conn = shared.connect_peer(peer).await?;
    shared.attach_conn(id, &conn);
    let gate = shared.gate(id);
    let (mut ctl_tx, mut ctl_rx) = conn.open_bi().await?;
    let offer = Msg::Offer {
        sender_name: shared.config().name,
        files: (**metas).clone(),
        total,
        attest: Some(shared.attest()),
        swarm: true,
        paused: gate.local(),
    };
    let offered_paused = matches!(offer, Msg::Offer { paused: true, .. });
    write_frame(&mut ctl_tx, &offer).await?;
    if !*accepted {
        shared.set_state(id, TransferState::Waiting);
    }
    let (offsets, have) = match read_frame::<Msg>(&mut ctl_rx).await? {
        Msg::Decision { accept: true, offsets, have, paused, .. } => {
            shared.set_remote_pause(id, paused);
            (offsets, have)
        }
        Msg::Decision { accept: false, .. } => {
            conn.close(transfer::DONE, b"declined");
            return Ok(TransferState::Declined);
        }
        other => bail!("unexpected reply {other:?}"),
    };
    *accepted = true;
    let already: u64 = metas
        .iter()
        .enumerate()
        .map(|(i, m)| if have.contains(&(i as u32)) { m.size } else { offsets.get(i).copied().unwrap_or(0).min(m.size) })
        .sum();
    progress.store(already, Ordering::Relaxed);

    write_frame(&mut ctl_tx, &Msg::Swarm { batch: hex(&coord.batch), secret: hex(&coord.secret), roster: coord.roster.clone() })
        .await?;
    coord.on_accept(peer.id, &offsets, &have);
    shared.set_state(id, TransferState::Active);

    // Hash stream: every block's hash, as soon as the sender knows it.
    let mut hashes = conn.open_uni().await?;
    hashes.write_all(&[STREAM_HASHES]).await?;
    let (htx, mut hrx) = mpsc::unbounded_channel::<(u32, u64, [u8; 32])>();
    coord.register_hashes(peer.id, htx);
    let hash_task = tokio::spawn(async move {
        while let Some((f, b, h)) = hrx.recv().await {
            let mut frame = Vec::with_capacity(44);
            frame.extend_from_slice(&f.to_le_bytes());
            frame.extend_from_slice(&b.to_le_bytes());
            frame.extend_from_slice(&h);
            if hashes.write_all(&frame).await.is_err() {
                break;
            }
        }
    });

    // Recipient -> sender control messages.
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<Msg>();
    let reader = tokio::spawn(async move {
        while let Ok(m) = read_frame::<Msg>(&mut ctl_rx).await {
            if ev_tx.send(m).is_err() {
                break;
            }
        }
    });

    let result: Result<TransferState> = async {
        let mut data = conn.open_uni().await?;
        data.write_all(&[STREAM_DATA]).await?;
        let wake = coord.wake(peer.id);
        let stats = shared.stats(id);
        let mut pause_rx = gate.subscribe();
        let mut told = offered_paused;
        let on_msg = |m: Msg| -> Result<Option<TransferState>> {
            if let Msg::Pause { paused } = m {
                shared.set_remote_pause(id, paused);
                return Ok(None);
            }
            handle_control(coord, peer.id, progress, m)
        };
        loop {
            while let Ok(m) = ev_rx.try_recv() {
                if let Some(done) = on_msg(m)? {
                    conn.close(transfer::DONE, b"done");
                    return Ok(done);
                }
            }
            if gate.local() != told {
                told = gate.local();
                write_frame(&mut ctl_tx, &Msg::Pause { paused: told }).await?;
            }
            // Paused (either side): this recipient takes no new blocks; the
            // others carry on with them.
            let next = if gate.paused() { None } else { coord.assign(peer.id) };
            if let Some((f, b)) = next {
                let (tag, bytes, hash, n) = coord.block(f, b).await?;
                let mut header = Vec::with_capacity(49);
                header.extend_from_slice(&f.to_le_bytes());
                header.extend_from_slice(&b.to_le_bytes());
                header.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
                header.extend_from_slice(&hash);
                header.push(tag);
                data.write_all(&header).await?;
                stats.add(n as u64, (header.len() + bytes.len()) as u64);
                limit::write(&mut data, bytes, &[&shared.limits.bench, &shared.limits.up], Some((&*gate, &conn))).await?;
                continue;
            }
            if coord.all_assigned() {
                let tell = {
                    let mut rs = coord.recipients.lock().unwrap();
                    let r = rs.entry(peer.id).or_default();
                    !std::mem::replace(&mut r.told_all_assigned, true)
                };
                if tell {
                    write_frame(&mut ctl_tx, &Msg::AllAssigned).await?;
                }
            }
            tokio::select! {
                m = ev_rx.recv() => match m {
                    Some(m) => {
                        if let Some(done) = on_msg(m)? {
                            conn.close(transfer::DONE, b"done");
                            return Ok(done);
                        }
                    }
                    None => return Err(conn.closed().await.into()),
                },
                _ = wake.notified() => {}
                _ = pause_rx.changed() => {}
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
        }
    }
    .await;
    reader.abort();
    hash_task.abort();
    if let Err(e) = &result {
        if let Some(f) = e.downcast_ref::<transfer::Fatal>() {
            conn.close(transfer::FAILED, f.0.as_bytes());
        }
    }
    result
}

/// Returns the final state once the recipient reports completion.
fn handle_control(coord: &Coordinator, me: DeviceId, progress: &AtomicU64, m: Msg) -> Result<Option<TransferState>> {
    match m {
        Msg::SwarmPeers { ids } => coord.set_linked(me, ids),
        Msg::SwarmHave { done } => progress.store(done, Ordering::Relaxed),
        Msg::Need { file, blocks } => coord.need(me, file, blocks),
        Msg::Complete { error: None } => {
            coord.finished(me);
            return Ok(Some(TransferState::Done { saved: Vec::new() }));
        }
        Msg::Complete { error: Some(e) } => return Err(fatal(e)),
        _ => {}
    }
    Ok(None)
}

// --------------------------------------------------------------- recipient

struct RecvState {
    have: Vec<Vec<bool>>,
    hashes: Vec<Vec<Option<[u8; 32]>>>,
    pending: HashMap<(u32, u64), Bytes>,
    missing: u64,
    last_new: Instant,
}

/// One batch being received as part of a swarm.
pub(crate) struct SwarmRecv {
    batch: [u8; 16],
    secret: [u8; 32],
    me: DeviceId,
    roster: Vec<RosterEntry>,
    sizes: Vec<u64>,
    state: Mutex<RecvState>,
    writer: mpsc::Sender<(u32, u64, Bytes)>,
    /// Decompresses blocks (from the sender or peers) for our own copy.
    dec: Mutex<Decoder>,
    stats: Arc<crate::WireStats>,
    /// Push queues toward linked peers.
    links: Mutex<HashMap<DeviceId, mpsc::Sender<(u32, u64, u8, Bytes)>>>,
    control: mpsc::UnboundedSender<Msg>,
    duplicate: AtomicU64,
    gate: Arc<Gate>,
    limits: Arc<Limits>,
}

impl SwarmRecv {
    fn report_links(&self) {
        let ids: Vec<DeviceId> = self.links.lock().unwrap().keys().copied().collect();
        let _ = self.control.send(Msg::SwarmPeers { ids });
    }

    fn on_hash(self: &Arc<Self>, f: u32, b: u64, h: [u8; 32]) -> Option<Bytes> {
        let mut st = self.state.lock().unwrap();
        let slot = st.hashes.get_mut(f as usize)?.get_mut(b as usize)?;
        *slot = Some(h);
        st.pending.remove(&(f, b))
    }

    /// A block arrived from the sender (with its hash) or from a peer, as it
    /// travelled (`tag` + payload, see `wire`).
    async fn accept(self: &Arc<Self>, f: u32, b: u64, tag: u8, payload: Bytes, inline: Option<[u8; 32]>) -> Result<()> {
        let from_sender = inline.is_some();
        let (fi, bi) = (f as usize, b as usize);
        let bad = |what: &str| -> Result<()> {
            if from_sender {
                bail!("sender sent {what}");
            }
            Ok(()) // a peer sent garbage: ignore it
        };
        if fi >= self.sizes.len() || b >= nblocks(self.sizes[fi]) {
            return bad("a block that doesn't fit");
        }
        let want = block_len(self.sizes[fi], b);
        if from_sender {
            // Pass it on, still compressed, to everyone linked with us. A full
            // queue means that peer is slow: skip it (it'll ask the sender
            // later) rather than wait.
            for tx in self.links.lock().unwrap().values() {
                let _ = tx.try_send((f, b, tag, payload.clone()));
            }
        }
        let data = if tag == RAW {
            payload
        } else {
            match self.dec.lock().unwrap().decode(tag, payload.to_vec(), want, None) {
                Ok((d, _)) => Bytes::from(d),
                Err(_) => return bad("a block that doesn't decode"),
            }
        };
        if data.len() != want {
            return bad("a block that doesn't fit");
        }
        let hash = {
            let mut st = self.state.lock().unwrap();
            if let Some(h) = inline {
                st.hashes[fi][bi] = Some(h);
            }
            if st.have[fi][bi] {
                self.duplicate.fetch_add(data.len() as u64, Ordering::Relaxed);
                return Ok(());
            }
            match st.hashes[fi][bi] {
                Some(h) => h,
                None => {
                    if st.pending.len() < PENDING_MAX {
                        st.pending.insert((f, b), data);
                    }
                    return Ok(());
                }
            }
        };
        if blake3::hash(&data).as_bytes() != &hash {
            if from_sender {
                bail!("a block failed its integrity check");
            }
            return Ok(()); // a peer sent a bad block: ignore it, someone else will send it
        }
        {
            let mut st = self.state.lock().unwrap();
            if std::mem::replace(&mut st.have[fi][bi], true) {
                return Ok(());
            }
            st.missing -= 1;
            st.last_new = Instant::now();
        }
        self.writer.send((f, b, data)).await.map_err(|_| anyhow!("writer stopped"))?;
        Ok(())
    }

    fn missing_list(&self) -> Vec<(u32, Vec<u64>)> {
        let st = self.state.lock().unwrap();
        st.have
            .iter()
            .enumerate()
            .filter_map(|(f, have)| {
                let missing: Vec<u64> = have
                    .iter()
                    .enumerate()
                    .filter(|(b, h)| !**h && !st.pending.contains_key(&(f as u32, *b as u64)))
                    .map(|(b, _)| b as u64)
                    .take(4096)
                    .collect();
                (!missing.is_empty()).then_some((f as u32, missing))
            })
            .collect()
    }
}

/// Receives a batch as a swarm member (after the Decision was sent).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn receive(
    shared: &Arc<Shared>,
    id: u64,
    told: bool,
    conn: &quinn::Connection,
    ctl_tx: &mut quinn::SendStream,
    ctl_rx: &mut quinn::RecvStream,
    files: &Arc<Vec<FileMeta>>,
    state: Arc<BatchState>,
    root: PathBuf,
    progress: Arc<AtomicU64>,
    offsets: &[u64],
    have_files: &[u32],
) -> Result<()> {
    let Msg::Swarm { batch, secret, roster } = read_frame::<Msg>(ctl_rx).await? else {
        bail!("expected swarm details");
    };
    let batch: [u8; 16] = unhex(&batch).and_then(|v| v.try_into().ok()).ok_or_else(|| anyhow!("bad swarm id"))?;
    let secret: [u8; 32] = unhex(&secret).and_then(|v| v.try_into().ok()).ok_or_else(|| anyhow!("bad swarm secret"))?;
    let sizes: Vec<u64> = files.iter().map(|m| m.size).collect();

    // What we already have: whole files, and each file's resumed prefix.
    let mut have: Vec<Vec<bool>> = Vec::new();
    let mut missing = 0u64;
    for (i, &size) in sizes.iter().enumerate() {
        let n = nblocks(size);
        let done_upto = if have_files.contains(&(i as u32)) { n } else { offsets.get(i).copied().unwrap_or(0) / BLOCK };
        let row: Vec<bool> = (0..n).map(|b| b < done_upto).collect();
        missing += row.iter().filter(|h| !**h).count() as u64;
        have.push(row);
    }

    // Destination files (continue partial ones, reserve new ones).
    let mut handles: Vec<Option<Arc<std::fs::File>>> = vec![None; files.len()];
    for (i, meta) in files.iter().enumerate() {
        if have_files.contains(&(i as u32)) {
            continue;
        }
        let fs = match state.get(i).filter(|f| !f.complete) {
            Some(f) => f,
            None => {
                let (final_path, part, _) = transfer::reserve(&root, &crate::proto::sanitize_rel_path(&meta.name)).await?;
                partial::FileState { part, final_path, size: meta.size, verified: 0, complete: false }
            }
        };
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&fs.part)
            .map_err(|e| fatal(format!("Can't write {}: {e}", fs.part.display())))?;
        handles[i] = Some(Arc::new(file));
        state.update(i, |slot| *slot = Some(fs));
    }

    let (control_tx, mut control_rx) = mpsc::unbounded_channel::<Msg>();
    let (writer_tx, writer_rx) = mpsc::channel::<(u32, u64, Bytes)>(4);
    let me = shared.identity.id;
    let recv = Arc::new(SwarmRecv {
        batch,
        secret,
        me,
        roster: roster.into_iter().filter(|r| r.id != me).collect(),
        sizes: sizes.clone(),
        state: Mutex::new(RecvState {
            hashes: have.iter().map(|r| vec![None; r.len()]).collect(),
            have: have.clone(),
            pending: HashMap::new(),
            missing,
            last_new: Instant::now(),
        }),
        writer: writer_tx,
        dec: Mutex::new(Decoder::new()?),
        stats: shared.stats(id),
        links: Mutex::new(HashMap::new()),
        control: control_tx,
        duplicate: AtomicU64::new(0),
        gate: shared.gate(id),
        limits: shared.limits.clone(),
    });
    shared.swarms.lock().unwrap().insert(batch, recv.clone());

    // Writer thread: positional writes, contiguous-prefix resume records,
    // finalizes each file once all its blocks are written.
    let writer = {
        let (state, progress) = (state.clone(), progress.clone());
        let sizes = sizes.clone();
        let mut written = have;
        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut writer_rx = writer_rx;
            let mut prefix: Vec<u64> = written.iter().map(|r| r.iter().take_while(|h| **h).count() as u64).collect();
            let mut left: Vec<u64> = written.iter().map(|r| r.iter().filter(|h| !**h).count() as u64).collect();
            let disk = |e: std::io::Error| fatal(format!("Disk error: {e}"));
            let finish_file = |i: usize, handle: &std::fs::File| -> Result<()> {
                handle.set_len(sizes[i]).map_err(disk)?;
                let mut done = None;
                state.update(i, |slot| {
                    if let Some(f) = slot {
                        f.verified = sizes[i];
                        done = Some(f.clone());
                    }
                });
                if let Some(mut f) = done {
                    transfer::finalize(&mut f)?;
                    state.update(i, |slot| *slot = Some(f));
                }
                Ok(())
            };
            // Files that need nothing (fully resumed) but aren't final yet.
            for i in 0..sizes.len() {
                if left[i] == 0 {
                    if let Some(h) = &handles[i] {
                        finish_file(i, h)?;
                        handles[i] = None;
                    }
                }
            }
            while left.iter().any(|l| *l > 0) {
                let Some((f, b, data)) = writer_rx.blocking_recv() else { bail!("swarm ended early") };
                let (fi, bi) = (f as usize, b as usize);
                if written[fi][bi] {
                    continue;
                }
                let Some(handle) = handles[fi].clone() else { continue };
                write_at(&handle, &data, b * BLOCK).map_err(disk)?;
                written[fi][bi] = true;
                left[fi] -= 1;
                progress.fetch_add(data.len() as u64, Ordering::Relaxed);
                while (prefix[fi] as usize) < written[fi].len() && written[fi][prefix[fi] as usize] {
                    prefix[fi] += 1;
                }
                if sizes[fi] >= 8 << 20 {
                    let verified = (prefix[fi] * BLOCK).min(sizes[fi]);
                    state.update(fi, |slot| {
                        if let Some(s) = slot {
                            s.verified = verified;
                        }
                    });
                }
                if left[fi] == 0 {
                    finish_file(fi, &handle)?;
                    handles[fi] = None;
                }
            }
            Ok(())
        })
    };

    let mut tasks = tokio::task::JoinSet::new();
    // Streams from the sender: block data and hashes.
    {
        let (conn, recv) = (conn.clone(), recv.clone());
        tasks.spawn(async move {
            loop {
                let mut s = conn.accept_uni().await?;
                let mut kind = [0u8; 1];
                s.read_exact(&mut kind).await?;
                let recv = recv.clone();
                match kind[0] {
                    STREAM_DATA => {
                        let conn = conn.clone();
                        tokio::spawn(async move {
                            let r: Result<()> = async {
                                loop {
                                    recv.gate.wait_conn(&conn).await?;
                                    let mut h = [0u8; 49];
                                    s.read_exact(&mut h).await?;
                                    let f = u32::from_le_bytes(h[..4].try_into().unwrap());
                                    let b = u64::from_le_bytes(h[4..12].try_into().unwrap());
                                    let len = u32::from_le_bytes(h[12..16].try_into().unwrap()) as usize;
                                    let hash: [u8; 32] = h[16..48].try_into().unwrap();
                                    let tag = h[48];
                                    if len > wire::MAX_BODY {
                                        bail!("oversized block");
                                    }
                                    let mut data = vec![0u8; len];
                                    limit::read(&mut s, &mut data, &recv.limits.down, Some((&recv.gate, &conn))).await?;
                                    let raw = (recv.sizes.get(f as usize).copied().unwrap_or(0)).saturating_sub(b * BLOCK).min(BLOCK);
                                    recv.stats.add(raw, (len + 49) as u64);
                                    recv.accept(f, b, tag, Bytes::from(data), Some(hash)).await?;
                                }
                            }
                            .await;
                            if let Err(e) = r {
                                crate::debug(&format!("swarm data stream ended: {e:#}"));
                            }
                        });
                    }
                    STREAM_HASHES => {
                        tokio::spawn(async move {
                            let mut h = [0u8; 44];
                            while s.read_exact(&mut h).await.is_ok() {
                                let f = u32::from_le_bytes(h[..4].try_into().unwrap());
                                let b = u64::from_le_bytes(h[4..12].try_into().unwrap());
                                let hash: [u8; 32] = h[12..44].try_into().unwrap();
                                if let Some(data) = recv.on_hash(f, b, hash) {
                                    let _ = recv.accept(f, b, RAW, data, None).await;
                                }
                            }
                        });
                    }
                    _ => {}
                }
            }
            #[allow(unreachable_code)]
            Ok::<(), anyhow::Error>(())
        });
    }
    // Dial the other recipients (they dial us too; the first link wins).
    for peer in recv.roster.clone() {
        let (shared, recv) = (shared.clone(), recv.clone());
        tasks.spawn(async move {
            // Stagger: the lower id dials first, the other a moment later.
            if me > peer.id {
                tokio::time::sleep(Duration::from_millis(1500)).await;
            }
            loop {
                if !links_disabled() && !recv.links.lock().unwrap().contains_key(&peer.id) {
                    if let Err(e) = dial(&shared, &recv, &peer).await {
                        crate::debug(&format!("swarm link to {} failed: {e:#}", peer.id));
                    }
                }
                tokio::time::sleep(Duration::from_secs(4)).await;
            }
            #[allow(unreachable_code)]
            Ok::<(), anyhow::Error>(())
        });
    }
    // Progress reports and the endgame ("I still need these").
    let all_assigned = Arc::new(Notify::new());
    {
        let (recv, progress, all_assigned) = (recv.clone(), progress.clone(), all_assigned.clone());
        tasks.spawn(async move {
            let mut reported = u64::MAX;
            let mut endgame = false;
            let mut last_need = Instant::now();
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(500)) => {}
                    _ = all_assigned.notified() => endgame = true,
                }
                let done = progress.load(Ordering::Relaxed);
                if done != reported {
                    reported = done;
                    let _ = recv.control.send(Msg::SwarmHave { done });
                }
                let idle = recv.state.lock().unwrap().last_new.elapsed() > Duration::from_secs(2);
                if endgame && idle && last_need.elapsed() > Duration::from_secs(2) && !recv.gate.paused() {
                    last_need = Instant::now();
                    for (file, blocks) in recv.missing_list() {
                        let _ = recv.control.send(Msg::Need { file, blocks });
                    }
                }
            }
            #[allow(unreachable_code)]
            Ok::<(), anyhow::Error>(())
        });
    }

    // Our pauses go to the sender with the other control messages.
    {
        let recv = recv.clone();
        tasks.spawn(async move {
            let mut rx = recv.gate.subscribe();
            let mut told = told;
            loop {
                let now = recv.gate.local();
                if now != told {
                    told = now;
                    let _ = recv.control.send(Msg::Pause { paused: now });
                }
                if rx.changed().await.is_err() {
                    break;
                }
            }
            Ok::<(), anyhow::Error>(())
        });
    }

    // Control stream: ours out, the sender's in (one reader for the whole
    // batch, so a frame is never cut off halfway).
    let reader = async {
        loop {
            match read_frame::<Msg>(ctl_rx).await? {
                Msg::AllAssigned => all_assigned.notify_one(),
                Msg::Pause { paused } => shared.set_remote_pause(id, paused),
                _ => {}
            }
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    };
    tokio::pin!(reader);
    let result: Result<()> = async {
        let mut writer = writer;
        loop {
            tokio::select! {
                m = control_rx.recv() => {
                    if let Some(m) = m {
                        write_frame(ctl_tx, &m).await?;
                    }
                }
                r = &mut reader => return r,
                w = &mut writer => return w?,
                r = tasks.join_next(), if !tasks.is_empty() => {
                    if let Some(Ok(Err(e))) = r {
                        return Err(e);
                    }
                }
            }
        }
    }
    .await;
    tasks.abort_all();
    shared.swarms.lock().unwrap().remove(&batch);
    recv.links.lock().unwrap().clear();
    let dup = recv.duplicate.load(Ordering::Relaxed);
    crate::debug(&format!("swarm batch done; {dup} duplicate bytes"));
    let _ = conn;
    result
}

// ------------------------------------------------------- recipient links

async fn dial(shared: &Arc<Shared>, recv: &Arc<SwarmRecv>, peer: &RosterEntry) -> Result<()> {
    let target = Peer {
        id: peer.id,
        name: peer.id.code(),
        addr: *peer.addrs.first().ok_or_else(|| anyhow!("no address"))?,
        account: None,
        account_key: None,
        alt: peer.addrs.iter().skip(1).copied().collect(),
        remote: peer.addrs.len() > 1,
        nat: None,
    };
    let conn = tokio::time::timeout(Duration::from_secs(8), shared.connect_peer(&target)).await??;
    let (mut tx, mut rx) = conn.open_bi().await?;
    write_frame(&mut tx, &Msg::SwarmHello { batch: hex(&recv.batch), proof: proof(&recv.secret, recv.me, peer.id) }).await?;
    match read_frame::<Msg>(&mut rx).await? {
        Msg::SwarmWelcome { ok: true } => {}
        _ => {
            conn.close(0u32.into(), b"not now");
            bail!("{} isn't ready for this batch yet", peer.id);
        }
    }
    let recv = recv.clone();
    let peer_id = peer.id;
    tokio::spawn(async move { run_link(recv, conn, peer_id, tx, rx).await });
    Ok(())
}

/// An incoming link from another recipient of one of our batches.
pub(crate) async fn handle_hello(
    shared: Arc<Shared>,
    conn: quinn::Connection,
    peer: DeviceId,
    mut tx: quinn::SendStream,
    rx: quinn::RecvStream,
    batch: String,
    their_proof: String,
) {
    let recv = unhex(&batch)
        .and_then(|b| <[u8; 16]>::try_from(b).ok())
        .and_then(|b| shared.swarms.lock().unwrap().get(&b).cloned());
    let ok = !links_disabled() && recv.as_ref().is_some_and(|r| {
        r.roster.iter().any(|e| e.id == peer)
            && their_proof == proof(&r.secret, peer, r.me)
            && !r.links.lock().unwrap().contains_key(&peer)
    });
    if write_frame(&mut tx, &Msg::SwarmWelcome { ok }).await.is_err() || !ok {
        let _ = tx.finish();
        let _ = tokio::time::timeout(Duration::from_secs(2), conn.closed()).await;
        return;
    }
    run_link(recv.unwrap(), conn, peer, tx, rx).await;
}

/// Pushes our sender-given blocks to `peer` and takes theirs.
async fn run_link(recv: Arc<SwarmRecv>, conn: quinn::Connection, peer: DeviceId, _tx: quinn::SendStream, _rx: quinn::RecvStream) {
    let (push_tx, mut push_rx) = mpsc::channel::<(u32, u64, u8, Bytes)>(PUSH_QUEUE);
    {
        let mut links = recv.links.lock().unwrap();
        if links.contains_key(&peer) {
            conn.close(0u32.into(), b"duplicate");
            return;
        }
        links.insert(peer, push_tx);
    }
    recv.report_links();
    let out = {
        let (conn, recv_out) = (conn.clone(), recv.clone());
        tokio::spawn(async move {
            let mut s = conn.open_uni().await?;
            s.write_all(&[STREAM_PUSH]).await?;
            let limits = recv_out.limits.clone();
            while let Some((f, b, tag, data)) = push_rx.recv().await {
                let mut h = Vec::with_capacity(17);
                h.extend_from_slice(&f.to_le_bytes());
                h.extend_from_slice(&b.to_le_bytes());
                h.extend_from_slice(&(data.len() as u32).to_le_bytes());
                h.push(tag);
                s.write_all(&h).await?;
                limit::write(&mut s, data, &[&limits.up], None).await?;
            }
            Ok::<(), anyhow::Error>(())
        })
    };
    let inbound: Result<()> = async {
        let mut s = conn.accept_uni().await?;
        let mut kind = [0u8; 1];
        s.read_exact(&mut kind).await?;
        if kind[0] != STREAM_PUSH {
            bail!("unexpected stream");
        }
        loop {
            recv.gate.wait_conn(&conn).await?;
            let mut h = [0u8; 17];
            s.read_exact(&mut h).await?;
            let f = u32::from_le_bytes(h[..4].try_into().unwrap());
            let b = u64::from_le_bytes(h[4..12].try_into().unwrap());
            let len = u32::from_le_bytes(h[12..16].try_into().unwrap()) as usize;
            if len > wire::MAX_BODY {
                bail!("oversized block");
            }
            let mut data = vec![0u8; len];
            limit::read(&mut s, &mut data, &recv.limits.down, Some((&recv.gate, &conn))).await?;
            let raw = (recv.sizes.get(f as usize).copied().unwrap_or(0)).saturating_sub(b * BLOCK).min(BLOCK);
            recv.stats.add(raw, (len + 17) as u64);
            recv.accept(f, b, h[16], Bytes::from(data), None).await?;
        }
    }
    .await;
    if let Err(e) = inbound {
        crate::debug(&format!("swarm link with {peer} closed: {e:#}"));
    }
    out.abort();
    recv.links.lock().unwrap().remove(&peer);
    recv.report_links();
    conn.close(0u32.into(), b"done");
}
