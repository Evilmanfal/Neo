//! Compression and "send only what changed", end to end on this machine.
//! Whatever travels, the received files must be byte-identical.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use neo_core::{Command, Config, Event, NodeHandle, Peer, TransferState};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("neo-wire-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn node(root: &Path, name: &str) -> NodeHandle {
    neo_core::start(Config {
        name: format!("{name}-device"),
        person_name: name.into(),
        data_dir: root.join(name).join("data"),
        downloads: root.join(name).join("downloads"),
        auto_accept: true,
        close_to_tray: false,
        upload_limit: None,
        download_limit: None,
        lan_discovery: true,
        internet: false,
        dht_bootstrap: None,
        publish_lan: true,
        nat_traversal: false,
        check_updates: false,
        discover_all: true,
    })
    .unwrap()
}

fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

/// Real text: Rust sources from the local cargo registry (compresses like
/// code, logs and documents do), repeated to `len` if needed.
fn real_text(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let home = std::env::var("CARGO_HOME").map(PathBuf::from).unwrap_or_default();
    let mut stack = vec![home.join("registry").join("src")];
    while out.len() < len {
        let Some(dir) = stack.pop() else { break };
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        let mut entries: Vec<_> = rd.flatten().map(|e| e.path()).collect();
        entries.sort();
        for p in entries {
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|e| e == "rs") && out.len() < len {
                out.extend_from_slice(&std::fs::read(&p).unwrap_or_default());
            }
        }
    }
    if out.is_empty() {
        out = b"fn main() { println!(\"hello\"); }\n".repeat(64);
    }
    while out.len() < len {
        let n = (len - out.len()).min(out.len());
        out.extend_from_within(..n);
    }
    out.truncate(len);
    out
}

async fn next(node: &NodeHandle) -> Event {
    tokio::time::timeout(Duration::from_secs(90), node.events.recv()).await.expect("timed out waiting for an event").unwrap()
}

async fn see(from: &NodeHandle, to: &NodeHandle) -> Peer {
    let id = to.me;
    loop {
        if let Event::Peers(list) = next(from).await {
            if let Some(p) = list.into_iter().find(|p| p.id == id) {
                return p;
            }
        }
    }
}

/// What one finished transfer looked like from `node`.
#[derive(Debug, Default)]
struct Seen {
    raw: u64,
    wire: u64,
    delta: bool,
    secs: f64,
    /// From the Send command to the end (includes the receiver preparing).
    total: f64,
    state: Option<TransferState>,
}

/// Follows `node`'s next transfer to its end (its first Active to its end).
async fn follow(node: &NodeHandle) -> Seen {
    let mut s = Seen::default();
    let mut started = None;
    loop {
        match next(node).await {
            Event::Wire { raw, wire, delta, .. } => (s.raw, s.wire, s.delta) = (raw, wire, delta),
            Event::State { state: TransferState::Active, .. } => {
                started.get_or_insert_with(Instant::now);
            }
            Event::State { state, .. } if state.is_finished() => {
                s.secs = started.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);
                s.state = Some(state);
                // The last counters come right after the final state.
                let until = tokio::time::Instant::now() + Duration::from_millis(300);
                while let Ok(Ok(e)) = tokio::time::timeout_at(until, node.events.recv()).await {
                    if let Event::Wire { raw, wire, delta, .. } = e {
                        (s.raw, s.wire, s.delta) = (raw, wire, delta);
                    }
                }
                return s;
            }
            _ => {}
        }
    }
}

async fn send(from: &NodeHandle, to: &NodeHandle, peer: &Peer, paths: Vec<PathBuf>) -> (Seen, Seen) {
    let t = Instant::now();
    from.command(Command::Send { paths, peers: vec![peer.clone()] });
    let (mut tx, rx) = tokio::join!(follow(from), follow(to));
    tx.total = t.elapsed().as_secs_f64() - 0.3; // minus follow()'s 300 ms tail
    assert!(matches!(tx.state, Some(TransferState::Done { .. })), "sender: {tx:?}");
    assert!(matches!(rx.state, Some(TransferState::Done { .. })), "receiver: {rx:?}");
    (tx, rx)
}

fn same_tree(a: &Path, b: &Path) {
    for e in std::fs::read_dir(a).unwrap().flatten() {
        let (pa, pb) = (e.path(), b.join(e.file_name()));
        if pa.is_dir() {
            same_tree(&pa, &pb);
        } else {
            assert!(std::fs::read(&pa).unwrap() == std::fs::read(&pb).unwrap_or_default(), "{} differs", pb.display());
        }
    }
}

