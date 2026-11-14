//! Speed limits and pause/resume, end to end on this machine (loopback).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use neo_core::{Command, Config, Event, NodeHandle, Peer, TransferState};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("neo-limits-{}-{name}", std::process::id()));
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

async fn next(node: &NodeHandle) -> Event {
    let e = tokio::time::timeout(Duration::from_secs(60), node.events.recv()).await.expect("timed out waiting for an event").unwrap();
    if std::env::var_os("TEST_EVENTS").is_some() && !matches!(e, Event::Progress { .. } | Event::Peers(_)) {
        eprintln!("{} {e:?}", node.name);
    }
    e
}

async fn wait_for<T>(node: &NodeHandle, mut pick: impl FnMut(Event) -> Option<T>) -> T {
    loop {
        if let Some(t) = pick(next(node).await) {
            return t;
        }
    }
}

async fn see(from: &NodeHandle, to: &NodeHandle) -> Peer {
    let id = to.me;
    wait_for(from, |e| match e {
        Event::Peers(list) => list.into_iter().find(|p| p.id == id),
        _ => None,
    })
    .await
}

/// Sends `file` and returns the seconds from Active to Done on the receiver.
async fn timed_send(from: &NodeHandle, to: &NodeHandle, peer: &Peer, file: &Path) -> f64 {
    let peer = peer.clone();
    from.command(Command::Send { paths: vec![file.to_path_buf()], peers: vec![peer] });
    let mut started = None;
    loop {
        if let Event::State { state, .. } = next(to).await {
            match state {
                TransferState::Active => started = Some(Instant::now()),
                TransferState::Done { .. } => return started.expect("was active").elapsed().as_secs_f64(),
                s if s.is_finished() => panic!("ended with {s:?}"),
                _ => {}
            }
        }
    }
}

const MB: usize = 1_000_000;

#[tokio::test(flavor = "multi_thread")]
async fn download_and_upload_limits_hold_the_total_rate() {
    let root = temp_dir("rates");
    let (a, b) = (node(&root, "a"), node(&root, "b"));
    let file = root.join("thirty.bin");
    // A different file for each timed send: a second copy of the same name
    // would go as a delta (only changes) and finish at once.
    let (second, third) = (root.join("second.bin"), root.join("third.bin"));
    let data = noise(30 * MB, 5);
    std::fs::write(&file, &data).unwrap();
    std::fs::write(&second, noise(30 * MB, 6)).unwrap();
    std::fs::write(&third, noise(30 * MB, 7)).unwrap();

    // Download limit on the receiver: QUIC flow control slows the sender.
    let peer = see(&a, &b).await;
    b.command(Command::SetLimits { up: None, down: Some(10.0) });
    let secs = timed_send(&a, &b, &peer, &file).await;
    let rate = 30.0 / secs;
    eprintln!("download limit 10 MB/s: measured {rate:.2} MB/s");
    assert!((9.0..=11.0).contains(&rate), "download capped at 10 MB/s, measured {rate:.2} MB/s");
    assert_eq!(std::fs::read(root.join("b/downloads/thirty.bin")).unwrap(), data);

    // Upload limit on the sender (and B unlimited again).
    b.command(Command::SetLimits { up: None, down: None });
    a.command(Command::SetLimits { up: Some(15.0), down: None });
    let secs = timed_send(&a, &b, &peer, &second).await;
    let rate = 30.0 / secs;
    eprintln!("upload limit 15 MB/s: measured {rate:.2} MB/s");
    assert!((13.5..=16.5).contains(&rate), "upload capped at 15 MB/s, measured {rate:.2} MB/s");

    // Limits are saved, and changes apply to a running transfer at once.
    let saved = neo_core::Settings::load(&root.join("a/data"));
    assert_eq!(saved.upload_limit, Some(15.0));
    a.command(Command::SetLimits { up: Some(2.0), down: None });
    a.command(Command::Send { paths: vec![third.clone()], peers: vec![peer] });
    wait_for(&b, |e| matches!(e, Event::State { state: TransferState::Active, .. }).then_some(())).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let lifted = Instant::now();
    a.command(Command::SetLimits { up: None, down: None });
    wait_for(&b, |e| matches!(e, Event::State { state: TransferState::Done { .. }, .. }).then_some(())).await;
    // 29 MB left at 2 MB/s would take ~15 s.
    assert!(lifted.elapsed() < Duration::from_secs(5), "lifting the limit took effect at once ({:?})", lifted.elapsed());
    let _ = std::fs::remove_dir_all(&root);
}

