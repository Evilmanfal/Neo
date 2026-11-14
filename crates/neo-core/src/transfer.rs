//! Sending and receiving.
//!
//! - Files are streamed in 1 MiB blocks, each followed by its BLAKE3 hash, so
//!   memory stays flat whatever the size and every block is verified.
//! - Disk I/O and hashing run on blocking threads, pipelined a few blocks
//!   ahead of the network, so reading, hashing, encryption and writing
//!   overlap.
//! - Up to `PARALLEL_FILES` files are in flight per recipient, which helps
//!   folders full of small files.
//! - Each recipient has its own connection: a slow peer never holds up a fast
//!   one, and the OS page cache serves the file from disk once for all.
//! - If a connection drops, the sender keeps retrying and the receiver
//!   continues each file from its last verified block (see `partial`).

use std::fmt;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use bytes::Bytes;
use quinn::VarInt;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::delta::{self, BaseFile, Hash16};
use crate::limit::{self, Gate, Limits};
use crate::wire::{self, Decoder, Encoder, Seg, PIECES, RAW};
use crate::partial;
use crate::proto::{read_frame, sanitize_rel_path, write_frame, FileMeta, Msg, BLOCK};
use crate::{Direction, Interrupted, Peer, Shared, TransferInfo, TransferState, WireStats};

pub const CANCELLED: VarInt = VarInt::from_u32(1);
pub(crate) const DONE: VarInt = VarInt::from_u32(0);
pub(crate) const FAILED: VarInt = VarInt::from_u32(2);
const SUPERSEDED: VarInt = VarInt::from_u32(3);
pub(crate) const DECISION_TIMEOUT: Duration = Duration::from_secs(600);
/// Files in flight per recipient. Measured: 6 or 12 for folders of small
/// files was no faster (12 was ~20% slower); the receiver's file creation,
/// not the network, sets the pace there.
const PARALLEL_FILES: usize = 3;
const PIPELINE_BLOCKS: usize = 2;
/// Keep retrying a dropped send this long after the last attempt that made progress.
const RETRY_FOR: Duration = Duration::from_secs(300);
/// Files smaller than this restart from zero rather than mid-file
/// (their completion is still remembered).
const RESUMABLE_MIN: u64 = 8 << 20;

/// An error retrying can't fix (file changed, integrity failure, disk error).
#[derive(Debug)]
pub(crate) struct Fatal(pub String);

impl fmt::Display for Fatal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Fatal {}

/// The receiver's old copy of a file didn't match its chunk index (edited
/// without its timestamp changing). The index is dropped and the transfer
/// reconnects; the file then continues without delta.
#[derive(Debug)]
pub(crate) struct BaseChanged;

impl fmt::Display for BaseChanged {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the old copy changed")
    }
}

impl std::error::Error for BaseChanged {}

/// Close code asking the sender to reconnect right away (see `BaseChanged`).
pub(crate) const RETRY: VarInt = VarInt::from_u32(6);

pub(crate) fn fatal(msg: impl Into<String>) -> anyhow::Error {
    Fatal(msg.into()).into()
}

pub(crate) enum Outcome {
    Cancelled,
    Failed(String),
    Retry(String),
}

pub(crate) fn classify(e: &anyhow::Error) -> Outcome {
    for c in e.chain() {
        if let Some(f) = c.downcast_ref::<Fatal>() {
            return Outcome::Failed(f.0.clone());
        }
        if c.is::<BaseChanged>() {
            return Outcome::Retry("The old copy changed; continuing without it".into());
        }
        let ce = if let Some(x) = c.downcast_ref::<quinn::ConnectionError>() {
            Some(x)
        } else if let Some(quinn::WriteError::ConnectionLost(x)) = c.downcast_ref::<quinn::WriteError>() {
            Some(x)
        } else if let Some(quinn::ReadError::ConnectionLost(x)) = c.downcast_ref::<quinn::ReadError>() {
            Some(x)
        } else if let Some(quinn::ReadExactError::ReadError(quinn::ReadError::ConnectionLost(x))) =
            c.downcast_ref::<quinn::ReadExactError>()
        {
            Some(x)
        } else {
            None
        };
        if let Some(ce) = ce {
            return match ce {
                quinn::ConnectionError::ApplicationClosed(a) if a.error_code == CANCELLED => Outcome::Cancelled,
                quinn::ConnectionError::ApplicationClosed(a) if a.error_code == FAILED => {
                    Outcome::Failed(String::from_utf8_lossy(&a.reason).into_owned())
                }
                quinn::ConnectionError::TimedOut => Outcome::Retry("Connection timed out".into()),
                _ => Outcome::Retry("Connection lost".into()),
            };
        }
        if c.downcast_ref::<quinn::ConnectError>().is_some() {
            return Outcome::Retry("Couldn't connect".into());
        }
    }
    Outcome::Failed(format!("{e:#}"))
}