const MB: usize = 1_000_000;

#[tokio::test(flavor = "multi_thread")]
async fn mixed_folders_arrive_byte_identical_and_text_travels_compressed() {
    let root = temp_dir("mixed");
    let (a, b) = (node(&root, "a"), node(&root, "b"));
    let src = root.join("src/project");
    std::fs::create_dir_all(src.join("assets/empty")).unwrap();
    std::fs::write(src.join("code.txt"), real_text(6 * MB)).unwrap();
    std::fs::write(src.join("random.bin"), noise(5 * MB + 777, 1)).unwrap();
    std::fs::write(src.join("assets/photo.jpg"), noise(3 * MB, 2)).unwrap();
    // Named like an archive but full of text: sampling still compresses it.
    std::fs::write(src.join("assets/stored.zip"), real_text(3 * MB + 5)).unwrap();
    // Text then noise in one file: compression adapts block by block.
    let mut half = real_text(4 * MB);
    half.extend(noise(4 * MB, 3));
    std::fs::write(src.join("half.dat"), half).unwrap();
    for i in 0..30 {
        std::fs::write(src.join(format!("assets/small{i}.txt")), real_text(1000 + i * 333)).unwrap();
    }
    std::fs::write(src.join("assets/zero.txt"), b"").unwrap();

    // A link slower than the CPU (any real network): compression stays on.
    b.command(Command::SetLimits { up: None, down: Some(40.0) });
    let peer = see(&a, &b).await;
    let (tx, rx) = send(&a, &b, &peer, vec![src.clone()]).await;
    same_tree(&src, &root.join("b/downloads/project"));
    let total = 6 * MB + 5 * MB + 777 + 3 * MB + 3 * MB + 5 + 8 * MB;
    eprintln!("mixed folder: {} raw bytes took {} on the wire (receiver saw {})", tx.raw, tx.wire, rx.wire);
    assert!(tx.raw >= total as u64 && rx.raw >= total as u64);
    // ~13 MB of text shrinks a lot; ~12 MB of noise stays (plus framing).
    assert!(tx.wire < tx.raw * 7 / 10, "compression helped: {tx:?}");
    assert_eq!(tx.wire, rx.wire, "both sides count the same wire bytes");
    assert!(!tx.delta);
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread")]
async fn compressed_transfer_resumes_mid_file_after_a_drop() {
    let root = temp_dir("resume");
    let (a, b) = (node(&root, "a"), node(&root, "b"));
    let file = root.join("big.txt");
    let data = real_text(60 * MB);
    std::fs::write(&file, &data).unwrap();
    b.command(Command::SetLimits { up: None, down: Some(4.0) }); // wire bytes: slow enough to cut mid-way
    let peer = see(&a, &b).await;
    a.command(Command::Send { paths: vec![file.clone()], peers: vec![peer] });
    let mut done = 0u64;
    while done < 20 * MB as u64 {
        if let Event::Progress { done: d, .. } = next(&b).await {
            done = d;
        }
    }
    a.command(Command::DropConnections);
    b.command(Command::SetLimits { up: None, down: None });
    let mut interrupted = false;
    let mut first_after = None;
    loop {
        match next(&b).await {
            Event::State { state: TransferState::Interrupted, .. } => interrupted = true,
            Event::Progress { done, .. } if interrupted && first_after.is_none() => first_after = Some(done),
            Event::State { state: TransferState::Done { .. }, .. } => break,
            Event::State { state, .. } if state.is_finished() => panic!("ended with {state:?}"),
            _ => {}
        }
    }
    assert!(interrupted);
    let resumed_at = first_after.unwrap();
    eprintln!("dropped at {done} bytes, resumed at {resumed_at}");
    assert!(resumed_at + (1 << 20) >= done, "resumed from the last verified block, not from 0");
    assert_eq!(std::fs::read(root.join("b/downloads/big.txt")).unwrap(), data);
    let _ = std::fs::remove_dir_all(&root);
}

/// The new version of a file: an insert, a deletion and an append.
fn edit(old: &[u8]) -> Vec<u8> {
    let mut new = old[..old.len() / 5].to_vec();
    new.extend(noise(20_000, 77)); // inserted
    new.extend_from_slice(&old[old.len() / 5..old.len() / 2]);
    new.extend_from_slice(&old[old.len() / 2 + 300_000..]); // 300 KB deleted
    new.extend(noise(100_000, 78)); // appended
    new
}

#[tokio::test(flavor = "multi_thread")]
async fn edited_files_send_only_their_changes_and_survive_bad_bases() {
    let root = temp_dir("delta");
    let (a, b) = (node(&root, "a"), node(&root, "b"));
    let peer = see(&a, &b).await;
    let file = root.join("disk.img");
    let v1 = noise(40 * MB, 11);
    std::fs::write(&file, &v1).unwrap();

    // First send: no base, everything travels.
    let (tx, _) = send(&a, &b, &peer, vec![file.clone()]).await;
    assert!(!tx.delta && tx.wire >= 40 * MB as u64);

    // Edited and sent again: only the changes travel.
    let v2 = edit(&v1);
    std::fs::write(&file, &v2).unwrap();
    let (tx, rx) = send(&a, &b, &peer, vec![file.clone()]).await;
    eprintln!("edited 40 MB file: sent {} bytes on the wire for {} ({:.1}%)", tx.wire, tx.raw, tx.wire as f64 * 100.0 / tx.raw as f64);
    assert!(tx.delta && rx.delta);
    assert!(tx.wire < 2 * MB as u64, "only changes travel: {tx:?}");
    let got = root.join("b/downloads/disk (1).img");
    assert_eq!(std::fs::read(&got).unwrap(), v2, "rebuilt file is byte-identical");
    assert_eq!(std::fs::read(root.join("b/downloads/disk.img")).unwrap(), v1, "the old copy is untouched");

    // The base changed normally (new content, new timestamp): re-chunked, still right.
    let v1b = edit(&v2);
    std::fs::write(root.join("b/downloads/disk.img"), &v1b).unwrap();
    let v3 = edit(&v1b);
    std::fs::write(&file, &v3).unwrap();
    let (tx, _) = send(&a, &b, &peer, vec![file.clone()]).await;
    assert!(tx.delta && tx.wire < 2 * MB as u64, "{tx:?}");
    assert_eq!(std::fs::read(root.join("b/downloads/disk (2).img")).unwrap(), v3);

    // The base changed but kept its timestamp, so the cached chunks lie: the
    // receiver notices, drops the cache and the send still ends correct.
    let base = root.join("b/downloads/disk.img");
    let mtime = std::fs::metadata(&base).unwrap().modified().unwrap();
    let mut lie = v1b.clone();
    for x in lie[..8 * MB].iter_mut() {
        *x ^= 0x5a;
    }
    std::fs::write(&base, &lie).unwrap();
    std::fs::File::options().write(true).open(&base).unwrap().set_modified(mtime).unwrap();
    let v4 = edit(&v3);
    std::fs::write(&file, &v4).unwrap();
    let (tx, _) = send(&a, &b, &peer, vec![file.clone()]).await;
    eprintln!("stale base: finished after a retry, {} bytes on the wire", tx.wire);
    assert_eq!(std::fs::read(root.join("b/downloads/disk (3).img")).unwrap(), v4);

    // No base under that name: a normal send.
    let other = root.join("other.img");
    std::fs::write(&other, &v4).unwrap();
    let (tx, _) = send(&a, &b, &peer, vec![other.clone()]).await;
    assert!(!tx.delta);
    assert_eq!(std::fs::read(root.join("b/downloads/other.img")).unwrap(), v4);
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread")]
async fn swarm_forwards_compressed_blocks() {
    let root = temp_dir("swarm");
    let (s, a, b) = (node(&root, "s"), node(&root, "a"), node(&root, "b"));
    let src = root.join("pack");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("text.txt"), real_text(20 * MB)).unwrap();
    std::fs::write(src.join("noise.bin"), noise(6 * MB, 4)).unwrap();
    let peers = vec![see(&s, &a).await, see(&s, &b).await];
    s.command(Command::Send { paths: vec![src.clone()], peers });
    let (ra, rb) = tokio::join!(follow(&a), follow(&b));
    assert!(matches!(ra.state, Some(TransferState::Done { .. })) && matches!(rb.state, Some(TransferState::Done { .. })));
    same_tree(&src, &root.join("a/downloads/pack"));
    same_tree(&src, &root.join("b/downloads/pack"));
    eprintln!("swarm: a got {} raw over {} wire, b got {} over {}", ra.raw, ra.wire, rb.raw, rb.wire);
    assert!(ra.wire < ra.raw * 6 / 10 && rb.wire < rb.raw * 6 / 10, "blocks arrived compressed, from the sender or forwarded");
    let _ = std::fs::remove_dir_all(&root);
}

// ------------------------------------------------------------ benchmarks
// cargo test --release --test wire_delta -- --ignored --nocapture

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn bench_compressible_folder_over_a_3_5_mb_s_upload() {
    let root = temp_dir("bench-compress");
    let (a, b) = (node(&root, "a"), node(&root, "b"));
    let peer = see(&a, &b).await;
    a.command(Command::SetLimits { up: Some(3.5), down: None });
    let make = |name: &str| {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let text = real_text(28 * MB);
        for (i, part) in text.chunks(MB).enumerate() {
            std::fs::write(dir.join(format!("src{i:02}.rs")), part).unwrap();
        }
        dir
    };
    let (before_dir, after_dir) = (make("plain"), make("packed"));
    // Fast link (loopback, no cap): compression must never make it slower.
    a.command(Command::SetLimits { up: None, down: None });
    let big = |name: &str| {
        let p = root.join(name);
        std::fs::write(&p, real_text(200 * MB)).unwrap();
        p
    };
    let (fast_raw, fast_on) = (big("fast-raw.txt"), big("fast-on.txt"));
    std::env::set_var("NEO_COMPRESS", "0");
    let (r, _) = send(&a, &b, &peer, vec![fast_raw]).await;
    std::env::remove_var("NEO_COMPRESS");
    let (c, _) = send(&a, &b, &peer, vec![fast_on]).await;
    eprintln!(
        "200 MB of text over loopback (no cap): raw {:.2} s ({:.0} MB/s), adaptive compression {:.2} s ({:.0} MB/s, {:.0}% of the bytes)",
        r.secs, 200.0 / r.secs, c.secs, 200.0 / c.secs, c.wire as f64 * 100.0 / c.raw as f64
    );
    a.command(Command::SetLimits { up: Some(3.5), down: None });
    std::env::set_var("NEO_COMPRESS", "0");
    let (off, _) = send(&a, &b, &peer, vec![before_dir]).await;
    std::env::remove_var("NEO_COMPRESS");
    let (on, _) = send(&a, &b, &peer, vec![after_dir]).await;
    eprintln!(
        "28 MB of source code at a 3.5 MB/s upload: raw {:.1} s ({:.2} MB/s), compressed {:.1} s ({:.2} MB/s effective, {:.0}% of the bytes)",
        off.secs,
        28.0 / off.secs,
        on.secs,
        28.0 / on.secs,
        on.wire as f64 * 100.0 / on.raw as f64
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn bench_delta_resend_of_a_large_edited_file() {
    let root = temp_dir("bench-delta");
    let (a, b) = (node(&root, "a"), node(&root, "b"));
    let peer = see(&a, &b).await;
    let file = root.join("vm.img");
    let v1 = noise(300 * MB, 21);
    std::fs::write(&file, &v1).unwrap();
    let (full, _) = send(&a, &b, &peer, vec![file.clone()]).await;
    let v2 = edit(&v1);
    std::fs::write(&file, &v2).unwrap();
    let (cold, _) = send(&a, &b, &peer, vec![file.clone()]).await; // receiver chunks its old copy first
    let v3 = edit(&v1);
    std::fs::write(&file, &v3).unwrap();
    let (warm, _) = send(&a, &b, &peer, vec![file.clone()]).await; // chunk index cached
    a.command(Command::SetLimits { up: Some(3.5), down: None });
    let v4 = edit(&v1);
    std::fs::write(&file, &v4).unwrap();
    let (capped, _) = send(&a, &b, &peer, vec![file.clone()]).await;
    eprintln!("300 MB file, loopback, Send to done: full send {:.2} s; edited re-send {:.2} s (first: receiver chunks its old copy), {:.2} s (chunks cached); {:.2} MB on the wire",
        full.total, cold.total, warm.total, warm.wire as f64 / 1e6);
    eprintln!("same edit at a 3.5 MB/s upload: {:.2} s (a full send would take ~{:.0} s)", capped.total, 300.0 / 3.5);
    let _ = std::fs::remove_dir_all(&root);
}