/// Receiver-side progress, plus pause flags seen on both sides.
#[derive(Default, Debug)]
struct Seen {
    done: u64,
    paused: Option<(bool, bool)>,
    interrupted: bool,
    finished: Option<TransferState>,
}

fn track(seen: &mut Seen, e: Event) {
    match e {
        Event::Progress { done, .. } => seen.done = done,
        Event::Paused { local, remote, .. } => seen.paused = Some((local, remote)),
        Event::State { state: TransferState::Interrupted, .. } => seen.interrupted = true,
        Event::State { state, .. } if state.is_finished() => seen.finished = Some(state),
        _ => {}
    }
}

/// Drains events for `dur`, folding them into `seen`.
async fn watch(node: &NodeHandle, seen: &mut Seen, dur: Duration) {
    let until = tokio::time::Instant::now() + dur;
    while let Ok(Ok(e)) = tokio::time::timeout_at(until, node.events.recv()).await {
        track(seen, e);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn pause_stops_data_survives_a_long_pause_and_a_dropped_connection() {
    let root = temp_dir("pause");
    let (a, b) = (node(&root, "a"), node(&root, "b"));
    let file = root.join("forty.bin");
    let data = noise(40 * MB, 9);
    std::fs::write(&file, &data).unwrap();
    b.command(Command::SetLimits { up: None, down: Some(10.0) }); // slow enough to catch mid-way

    let peer = see(&a, &b).await;
    a.command(Command::Send { paths: vec![file.clone()], peers: vec![peer] });
    let id = wait_for(&b, |e| match e {
        Event::Transfer(t) => Some(t.id),
        _ => None,
    })
    .await;
    let (mut rs, mut ss) = (Seen::default(), Seen::default());
    while rs.done < 8 * MB as u64 {
        track(&mut rs, next(&b).await);
    }

    // The receiver pauses; the sender is told.
    b.command(Command::Pause { id, paused: true });
    watch(&b, &mut rs, Duration::from_millis(500)).await;
    assert_eq!(rs.paused, Some((true, false)));
    watch(&a, &mut ss, Duration::from_millis(10)).await;
    assert_eq!(ss.paused, Some((false, true)), "sender sees 'paused by b'");
    let at_pause = (rs.done, ss.done);

    // Longer than QUIC's 6 s idle timeout: keepalives hold the connection,
    // and no data moves.
    let (r2, s2) = tokio::join!(
        async {
            let mut r = Seen::default();
            watch(&b, &mut r, Duration::from_secs(8)).await;
            r
        },
        async {
            let mut s = Seen::default();
            watch(&a, &mut s, Duration::from_secs(8)).await;
            s
        }
    );
    assert!(!r2.interrupted && !s2.interrupted, "connection stayed up through a long pause");
    assert!(r2.done == 0 || r2.done == at_pause.0, "receiver moved no data while paused ({} -> {})", at_pause.0, r2.done);
    assert!(s2.done == 0 || s2.done == at_pause.1, "sender moved no data while paused ({} -> {})", at_pause.1, s2.done);

    // Now the network drops while still paused.
    a.command(Command::DropConnections);
    let mut s3 = Seen::default();
    let mut min_after = u64::MAX;
    let deadline = Instant::now() + Duration::from_secs(30);
    // The sender reconnects on its own and learns it's still paused.
    while !(s3.interrupted && s3.paused == Some((false, true))) {
        assert!(Instant::now() < deadline, "sender didn't reconnect: {s3:?}");
        let e = next(&a).await;
        if let Event::Progress { done, .. } = e {
            if s3.interrupted {
                min_after = min_after.min(done);
            }
        }
        track(&mut s3, e);
    }

    // Resume: it continues from the last verified block, not from 0.
    b.command(Command::Pause { id, paused: false });
    let mut s4 = Seen::default();
    while s4.finished.is_none() {
        let e = next(&a).await;
        if let Event::Progress { done, .. } = e {
            min_after = min_after.min(done);
        }
        track(&mut s4, e);
    }
    assert!(matches!(s4.finished, Some(TransferState::Done { .. })), "{s4:?}");
    assert!(
        min_after + (1 << 20) >= at_pause.0,
        "resumed at {min_after}, but {} bytes were verified before the drop",
        at_pause.0
    );
    let mut rdone = Seen::default();
    while rdone.finished.is_none() {
        track(&mut rdone, next(&b).await);
    }
    assert_eq!(std::fs::read(root.join("b/downloads/forty.bin")).unwrap(), data);
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread")]
async fn pausing_one_swarm_recipient_does_not_hold_up_the_other() {
    let root = temp_dir("swarm");
    let (sender, a, b) = (node(&root, "sender"), node(&root, "a"), node(&root, "b"));
    let file = root.join("shared.bin");
    let data = noise(30 * MB, 13);
    std::fs::write(&file, &data).unwrap();
    sender.command(Command::SetLimits { up: Some(10.0), down: None });

    let peers = vec![see(&sender, &a).await, see(&sender, &b).await];
    sender.command(Command::Send { paths: vec![file.clone()], peers });
    let a_id = wait_for(&a, |e| match e {
        Event::Transfer(t) => Some(t.id),
        _ => None,
    })
    .await;
    let mut seen_a = Seen::default();
    while seen_a.done < 3 * MB as u64 {
        track(&mut seen_a, next(&a).await);
    }
    a.command(Command::Pause { id: a_id, paused: true });

    // B finishes while A stays paused.
    let mut seen_b = Seen::default();
    let t = Instant::now();
    while seen_b.finished.is_none() {
        track(&mut seen_b, next(&b).await);
        assert!(t.elapsed() < Duration::from_secs(30), "b stalled behind a paused a");
    }
    assert!(matches!(seen_b.finished, Some(TransferState::Done { .. })), "{seen_b:?}");
    assert_eq!(std::fs::read(root.join("b/downloads/shared.bin")).unwrap(), data);
    watch(&a, &mut seen_a, Duration::from_millis(200)).await;
    assert!(seen_a.finished.is_none() && seen_a.done < 30 * MB as u64, "a is still paused");

    // A resumes and gets the rest.
    a.command(Command::Pause { id: a_id, paused: false });
    while seen_a.finished.is_none() {
        track(&mut seen_a, next(&a).await);
    }
    assert!(matches!(seen_a.finished, Some(TransferState::Done { .. })), "{seen_a:?}");
    assert_eq!(std::fs::read(root.join("a/downloads/shared.bin")).unwrap(), data);
    let _ = std::fs::remove_dir_all(&root);
}

/// Size of the receiver's in-progress file (0 if not there yet).
fn part_len(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// After `act`, how long until `path` stops growing (it must then hold still
/// for `settle`), and how much it grew in the meantime.
async fn time_to_stop(path: &Path, settle: Duration) -> (Duration, u64) {
    let (t0, start) = (Instant::now(), part_len(path));
    let (mut last, mut changed) = (start, t0);
    while changed.elapsed() < settle {
        tokio::time::sleep(Duration::from_millis(2)).await;
        let now = part_len(path);
        if now != last {
            last = now;
            changed = Instant::now();
        }
    }
    (changed - t0, last - start)
}

/// How long until `path` grows again.
async fn time_to_flow(path: &Path) -> Duration {
    let (t0, start) = (Instant::now(), part_len(path));
    while part_len(path) == start {
        assert!(t0.elapsed() < Duration::from_secs(5), "never resumed");
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    t0.elapsed()
}

#[tokio::test(flavor = "multi_thread")]
async fn pause_and_resume_take_effect_quickly() {
    let root = temp_dir("quick");
    let (a, b) = (node(&root, "a"), node(&root, "b"));
    let file = root.join("eighty.bin");
    std::fs::write(&file, noise(160 * MB, 3)).unwrap();
    // Slow enough to stay mid-transfer for the whole test.
    b.command(Command::SetLimits { up: None, down: Some(20.0) }); // a 1 MB block lands every ~50 ms
    let peer = see(&a, &b).await;
    a.command(Command::Send { paths: vec![file.clone()], peers: vec![peer] });
    let rid = wait_for(&b, |e| match e {
        Event::Transfer(t) => Some(t.id),
        _ => None,
    })
    .await;
    let sid = wait_for(&a, |e| match e {
        Event::Transfer(t) => Some(t.id),
        _ => None,
    })
    .await;
    let part = root.join("b/downloads/eighty.bin.neopart");
    time_to_flow(&part).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let settle = Duration::from_millis(600);
    let mut report = Vec::new();
    for (who, node, id) in [("receiver", &b, rid), ("sender", &a, sid)] {
        node.command(Command::Pause { id, paused: true });
        let (stop, grew) = time_to_stop(&part, settle).await;
        node.command(Command::Pause { id, paused: false });
        let flow = time_to_flow(&part).await;
        report.push(format!("{who}: stops in {stop:?} (+{} KB after the click), flows again in {flow:?}", grew / 1000));
        assert!(stop < Duration::from_millis(300), "{who} pause took {stop:?}");
        assert!(flow < Duration::from_millis(300), "{who} resume took {flow:?}");
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    eprintln!("{}", report.join("\n"));
    drop((a, b));
    let _ = std::fs::remove_dir_all(&root);
}