pub(crate) fn report(shared: &Shared, id: u64, result: Result<TransferState>) {
    if shared.is_gone(id) {
        return; // cancelled locally; state already reported
    }
    let state = match result {
        Ok(s) => s,
        Err(e) => match classify(&e) {
            Outcome::Cancelled => TransferState::Cancelled,
            Outcome::Failed(m) | Outcome::Retry(m) => TransferState::Failed(m),
        },
    };
    shared.set_state(id, state);
}

// ------------------------------------------------------------------ sending

/// Sender-side fingerprint so the receiver can tell "same file, resume" from
/// "different file that happens to share a name".
fn file_key(path: &Path, meta: &std::fs::Metadata) -> String {
    let mtime = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_nanos()).unwrap_or(0);
    let mut h = blake3::Hasher::new();
    h.update(path.to_string_lossy().as_bytes());
    h.update(&meta.len().to_le_bytes());
    h.update(&mtime.to_le_bytes());
    h.finalize().to_hex()[..32].to_string()
}

/// Expands files and folders into (absolute path, wire metadata).
pub(crate) fn expand(paths: &[PathBuf]) -> Result<Vec<(PathBuf, FileMeta)>> {
    fn walk(dir: &Path, rel: &str, out: &mut Vec<(PathBuf, FileMeta)>) -> Result<()> {
        let mut entries: Vec<_> = std::fs::read_dir(dir)?.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name().to_string_lossy().into_owned();
            let rel = format!("{rel}/{name}");
            let ty = e.file_type()?;
            if ty.is_dir() {
                walk(&e.path(), &rel, out)?;
            } else if ty.is_file() {
                let meta = e.metadata()?;
                let key = file_key(&e.path(), &meta);
                out.push((e.path(), FileMeta { name: rel, size: meta.len(), key }));
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    for p in paths {
        let meta = std::fs::metadata(p).with_context(|| format!("reading {}", p.display()))?;
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into());
        if meta.is_dir() {
            walk(p, &name, &mut out)?;
        } else {
            let key = file_key(p, &meta);
            out.push((p.clone(), FileMeta { name, size: meta.len(), key }));
        }
    }
    if out.is_empty() {
        bail!("nothing to send");
    }
    Ok(out)
}

pub async fn send_job(shared: Arc<Shared>, paths: Vec<PathBuf>, peers: Vec<Peer>) {
    let files = match tokio::task::spawn_blocking(move || expand(&paths)).await {
        Ok(Ok(f)) => Arc::new(f),
        Ok(Err(e)) => return shared.emit(crate::Event::Notice(format!("Can't send: {e:#}"))),
        Err(e) => return shared.emit(crate::Event::Notice(format!("Can't send: {e}"))),
    };
    let metas: Arc<Vec<FileMeta>> = Arc::new(files.iter().map(|(_, m)| m.clone()).collect());
    let total: u64 = metas.iter().map(|m| m.size).sum();
    if peers.len() >= 2 && crate::swarm::enabled() {
        // Several recipients: they pass blocks on to each other.
        return crate::swarm::send_job(shared, files, metas, total, peers).await;
    }
    let job = shared.next_id();
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
        let (shared, files, metas) = (shared.clone(), files.clone(), metas.clone());
        tokio::spawn(async move {
            let result = send_with_retry(&shared, id, &peer, files, metas, total, progress).await;
            report(&shared, id, result);
        });
    }
}

async fn send_with_retry(
    shared: &Shared,
    id: u64,
    peer: &Peer,
    files: Arc<Vec<(PathBuf, FileMeta)>>,
    metas: Arc<Vec<FileMeta>>,
    total: u64,
    progress: Arc<AtomicU64>,
) -> Result<TransferState> {
    let mut target = peer.clone();
    let mut accepted = false;
    let mut backoff = Duration::from_secs(1);
    let mut deadline = Instant::now() + RETRY_FOR;
    loop {
        hold_while_paused(shared, id).await;
        shared.set_remote_pause(id, false); // they re-send theirs on the new connection
        let before = progress.load(Ordering::Relaxed);
        let result = send_attempt(shared, id, &target, &files, &metas, total, &progress, &mut accepted).await;
        let err = match result {
            Ok(state) => return Ok(state),
            Err(e) => e,
        };
        // A paused transfer isn't stuck: keep the retry window open.
        if progress.load(Ordering::Relaxed) > before || shared.gate(id).paused() {
            deadline = Instant::now() + RETRY_FOR;
            backoff = Duration::from_secs(1);
        }
        let retry = matches!(classify(&err), Outcome::Retry(_)) && accepted && Instant::now() < deadline;
        if !retry || shared.is_gone(id) {
            return Err(err);
        }
        shared.set_state(id, TransferState::Interrupted);
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(15));
        if shared.is_gone(id) {
            return Err(err);
        }
        // Follow the device if its addresses changed (LAN or internet).
        if let Some(p) = shared.peer(peer.id) {
            target = p;
        }
    }
}

/// Paused here and disconnected: no point reconnecting until resumed (or
/// cancelled).
pub(crate) async fn hold_while_paused(shared: &Shared, id: u64) {
    let gate = shared.gate(id);
    let mut rx = gate.subscribe();
    while gate.local() && !shared.is_gone(id) {
        if rx.changed().await.is_err() {
            break;
        }
    }
}

