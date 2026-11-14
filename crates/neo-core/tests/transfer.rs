//! End-to-end: real nodes on this machine discover each other over LAN
//! multicast and transfer files over QUIC. Data dirs live in the temp folder.

use std::path::{Path, PathBuf};
use std::time::Duration;

use neo_core::{Command, Config, Event, NodeHandle, Peer, TransferState};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("neo-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn node(root: &Path, name: &str) -> NodeHandle {
    node_with(root, name, true)
}

/// A node that asks before accepting (like the real app).
fn node_with(root: &Path, name: &str, auto_accept: bool) -> NodeHandle {
    neo_core::start(Config {
        name: format!("{name}-device"),
        person_name: name.into(),
        data_dir: root.join(name).join("data"),
        downloads: root.join(name).join("downloads"),
        auto_accept,
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

/// Waits for the first event `pick` accepts, skipping others.
async fn wait_for<T>(node: &NodeHandle, mut pick: impl FnMut(Event) -> Option<T>) -> T {
    loop {
        if let Some(t) = pick(next(node).await) {
            return t;
        }
    }
}

/// Waits until `node` sees a device of `account`, and returns it.
async fn see(node: &NodeHandle, account: neo_core::AccountId) -> Peer {
    wait_for(node, |e| match e {
        Event::Peers(list) => list.into_iter().find(|p| p.account == Some(account)),
        _ => None,
    })
    .await
}

/// Deterministic pseudo-random bytes (xorshift), so offset bugs can't hide.
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
    tokio::time::timeout(Duration::from_secs(60), node.events.recv()).await.expect("timed out waiting for an event").unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn sends_files_and_folders_to_two_recipients() {
    let root = temp_dir("fanout");
    let (a, b, sender) = (node(&root, "a"), node(&root, "b"), node(&root, "sender"));

    // Something to send: a multi-block file and a folder of small files.
    let src = root.join("src");
    std::fs::create_dir_all(src.join("album/nested")).unwrap();
    let big = noise(5 * (1 << 20) + 12345, 7);
    std::fs::write(src.join("big.bin"), &big).unwrap();
    for i in 0..40 {
        std::fs::write(src.join(format!("album/{i}.dat")), noise(1000 + i * 977, i as u64)).unwrap();
    }
    std::fs::write(src.join("album/nested/empty.txt"), b"").unwrap();

    // Wait until the sender sees both recipients.
    let wanted = [a.me, b.me];
    let peers: Vec<Peer> = loop {
        if let Event::Peers(list) = next(&sender).await {
            let found: Vec<Peer> = list.into_iter().filter(|p| wanted.contains(&p.id)).collect();
            if found.len() == 2 {
                break found;
            }
        }
    };
    sender.command(Command::Send { paths: vec![src.join("big.bin"), src.join("album")], peers });

    let mut done = 0;
    while done < 2 {
        if let Event::State { state, .. } = next(&sender).await {
            match state {
                TransferState::Done { .. } => done += 1,
                s if s.is_finished() => panic!("transfer ended with {s:?}"),
                _ => {}
            }
        }
    }

    for name in ["a", "b"] {
        let got = root.join(name).join("downloads");
        assert_eq!(std::fs::read(got.join("big.bin")).unwrap(), big);
        for i in 0..40 {
            assert_eq!(std::fs::read(got.join(format!("album/{i}.dat"))).unwrap(), noise(1000 + i * 977, i as u64));
        }
        assert_eq!(std::fs::read(got.join("album/nested/empty.txt")).unwrap().len(), 0);
        // No leftovers: no partial files, and the resume record is gone.
        let leftovers = walk(&got).into_iter().filter(|p| p.extension().is_some_and(|e| e == "neopart")).count();
        assert_eq!(leftovers, 0);
        let records = std::fs::read_dir(root.join(name).join("data/partials")).map(|d| d.count()).unwrap_or(0);
        assert_eq!(records, 0);
    }
    let _ = std::fs::remove_dir_all(&root);
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn settings_are_saved_and_reloaded() {
    let root = temp_dir("settings");
    let n = node(&root, "s");
    let new_downloads = root.join("elsewhere");
    n.command(Command::UpdateSettings {
        name: "  Studio PC  ".into(),
        person_name: "Sam".into(),
        downloads: new_downloads.clone(),
        close_to_tray: false,
    });
    let file = root.join("s/data/settings.json");
    for _ in 0..100 {
        if file.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let saved = neo_core::Settings::load(&root.join("s/data"));
    assert_eq!(saved.name.as_deref(), Some("Studio PC"));
    assert_eq!(saved.downloads.as_deref(), Some(new_downloads.as_path()));
    assert_eq!(saved.close_to_tray, Some(false));
    assert_eq!(saved.person_name.as_deref(), Some("Sam"));
    let _ = std::fs::remove_dir_all(&root);
}

async fn send_and_wait(from: &NodeHandle, to: Peer, file: PathBuf) -> TransferState {
    from.command(Command::Send { paths: vec![file], peers: vec![to] });
    wait_for(from, |e| match e {
        Event::State { state, .. } if state.is_finished() => Some(state),
        _ => None,
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn pairing_needs_consent_and_trusted_contacts_skip_the_prompt() {
    let root = temp_dir("pairing");
    let (alice, bob, carol) = (node_with(&root, "alice", false), node_with(&root, "bob", false), node_with(&root, "carol", false));
    let file = root.join("note.txt");
    std::fs::write(&file, noise(4000, 3)).unwrap();

    // Alice asks; Bob sees who's asking and accepts.
    let bob_device = see(&alice, bob.account).await;
    alice.command(Command::AddContact { device: bob_device.id });
    let (request, asker) = wait_for(&bob, |e| match e {
        Event::PairRequest { id, name, account, .. } => Some((id, (name, account))),
        _ => None,
    })
    .await;
    assert_eq!(asker, ("alice".to_string(), alice.account));
    bob.command(Command::Respond { id: request, accept: true });

    // Both now have each other, under the names they gave.
    let has = |list: &[neo_core::Contact], who: neo_core::AccountId, name: &str| {
        list.iter().any(|c| c.account == who && c.name == name)
    };
    wait_for(&alice, |e| matches!(&e, Event::Contacts(l) if has(l, bob.account, "bob")).then_some(())).await;
    wait_for(&bob, |e| matches!(&e, Event::Contacts(l) if has(l, alice.account, "alice")).then_some(())).await;

    // Bob trusts Alice: her files arrive without a prompt.
    bob.command(Command::SetAutoAccept { account: alice.account, on: true });
    wait_for(&bob, |e| matches!(&e, Event::Contacts(l) if l.iter().any(|c| c.account == alice.account && c.auto_accept)).then_some(())).await;
    assert!(matches!(send_and_wait(&alice, bob_device.clone(), file.clone()).await, TransferState::Done { .. }));

    // Carol isn't a contact: Bob is asked, declines, Carol sees "declined".
    let bob_for_carol = see(&carol, bob.account).await;
    carol.command(Command::Send { paths: vec![file.clone()], peers: vec![bob_for_carol] });
    let incoming = wait_for(&bob, |e| match e {
        Event::Transfer(t) if t.peer_account == Some(carol.account) => Some(t.id),
        _ => None,
    })
    .await;
    wait_for(&bob, |e| matches!(e, Event::State { id, state: TransferState::Waiting } if id == incoming).then_some(())).await;
    bob.command(Command::Respond { id: incoming, accept: false });
    let carol_result = wait_for(&carol, |e| match e {
        Event::State { state, .. } if state.is_finished() => Some(state),
        _ => None,
    })
    .await;
    assert_eq!(carol_result, TransferState::Declined);
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread")]
async fn linking_makes_a_device_act_as_you() {
    let root = temp_dir("linking");
    let (desktop, laptop) = (node_with(&root, "desktop", false), node_with(&root, "laptop", false));
    let file = root.join("photo.bin");
    std::fs::write(&file, noise(300_000, 9)).unwrap();

    // The laptop asks to join the desktop's identity by its code.
    see(&laptop, desktop.account).await;
    laptop.command(Command::LinkWith { code: desktop.account.code().to_lowercase() });
    let shown_on_laptop = wait_for(&laptop, |e| match e {
        Event::LinkPending { sas, .. } => Some(sas),
        _ => None,
    })
    .await;
    let (request, shown_on_desktop) = wait_for(&desktop, |e| match e {
        Event::LinkRequest { id, sas, .. } => Some((id, sas)),
        _ => None,
    })
    .await;
    assert_eq!(shown_on_laptop, shown_on_desktop, "both screens show the same number");
    desktop.command(Command::Respond { id: request, accept: true });

    // The laptop is now "desktop's owner" too…
    wait_for(&laptop, |e| matches!(e, Event::Identity { account, .. } if account == desktop.account).then_some(())).await;
    // …so the desktop sends to it without anyone being asked.
    let laptop_device = wait_for(&desktop, |e| match e {
        Event::Peers(list) => list.into_iter().find(|p| p.id == laptop.me && p.account == Some(desktop.account)),
        _ => None,
    })
    .await;
    assert!(matches!(send_and_wait(&desktop, laptop_device, file).await, TransferState::Done { .. }));
    let _ = std::fs::remove_dir_all(&root);
}

/// A node that can only find others through a (local test) DHT: no LAN
/// multicast, no STUN/UPnP. It publishes its LAN addresses as candidates.
fn internet_only(root: &Path, name: &str, bootstrap: &[String]) -> NodeHandle {
    neo_core::start(Config {
        name: format!("{name}-device"),
        person_name: name.into(),
        data_dir: root.join(name).join("data"),
        downloads: root.join(name).join("downloads"),
        auto_accept: false,
        close_to_tray: false,
        upload_limit: None,
        download_limit: None,
        lan_discovery: false,
        internet: true,
        dht_bootstrap: Some(bootstrap.to_vec()),
        publish_lan: true,
        nat_traversal: false,
        check_updates: false,
        discover_all: true,
    })
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn contacts_find_each_other_through_the_dht_alone() {
    let root = temp_dir("dht");
    let testnet = mainline::Testnet::builder(10).build().unwrap();
    let (alice, bob) = (internet_only(&root, "alice", &testnet.bootstrap), internet_only(&root, "bob", &testnet.bootstrap));
    let file = root.join("far-away.bin");
    std::fs::write(&file, noise(2_000_000, 21)).unwrap();

    // Wait until Bob has published (his reachability says the DHT is up).
    wait_for(&bob, |e| matches!(e, Event::Reachability(r) if r.dht).then_some(())).await;
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Alice only knows Bob's code: it resolves through the DHT, Bob accepts.
    alice.command(Command::AddByCode { code: bob.account.code() });
    let request = wait_for(&bob, |e| match e {
        Event::PairRequest { id, .. } => Some(id),
        _ => None,
    })
    .await;
    bob.command(Command::Respond { id: request, accept: true });

    // Bob now shows up for Alice as an internet peer, and she can send to him.
    let bob_device = wait_for(&alice, |e| match e {
        Event::Peers(list) => list.into_iter().find(|p| p.account == Some(bob.account) && p.remote),
        _ => None,
    })
    .await;
    alice.command(Command::Send { paths: vec![file.clone()], peers: vec![bob_device] });
    let incoming = wait_for(&bob, |e| match e {
        Event::State { id, state: TransferState::Waiting } => Some(id),
        _ => None,
    })
    .await;
    bob.command(Command::Respond { id: incoming, accept: true });
    let result = wait_for(&alice, |e| match e {
        Event::State { state, .. } if state.is_finished() => Some(state),
        _ => None,
    })
    .await;
    assert!(matches!(result, TransferState::Done { .. }), "{result:?}");
    assert_eq!(std::fs::read(root.join("bob/downloads/far-away.bin")).unwrap(), noise(2_000_000, 21));
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread")]
async fn contacts_can_send_each_other_gofile_links() {
    let root = temp_dir("links");
    // Alice already has an upload in her Gofile list.
    let alice_data = root.join("alice/data");
    std::fs::create_dir_all(&alice_data).unwrap();
    std::fs::write(
        alice_data.join("gofile.json"),
        r#"{"account":null,"uploads":[{"id":"u1","code":"Ab12Cd34","name":"Trip","link":"https://gofile.io/d/Ab12Cd34","size":5,"files":1,"created":1,"public":true}]}"#,
    )
    .unwrap();
    let (alice, bob) = (node_with(&root, "alice", false), node_with(&root, "bob", false));

    // A stranger's link is ignored.
    let bob_device = see(&alice, bob.account).await;
    alice.command(Command::Gofile(neo_core::gofile::GofileCommand::ShareLink { id: "u1".into(), peer: bob_device.clone() }));
    wait_for(&alice, |e| matches!(e, Event::Info(ref t) if t.starts_with("Sent the link")).then_some(())).await;

    // Pair, then the link arrives.
    alice.command(Command::AddContact { device: bob_device.id });
    let request = wait_for(&bob, |e| match e {
        Event::PairRequest { id, .. } => Some(id),
        Event::LinkShared { .. } => panic!("a stranger's link must not be shown"),
        _ => None,
    })
    .await;
    bob.command(Command::Respond { id: request, accept: true });
    wait_for(&alice, |e| matches!(&e, Event::Contacts(l) if l.iter().any(|c| c.account == bob.account)).then_some(())).await;
    alice.command(Command::Gofile(neo_core::gofile::GofileCommand::ShareLink { id: "u1".into(), peer: bob_device }));
    let (from, title, url) = wait_for(&bob, |e| match e {
        Event::LinkShared { from, title, url } => Some((from, title, url)),
        _ => None,
    })
    .await;
    assert_eq!((from.as_str(), title.as_str(), url.as_str()), ("alice", "Trip", "https://gofile.io/d/Ab12Cd34"));
    let _ = std::fs::remove_dir_all(&root);
}
