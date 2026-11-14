//! A Neo 0.1 device (protocol `neo/5`) and this version meet: both sides get
//! a plain "update Neo" message instead of a cryptic handshake failure.

use std::path::{Path, PathBuf};
use std::time::Duration;

use neo_core::identity::Identity;
use neo_core::tls;
use neo_core::{Command, Config, Event, NodeHandle, Peer, TransferState};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("neo-version-{}-{name}", std::process::id()));
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

async fn next(node: &NodeHandle) -> Event {
    tokio::time::timeout(Duration::from_secs(30), node.events.recv()).await.expect("timed out").unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn old_and_new_neo_tell_their_users_to_update() {
    let root = temp_dir("v");
    let (a, b) = (node(&root, "a"), node(&root, "b"));
    let a_addr = loop {
        if let Event::Peers(list) = next(&b).await {
            if let Some(p) = list.into_iter().find(|p| p.id == a.me) {
                break p.addr;
            }
        }
    };
    let old = Identity::load_or_create(&root.join("old")).unwrap();

    // An old client reaches a new device: it is told why, in the close reason
    // Neo 0.1 shows as the failure, and the new device's user hears about it.
    let mut client = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    client.set_default_client_config(tls::client_config_alpn(&old, a.me, &[tls::LEGACY_ALPN]).unwrap());
    let to = std::net::SocketAddr::from(([127, 0, 0, 1], a_addr.port()));
    let conn = client.connect(to, tls::SERVER_NAME).unwrap().await.unwrap();
    let reason = match conn.closed().await {
        quinn::ConnectionError::ApplicationClosed(c) => (c.error_code.into_inner(), String::from_utf8_lossy(&c.reason).into_owned()),
        other => panic!("unexpected {other:?}"),
    };
    assert_eq!(reason.0, 2, "code 2: Neo 0.1 shows the reason as the failure");
    assert!(reason.1.contains("newer version of Neo"), "{}", reason.1);
    loop {
        if let Event::Notice(text) = next(&a).await {
            assert!(text.contains("older version of Neo"), "{text}");
            break;
        }
    }

    // A new device sends to an old one: a clear failure, no retry loop.
    let old_server = quinn::Endpoint::server(tls::server_config_alpn(&old, &[tls::LEGACY_ALPN]).unwrap(), "127.0.0.1:0".parse().unwrap()).unwrap();
    let old_addr = old_server.local_addr().unwrap();
    tokio::spawn(async move {
        while let Some(inc) = old_server.accept().await {
            let _ = inc.await;
        }
    });
    let old_peer = Peer {
        id: old.id,
        name: "Old PC".into(),
        addr: std::net::SocketAddr::from(([127, 0, 0, 1], old_addr.port())),
        account: None,
        account_key: None,
        alt: Vec::new(),
        remote: false,
        nat: None,
    };
    let file = root.join("note.txt");
    std::fs::write(&file, b"hello").unwrap();
    b.command(Command::Send { paths: vec![file], peers: vec![old_peer] });
    let state = loop {
        if let Event::State { state, .. } = next(&b).await {
            if state.is_finished() {
                break state;
            }
        }
    };
    match state {
        TransferState::Failed(m) => assert!(m.contains("Old PC has an older version of Neo"), "{m}"),
        other => panic!("expected a clear failure, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&root);
}