/// Runs `work` while relaying pause changes over the control stream: theirs
/// in (`Pause` frames), ours out. `told` is what the handshake already told
/// the other side. Returns `work`'s result and, if `until_reply`, the next
/// non-`Pause` frame after it (e.g. `Complete`).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn with_pause_relay<T>(
    shared: &Shared,
    id: u64,
    told: bool,
    ctl_tx: &mut quinn::SendStream,
    ctl_rx: &mut quinn::RecvStream,
    work: impl std::future::Future<Output = Result<T>>,
    until_reply: bool,
) -> Result<(T, Option<Msg>)> {
    let gate = shared.gate(id);
    let reader = async {
        loop {
            match read_frame::<Msg>(ctl_rx).await? {
                Msg::Pause { paused } => shared.set_remote_pause(id, paused),
                other => return Ok::<Msg, anyhow::Error>(other),
            }
        }
    };
    let writer = async {
        let mut rx = gate.subscribe();
        let mut told = told;
        loop {
            // Also catches a change made before we got here.
            let now = gate.local();
            if now != told {
                told = now;
                write_frame(ctl_tx, &Msg::Pause { paused: now }).await?;
            }
            if rx.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    };
    tokio::pin!(reader, writer, work);
    let out = tokio::select! {
        r = &mut work => r?,
        m = &mut reader => bail!("unexpected message {:?}", m?),
        e = &mut writer => return Err(e.err().unwrap_or_else(|| anyhow!("control stream closed"))),
    };
    if !until_reply {
        return Ok((out, None));
    }
    let reply = tokio::select! {
        m = &mut reader => m?,
        e = &mut writer => return Err(e.err().unwrap_or_else(|| anyhow!("control stream closed"))),
    };
    Ok((out, Some(reply)))
}

#[allow(clippy::too_many_arguments)]
async fn send_attempt(
    shared: &Shared,
    id: u64,
    peer: &Peer,
    files: &Arc<Vec<(PathBuf, FileMeta)>>,
    metas: &Arc<Vec<FileMeta>>,
    total: u64,
    progress: &Arc<AtomicU64>,
    accepted: &mut bool,
) -> Result<TransferState> {
    if !*accepted {
        shared.set_state(id, TransferState::Connecting); // retries stay "reconnecting"
    }
    let conn = shared.connect_peer(peer).await?;
    shared.attach_conn(id, &conn);
    let (mut ctl_tx, mut ctl_rx) = conn.open_bi().await?;
    let gate = shared.gate(id);
    let offer = Msg::Offer {
        sender_name: shared.config().name,
        files: (**metas).clone(),
        total,
        attest: Some(shared.attest()),
        swarm: false,
        paused: gate.local(),
    };
    let offered_paused = matches!(offer, Msg::Offer { paused: true, .. });
    write_frame(&mut ctl_tx, &offer).await?;
    if !*accepted {
        shared.set_state(id, TransferState::Waiting);
    }
    let (offsets, have, bases) = match read_frame::<Msg>(&mut ctl_rx).await? {
        Msg::Decision { accept: true, offsets, have, paused, bases } => {
            shared.set_remote_pause(id, paused);
            (offsets, have, bases)
        }
        Msg::Decision { accept: false, .. } => {
            conn.close(DONE, b"declined");
            return Ok(TransferState::Declined);
        }
        other => bail!("unexpected reply {other:?}"),
    };
    *accepted = true;

    // Work out what still has to go, and where each file starts.
    let mut todo = Vec::new();
    let mut already = 0u64;
    for (i, (path, meta)) in files.iter().enumerate() {
        if have.contains(&(i as u32)) {
            already += meta.size;
            continue;
        }
        let start = offsets.get(i).copied().unwrap_or(0);
        if start > meta.size || (start % BLOCK != 0) {
            bail!("recipient asked for an invalid resume point");
        }
        already += start;
        // Their old version's chunks, if they have one (whole files only).
        let known = bases.get(i).filter(|_| start == 0).and_then(|b| delta::from_wire(b)).map(Arc::new);
        todo.push(SendFile { index: i as u32, path: path.clone(), name: meta.name.clone(), size: meta.size, start, known });
    }
    progress.store(already, Ordering::Relaxed);
    shared.set_state(id, TransferState::Active);

    let permits = Arc::new(tokio::sync::Semaphore::new(PARALLEL_FILES));
    let stats = shared.stats(id);
    let mut set = JoinSet::new();
    for f in todo {
        let (conn, progress, permits) = (conn.clone(), progress.clone(), permits.clone());
        let (gate, limits, stats) = (gate.clone(), shared.limits.clone(), stats.clone());
        set.spawn(async move {
            let _permit = permits.acquire_owned().await?;
            send_file(&conn, f, &progress, &gate, &limits, &stats).await
        });
    }
    let files_done = async {
        while let Some(r) = set.join_next().await {
            if let Err(e) = r.map_err(anyhow::Error::from).and_then(|r| r) {
                if let Some(f) = e.downcast_ref::<Fatal>() {
                    conn.close(FAILED, f.0.as_bytes());
                }
                return Err(e);
            }
        }
        Ok(())
    };
    let ((), reply) = with_pause_relay(shared, id, offered_paused, &mut ctl_tx, &mut ctl_rx, files_done, true).await?;

    match reply.ok_or_else(|| anyhow!("no reply"))? {
        Msg::Complete { error: None } => {
            conn.close(DONE, b"done");
            Ok(TransferState::Done { saved: Vec::new() })
        }
        Msg::Complete { error: Some(e) } => Err(fatal(e)),
        other => bail!("unexpected reply {other:?}"),
    }
}

/// One file still to send.
struct SendFile {
    index: u32,
    path: PathBuf,
    name: String,
    size: u64,
    start: u64,
    /// The receiver's chunks of its old version (send only what changed).
    known: Option<Arc<std::collections::HashSet<Hash16>>>,
}

/// A block ready for the wire: (tag, payload, hash of the original, original length).
type Block = (u8, Bytes, [u8; 32], u64);

async fn send_file(
    conn: &quinn::Connection,
    f: SendFile,
    progress: &AtomicU64,
    gate: &Gate,
    limits: &Limits,
    stats: &WireStats,
) -> Result<()> {
    let (size, start) = (f.size, f.start);
    let mut stream = conn.open_uni().await?;
    let mut header = [0u8; 12];
    header[..4].copy_from_slice(&f.index.to_le_bytes());
    header[4..].copy_from_slice(&start.to_le_bytes());
    stream.write_all(&header).await?;

    // Reader thread: reads, hashes and compresses (or splits against the
    // receiver's old version) a few blocks ahead of the network.
    let (tx, mut rx) = mpsc::channel::<Result<Block>>(PIPELINE_BLOCKS);
    let reader = {
        tokio::task::spawn_blocking(move || {
            let path = f.path;
            let changed = || fatal(format!("{} changed while sending", path.display()));
            let mut enc = Encoder::new(&f.name);
            // Hands a block to the network, telling the encoder how long that waited.
            let hand = |enc: &mut Encoder, block: Block| -> bool {
                let n = block.3;
                let t = std::time::Instant::now();
                let ok = tx.blocking_send(Ok(block)).is_ok();
                enc.sent(n, t.elapsed().as_secs_f64());
                ok
            };
            let run = || -> Result<()> {
                let mut file = std::fs::File::open(&path).map_err(|e| fatal(format!("Can't open {}: {e}", path.display())))?;
                if let Some(known) = f.known.filter(|_| start == 0) {
                    let mut sent = 0u64;
                    delta::split((&mut file).take(size), &known, |block, segs| {
                        let hash = *blake3::hash(block).as_bytes();
                        let (tag, body) = if segs.iter().any(|s| matches!(s, Seg::Ref { .. })) {
                            enc.pieces(block, segs)
                        } else {
                            enc.block(Bytes::copy_from_slice(block))
                        };
                        sent += block.len() as u64;
                        Ok(hand(&mut enc, (tag, body, hash, block.len() as u64)))
                    })?;
                    if sent != size {
                        return Err(changed());
                    }
                    return Ok(());
                }
                file.seek(SeekFrom::Start(start))?;
                let mut pos = start;
                while pos < size {
                    let n = (size - pos).min(BLOCK) as usize;
                    let mut buf = vec![0u8; n];
                    file.read_exact(&mut buf).map_err(|_| changed())?;
                    let hash = *blake3::hash(&buf).as_bytes();
                    let (tag, body) = enc.block(Bytes::from(buf));
                    if !hand(&mut enc, (tag, body, hash, n as u64)) {
                        return Ok(()); // network side gave up
                    }
                    pos += n as u64;
                }
                Ok(())
            };
            if let Err(e) = run() {
                let _ = tx.blocking_send(Err(e));
            }
        })
    };

    while let Some(item) = rx.recv().await {
        let (tag, body, hash, n) = item?;
        gate.wait_conn(conn).await?;
        let mut head = [tag, 0, 0, 0, 0];
        let head = if tag == RAW {
            &head[..1]
        } else {
            head[1..].copy_from_slice(&(body.len() as u32).to_le_bytes());
            &head[..]
        };
        stream.write_all(head).await?;
        let on_wire = (head.len() + body.len() + 32) as u64;
        limit::write(&mut stream, body, &[&limits.bench, &limits.up], Some((gate, conn))).await?;
        stream.write_all(&hash).await?;
        progress.fetch_add(n, Ordering::Relaxed);
        stats.add(n, on_wire);
        if tag == PIECES {
            stats.delta.store(true, Ordering::Relaxed);
        }
    }
    let _ = reader.await;
    stream.finish()?;
    Ok(())
}

// ---------------------------------------------------------------- receiving

/// Live resume state for one incoming batch, saved by a background task
/// whenever it changes (at most twice a second).
pub(crate) struct BatchState {
    batch: std::sync::Mutex<partial::Batch>,
    dirty: std::sync::atomic::AtomicBool,
    pdir: PathBuf,
    fp: partial::Fingerprint,
}

impl BatchState {
    pub(crate) fn update(&self, index: usize, f: impl FnOnce(&mut Option<partial::FileState>)) {
        f(&mut self.batch.lock().unwrap().files[index]);
        self.dirty.store(true, Ordering::Release);
    }

    pub(crate) fn get(&self, index: usize) -> Option<partial::FileState> {
        self.batch.lock().unwrap().files[index].clone()
    }

    fn flush(&self) {
        if self.dirty.swap(false, Ordering::AcqRel) {
            let snapshot = self.batch.lock().unwrap().clone();
            let _ = partial::save(&self.pdir, &self.fp, &snapshot);
        }
    }
}

/// What an incoming `Offer` said.
pub(crate) struct Offer {
    pub sender_name: String,
    pub files: Vec<FileMeta>,
    pub total: u64,
    pub swarm: bool,
    /// The sender has it paused.
    pub paused: bool,
}

/// Handles an incoming `Offer` (the router already read it).
pub(crate) async fn receive_offer(
    shared: Arc<Shared>,
    conn: quinn::Connection,
    peer_id: crate::DeviceId,
    peer_account: Option<crate::AccountId>,
    ctl_tx: quinn::SendStream,
    ctl_rx: quinn::RecvStream,
    offer: Offer,
) {
    let Offer { sender_name, files, total, swarm, paused } = offer;
    if files.is_empty() || files.iter().map(|f| f.size).sum::<u64>() != total {
        return;
    }
    let fp = partial::fingerprint(peer_id, &files);

    // The same sender reconnecting for the same batch: retire the stale
    // connection and let its writers finish before touching the files.
    let (done_tx, done_rx) = tokio::sync::watch::channel(false);
    let stale = shared.receiving.lock().unwrap().insert(fp, (conn.clone(), done_rx));
    if let Some((old, mut old_done)) = stale {
        old.close(SUPERSEDED, b"reconnected");
        let _ = tokio::time::timeout(Duration::from_secs(10), old_done.wait_for(|d| *d)).await;
    }
    let offer = Offer { sender_name, files, total, swarm, paused };
    receive_batch(&shared, conn.clone(), peer_id, peer_account, offer, ctl_tx, ctl_rx, fp).await;
    {
        let mut map = shared.receiving.lock().unwrap();
        if map.get(&fp).is_some_and(|(c, _)| c.stable_id() == conn.stable_id()) {
            map.remove(&fp);
        }
    }
    let _ = done_tx.send(true);
}

#[allow(clippy::too_many_arguments)]
async fn receive_batch(
    shared: &Arc<Shared>,
    conn: quinn::Connection,
    peer_id: crate::DeviceId,
    peer_account: Option<crate::AccountId>,
    offer: Offer,
    ctl_tx: quinn::SendStream,
    ctl_rx: quinn::RecvStream,
    fp: partial::Fingerprint,
) {
    let Offer { sender_name, files, total, swarm, paused } = offer;
    let reused = shared.interrupted.lock().unwrap().remove(&fp);
    let id = reused.as_ref().map(|r| r.id).unwrap_or_else(|| shared.next_id());
    let files = Arc::new(files);
    let info = TransferInfo {
        id,
        job: id,
        direction: Direction::Incoming,
        peer_id,
        peer_name: sender_name.chars().take(64).collect(),
        peer_account,
        files: files.clone(),
        total,
    };
    let progress = shared.register(info, reused.is_none());
    shared.attach_conn(id, &conn);
    shared.set_remote_pause(id, paused);

    let mut accepted = false;
    let trusted = shared.trusts(peer_account);
    let result = receive_files(shared, id, &conn, ctl_tx, ctl_rx, &files, fp, &progress, trusted, swarm, &mut accepted).await;
    if let Err(e) = &result {
        match classify(e) {
            Outcome::Retry(_) if accepted && !shared.is_gone(id) => {
                if e.chain().any(|c| c.is::<BaseChanged>()) {
                    conn.close(RETRY, b"old copy changed");
                }
                // Keep the card; the sender will reconnect and we continue.
                shared.interrupted.lock().unwrap().insert(fp, Interrupted { id });
                shared.set_remote_pause(id, false);
                shared.set_state(id, TransferState::Interrupted);
                shared.active.lock().unwrap().remove(&id);
                return;
            }
            Outcome::Failed(msg) => conn.close(FAILED, msg.as_bytes()),
            _ => {}
        }
    }
    report(shared, id, result);
}

#[allow(clippy::too_many_arguments)]
async fn receive_files(
    shared: &Arc<Shared>,
    id: u64,
    conn: &quinn::Connection,
    mut ctl_tx: quinn::SendStream,
    mut ctl_rx: quinn::RecvStream,
    files: &Arc<Vec<FileMeta>>,
    fp: partial::Fingerprint,
    progress: &Arc<AtomicU64>,
    trusted: bool,
    swarm: bool,
    accepted: &mut bool,
) -> Result<TransferState> {
    let cfg = shared.config();
    let pdir = cfg.partials_dir();

    // What do we already have from earlier attempts?
    let mut batch = {
        let (pdir, files) = (pdir.clone(), files.clone());
        tokio::task::spawn_blocking(move || partial::load(&pdir, &fp, &files)).await?
    }
    .unwrap_or_else(|| partial::Batch { files: vec![None; files.len()], saved_at: 0 });
    let resuming = batch.files.iter().flatten().any(|f| !f.complete);

    // Your own devices, contacts you marked "accept automatically", and
    // batches you already accepted once don't ask again.
    let accept = if cfg.auto_accept || resuming || trusted {
        true
    } else {
        shared.set_state(id, TransferState::Waiting);
        let (tx, rx) = tokio::sync::oneshot::channel();
        shared.pending.lock().unwrap().insert(id, tx);
        tokio::select! {
            r = rx => r.unwrap_or(false),
            e = conn.closed() => return Err(e.into()),
            _ = tokio::time::sleep(DECISION_TIMEOUT) => false,
        }
    };
    if !accept {
        write_frame(&mut ctl_tx, &Msg::Decision { accept: false, offsets: vec![], have: vec![], paused: false, bases: vec![] }).await?;
        let _ = ctl_tx.finish();
        let _ = tokio::time::timeout(Duration::from_secs(3), conn.closed()).await;
        return Ok(TransferState::Declined);
    }

    let mut offsets = vec![0u64; files.len()];
    let mut have = Vec::new();
    let mut already = 0u64;
    for (i, slot) in batch.files.iter_mut().enumerate() {
        let Some(f) = slot else { continue };
        if !f.complete && f.verified == f.size && finalize(f).is_err() {
            *slot = None;
            continue;
        }
        if f.complete {
            have.push(i as u32);
            already += f.size;
        } else {
            offsets[i] = f.verified - f.verified % BLOCK;
            already += offsets[i];
        }
    }
    // Old versions of the files we'll get from the start: their chunks let the
    // sender skip what we already have (direct sends only; see `delta`).
    let root = cfg.downloads.clone();
    let cache_dir = cfg.data_dir.join("chunks");
    let (bases, base_files) = if swarm || !delta_enabled() {
        (Vec::new(), Arc::new(vec![None; files.len()]))
    } else {
        let (files, offsets, have, root, cache_dir) = (files.clone(), offsets.clone(), have.clone(), root.clone(), cache_dir.clone());
        tokio::task::spawn_blocking(move || find_bases(&files, &offsets, &have, &root, &cache_dir)).await?
    };
    let gate = shared.gate(id);
    let decision = Msg::Decision { accept: true, offsets: offsets.clone(), have: have.clone(), paused: gate.local(), bases };
    write_frame(&mut ctl_tx, &decision).await?;
    let told = matches!(decision, Msg::Decision { paused: true, .. });
    *accepted = true;
    progress.store(already, Ordering::Relaxed);
    shared.set_state(id, TransferState::Active);

    tokio::fs::create_dir_all(&root).await.map_err(|e| fatal(format!("Can't create {}: {e}", root.display())))?;

    let state = Arc::new(BatchState { batch: std::sync::Mutex::new(batch), dirty: true.into(), pdir: pdir.clone(), fp });
    let saver = {
        let state = state.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(500)).await;
                let s = state.clone();
                let _ = tokio::task::spawn_blocking(move || s.flush()).await;
            }
        })
    };

    let expected = if swarm { 0 } else { files.len() - have.len() };
    let mut set: JoinSet<Result<usize>> = JoinSet::new();
    let mut started = 0usize;
    let mut finished = 0usize;
    let result: Result<()> = async {
        if swarm {
            return crate::swarm::receive(
                shared, id, told, conn, &mut ctl_tx, &mut ctl_rx, files, state.clone(), root.clone(), progress.clone(), &offsets,
                &have,
            )
            .await;
        }
        let stats = shared.stats(id);
        let files_done = async {
            while finished < expected {
                tokio::select! {
                    s = conn.accept_uni(), if started < expected => {
                        let stream = s?;
                        started += 1;
                        let ctx = FileCtx {
                            files: files.clone(),
                            state: state.clone(),
                            root: root.clone(),
                            progress: progress.clone(),
                            conn: conn.clone(),
                            gate: gate.clone(),
                            limits: shared.limits.clone(),
                            stats: stats.clone(),
                            bases: base_files.clone(),
                            cache_dir: cache_dir.clone(),
                        };
                        set.spawn(async move { receive_file(stream, ctx).await });
                    }
                    Some(r) = set.join_next(), if !set.is_empty() => {
                        r??;
                        finished += 1;
                    }
                }
            }
            Ok(())
        };
        with_pause_relay(shared, id, told, &mut ctl_tx, &mut ctl_rx, files_done, false).await.map(|_| ())
    }
    .await;
    if let Err(e) = &result {
        // Stop the other files promptly, then let every writer finish so no
        // one is still writing when a reconnect takes over these files.
        if let Outcome::Failed(msg) = classify(e) {
            conn.close(FAILED, msg.as_bytes());
        }
    }
    while set.join_next().await.is_some() {}
    saver.abort();

    if let Err(e) = result {
        let s = state.clone();
        let _ = tokio::task::spawn_blocking(move || s.flush()).await;
        return Err(e);
    }
    partial::remove(&pdir, &fp);
    write_frame(&mut ctl_tx, &Msg::Complete { error: None }).await?;
    let _ = ctl_tx.finish();
    let _ = tokio::time::timeout(Duration::from_secs(5), conn.closed()).await;
    let saved = state.batch.lock().unwrap().files.iter().flatten().map(|f| f.final_path.clone()).collect();
    Ok(TransferState::Done { saved })
}

