//! On a shared network Neo lists nobody but your contacts and your own
//! devices; strangers are added by code. With the internet off, contacts
//! still find each other on the LAN through sealed announcements.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use neo_core::{Command, Config, Event, NodeHandle, TransferState};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("neo-lan-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Like the app: no open discovery, and no internet at all here.
fn node(root: &Path, name: &str) -> NodeHandle {
    neo_core::start(Config {
        name: format!("{name}-device"),
        person_name: name.into(),
        data_dir: root.join(name).join("data"),
        downloads: root.join(name).join("downloads"),
        auto_accept: false,
        close_to_tray: false,
        upload_limit: None,
        download_limit: None,
        lan_discovery: true,
        internet: false,
        dht_bootstrap: None,
        publish_lan: true,
        nat_traversal: false,
        check_updates: false,
        discover_all: false,
    })
    .unwrap()
}

async fn next(node: &NodeHandle) -> Event {
    tokio::time::timeout(Duration::from_secs(30), node.events.recv()).await.expect("timed out").unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn strangers_stay_invisible_and_contacts_meet_on_the_lan() {
    let root = temp_dir("p");
    let (alice, bob) = (node(&root, "alice"), node(&root, "bob"));

    // Five announcement rounds: neither sees the other.
    let until = tokio::time::Instant::now() + Duration::from_secs(5);
    while let Ok(Ok(e)) = tokio::time::timeout_at(until, alice.events.recv()).await {
        if let Event::Peers(list) = e {
            assert!(!list.iter().any(|p| p.id == bob.me), "a stranger was listed");
        }
    }

    // Alice types Bob's code: his announcement becomes readable to her.
    let t = Instant::now();
    alice.command(Command::AddByCode { code: bob.account.code() });
    let request = loop {
        if let Event::PairRequest { id, .. } = next(&bob).await {
            break id;
        }
    };
    eprintln!("found by code on the LAN (no internet) in {:.1} s", t.elapsed().as_secs_f64());
    bob.command(Command::Respond { id: request, accept: true });

    // Now they're contacts and see each other's devices.
    let peer = loop {
        if let Event::Peers(list) = next(&alice).await {
            if let Some(p) = list.into_iter().find(|p| p.id == bob.me) {
                break p;
            }
        }
    };
    loop {
        if let Event::Peers(list) = next(&bob).await {
            if list.iter().any(|p| p.id == alice.me) {
                break;
            }
        }
    }
    bob.command(Command::SetAutoAccept { account: alice.account, on: true });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let file = root.join("hi.txt");
    std::fs::write(&file, b"over the LAN").unwrap();
    alice.command(Command::Send { paths: vec![file], peers: vec![peer] });
    loop {
        if let Event::State { state, .. } = next(&alice).await {
            if state.is_finished() {
                assert!(matches!(state, TransferState::Done { .. }), "{state:?}");
                break;
            }
        }
    }
    assert_eq!(std::fs::read(root.join("bob/downloads/hi.txt")).unwrap(), b"over the LAN");
    let _ = std::fs::remove_dir_all(&root);
}