struct FileCtx {
    files: Arc<Vec<FileMeta>>,
    state: Arc<BatchState>,
    root: PathBuf,
    progress: Arc<AtomicU64>,
    conn: quinn::Connection,
    gate: Arc<Gate>,
    limits: Arc<Limits>,
    stats: Arc<WireStats>,
    /// Old versions to build from, per file.
    bases: Arc<Vec<Option<Arc<BaseFile>>>>,
    cache_dir: PathBuf,
}

/// Delta is on unless `NEO_DELTA=0` (benchmarks).
fn delta_enabled() -> bool {
    std::env::var("NEO_DELTA").map(|v| v != "0").unwrap_or(true)
}

/// For each file we'll receive from its start: if the downloads folder already
/// has a big enough file at the same path, chunk it (cached) and keep it open.
fn find_bases(
    files: &[FileMeta],
    offsets: &[u64],
    have: &[u32],
    root: &Path,
    cache_dir: &Path,
) -> (Vec<String>, Arc<Vec<Option<Arc<BaseFile>>>>) {
    let mut wire = vec![String::new(); files.len()];
    let mut open = vec![None; files.len()];
    let mut chunks = 0usize;
    for (i, meta) in files.iter().enumerate() {
        if have.contains(&(i as u32)) || offsets.get(i).copied().unwrap_or(0) != 0 || meta.size < delta::MIN_FILE {
            continue;
        }
        let path = root.join(sanitize_rel_path(&meta.name));
        if !std::fs::metadata(&path).is_ok_and(|m| m.is_file() && m.len() >= delta::MIN_FILE) {
            continue;
        }
        let Ok(idx) = delta::index(cache_dir, &path) else { continue };
        if chunks + idx.chunks.len() > delta::MAX_CHUNKS {
            continue;
        }
        if let Ok(base) = BaseFile::open(&path, &idx) {
            chunks += idx.chunks.len();
            wire[i] = delta::to_wire(&idx);
            open[i] = Some(Arc::new(base));
        }
    }
    if open.iter().all(Option::is_none) {
        wire.clear();
    }
    (wire, Arc::new(open))
}

async fn receive_file(mut stream: quinn::RecvStream, ctx: FileCtx) -> Result<usize> {
    let mut header = [0u8; 12];
    stream.read_exact(&mut header).await?;
    let index = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
    let start = u64::from_le_bytes(header[4..].try_into().unwrap());
    let meta = ctx.files.get(index).ok_or_else(|| anyhow!("bad file index"))?.clone();
    if start > meta.size || start % BLOCK != 0 {
        bail!("bad resume offset");
    }

    // Continue the existing partial file, or reserve a new destination.
    let (fs, file) = match ctx.state.get(index).filter(|f| !f.complete && f.verified >= start) {
        Some(f) => (f, None),
        None => {
            if start != 0 {
                bail!("sender resumed a file we don't have");
            }
            let (final_path, part, file) = reserve(&ctx.root, &sanitize_rel_path(&meta.name)).await?;
            let f = partial::FileState { part, final_path, size: meta.size, verified: 0, complete: false };
            (f, Some(file))
        }
    };
    ctx.state.update(index, |slot| *slot = Some(partial::FileState { verified: start, ..fs.clone() }));
    if file.is_some() && meta.size >= RESUMABLE_MIN {
        // Record a new large file's location right away so it can't be orphaned.
        let state = ctx.state.clone();
        tokio::task::spawn_blocking(move || state.flush()).await?;
    }

    // Writer thread: rebuilds each block (decompress, fill in chunks from the
    // old version), verifies its hash and writes it.
    let (tx, mut rx) = mpsc::channel::<(u8, Vec<u8>, usize, [u8; 32])>(PIPELINE_BLOCKS);
    let progress = ctx.progress.clone();
    let state = ctx.state.clone();
    let part = fs.part.clone();
    let size = meta.size;
    let base = ctx.bases.get(index).cloned().flatten();
    let cache_dir = ctx.cache_dir.clone();
    let writer = tokio::task::spawn_blocking(move || -> Result<()> {
        let mut dec = Decoder::new()?;
        let mut file = match file {
            Some(f) => f,
            None => std::fs::OpenOptions::new()
                .write(true)
                .open(&part)
                .map_err(|e| fatal(format!("Can't write {}: {e}", part.display())))?,
        };
        let disk = |e: std::io::Error| fatal(format!("Disk error: {e}"));
        if start > 0 {
            file.set_len(start).map_err(disk)?;
            file.seek(SeekFrom::Start(start)).map_err(disk)?;
        }
        let mut verified = start;
        while let Some((tag, body, len, hash)) = rx.blocking_recv() {
            let base_ref = base.as_deref().map(|b| b as &dyn wire::Base);
            let decoded = dec.decode(tag, body, len, base_ref);
            let ok = matches!(&decoded, Ok((b, _)) if b.len() == len && blake3::hash(b).as_bytes() == &hash);
            if !ok {
                if tag == PIECES {
                    if let Some(b) = &base {
                        delta::forget(&cache_dir, &b.path);
                    }
                    return Err(BaseChanged.into());
                }
                return Err(fatal("A block failed its integrity check"));
            }
            let (block, _) = decoded?;
            file.write_all(&block).map_err(disk)?;
            verified += block.len() as u64;
            progress.fetch_add(block.len() as u64, Ordering::Relaxed);
            if size >= RESUMABLE_MIN {
                state.update(index, |slot| {
                    if let Some(f) = slot {
                        f.verified = verified;
                    }
                });
            }
        }
        Ok(())
    });

    let net: Result<()> = async {
        let mut pos = start;
        while pos < meta.size {
            let n = (meta.size - pos).min(BLOCK) as usize;
            // One contiguous block: lets BLAKE3 use SIMD and the writer do one write.
            ctx.gate.wait_conn(&ctx.conn).await?;
            let mut tag = [0u8; 1];
            stream.read_exact(&mut tag).await?;
            let (body_len, head) = if tag[0] == RAW {
                (n, 1)
            } else {
                let mut l = [0u8; 4];
                stream.read_exact(&mut l).await?;
                let l = u32::from_le_bytes(l) as usize;
                if l > wire::MAX_BODY {
                    bail!("oversized block");
                }
                (l, 5)
            };
            let mut body = vec![0u8; body_len];
            limit::read(&mut stream, &mut body, &ctx.limits.down, Some((&ctx.gate, &ctx.conn))).await?;
            let mut hash = [0u8; 32];
            stream.read_exact(&mut hash).await?;
            ctx.stats.add(n as u64, (head + body_len + 32) as u64);
            if tag[0] == PIECES {
                ctx.stats.delta.store(true, Ordering::Relaxed);
            }
            if tx.send((tag[0], body, n, hash)).await.is_err() {
                break; // writer failed; its error is reported below
            }
            pos += n as u64;
        }
        Ok(())
    }
    .await;
    drop(tx);
    writer.await??;
    net?;

    let mut done = partial::FileState { verified: meta.size, ..fs };
    let done = tokio::task::spawn_blocking(move || finalize(&mut done).map(|_| done)).await??;
    ctx.state.update(index, |slot| *slot = Some(done));
    Ok(index)
}

/// Renames a fully received `.neopart` into place and marks it complete.
pub(crate) fn finalize(f: &mut partial::FileState) -> Result<()> {
    let mut target = f.final_path.clone();
    if target.exists() {
        target = free_name(&target).ok_or_else(|| fatal("no free file name"))?;
    }
    std::fs::rename(&f.part, &target).map_err(|e| fatal(format!("Can't save {}: {e}", target.display())))?;
    f.final_path = target;
    f.complete = true;
    Ok(())
}

fn numbered(target: &Path, n: u32) -> PathBuf {
    if n == 0 {
        return target.to_path_buf();
    }
    let stem = target.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = target.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    target.with_file_name(format!("{stem} ({n}){ext}"))
}

fn free_name(target: &Path) -> Option<PathBuf> {
    (1..10_000).map(|n| numbered(target, n)).find(|p| !p.exists())
}

fn part_of(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(".neopart");
    PathBuf::from(p)
}

/// Picks a free destination (`name`, `name (1)`, …) and creates its
/// `.neopart` file atomically so concurrent transfers never collide.
pub(crate) async fn reserve(root: &Path, rel: &Path) -> Result<(PathBuf, PathBuf, std::fs::File)> {
    let target = root.join(rel);
    if let Some(parent) = target.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|e| fatal(format!("Can't create {}: {e}", parent.display())))?;
    }
    for n in 0..10_000u32 {
        let candidate = numbered(&target, n);
        if tokio::fs::try_exists(&candidate).await.unwrap_or(false) {
            continue;
        }
        let part = part_of(&candidate);
        match tokio::fs::OpenOptions::new().write(true).create_new(true).open(&part).await {
            Ok(f) => return Ok((candidate, part, f.into_std().await)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(fatal(format!("Can't write {}: {e}", part.display()))),
        }
    }
    Err(fatal(format!("no free file name for {}", target.display())))
}
