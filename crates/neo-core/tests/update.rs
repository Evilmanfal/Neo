//! The update check and download against a local mock of GitHub's release
//! API: a newer signed release is announced, downloads and verifies; a
//! tampered download is refused and deleted.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use neo_core::update::UpdateEvent;
use neo_core::{Command, Config, Event, NodeHandle};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

/// Serves `/latest` (JSON), `/neo.exe` and `/neo.exe.sig` from `files`.
async fn mock(files: Arc<Mutex<Vec<(String, Vec<u8>)>>>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = listener.accept().await else { return };
            let files = files.clone();
            tokio::spawn(async move {
                let (r, mut w) = sock.into_split();
                let mut r = BufReader::new(r);
                loop {
                    let mut line = String::new();
                    if r.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let path = line.split_whitespace().nth(1).unwrap_or_default().to_string();
                    loop {
                        let mut h = String::new();
                        r.read_line(&mut h).await.unwrap();
                        if h.trim().is_empty() {
                            break;
                        }
                    }
                    let body = files.lock().unwrap().iter().find(|(p, _)| *p == path).map(|(_, b)| b.clone());
                    let resp = match body {
                        Some(b) => {
                            let mut v = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", b.len()).into_bytes();
                            v.extend(b);
                            v
                        }
                        None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec(),
                    };
                    if w.write_all(&resp).await.is_err() {
                        return;
                    }
                    let _ = r.read(&mut []).await;
                }
            });
        }
    });
    base
}

fn node(root: &Path) -> NodeHandle {
    neo_core::start(Config {
        name: "me-device".into(),
        person_name: "me".into(),
        data_dir: root.join("data"),
        downloads: root.join("downloads"),
        auto_accept: false,
        close_to_tray: false,
        upload_limit: None,
        download_limit: None,
        lan_discovery: false,
        internet: false,
        dht_bootstrap: None,
        publish_lan: false,
        nat_traversal: false,
        check_updates: true,
        discover_all: false,
    })
    .unwrap()
}

async fn update_event(n: &NodeHandle) -> UpdateEvent {
    loop {
        let e = tokio::time::timeout(Duration::from_secs(30), n.events.recv()).await.expect("timed out").unwrap();
        if let Event::Update(u) = e {
            if !matches!(u, UpdateEvent::Progress { .. }) {
                return u;
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_signed_release_is_offered_downloaded_and_verified_and_a_tampered_one_refused() {
    let root: PathBuf = std::env::temp_dir().join(format!("neo-update-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();

    // A "release": an exe and its signature with a test key.
    let exe: Vec<u8> = (0..3_000_000u32).map(|i| (i * 31 % 251) as u8).collect();
    let exe_path = root.join("neo.exe");
    std::fs::write(&exe_path, &exe).unwrap();
    let (sig, public) = neo_core::update::sign(&exe_path, &root.join("test.key")).unwrap();
    let files = Arc::new(Mutex::new(Vec::new()));
    let base = mock(files.clone()).await;
    let latest = serde_json::json!({
        "tag_name": "v99.0.0",
        "body": "## Neo 99\n\n- **Much** faster sends\n- other things",
        "assets": [
            { "name": "neo.exe", "browser_download_url": format!("{base}/neo.exe") },
            { "name": "neo.exe.sig", "browser_download_url": format!("{base}/neo.exe.sig") }
        ]
    });
    *files.lock().unwrap() = vec![
        ("/latest".into(), serde_json::to_vec(&latest).unwrap()),
        ("/neo.exe".into(), exe.clone()),
        ("/neo.exe.sig".into(), sig.clone()),
    ];
    std::env::set_var("NEO_UPDATE_URL", format!("{base}/latest"));
    std::env::set_var("NEO_UPDATE_KEY", public.iter().map(|b| format!("{b:02x}")).collect::<String>());

    let n = node(&root);
    let rel = match update_event(&n).await {
        UpdateEvent::Available(r) => r,
        other => panic!("expected an offer, got {other:?}"),
    };
    assert_eq!((rel.version.as_str(), rel.notes.as_str()), ("99.0.0", "Much faster sends"));

    n.command(Command::DownloadUpdate);
    let path = match update_event(&n).await {
        UpdateEvent::Ready(p) => p,
        other => panic!("expected a verified download, got {other:?}"),
    };
    assert_eq!(std::fs::read(&path).unwrap(), exe);
    let _ = std::fs::remove_file(&path);

    // Same signature, different bytes: refused, nothing left behind.
    let mut bad = exe.clone();
    bad[1234] ^= 1;
    files.lock().unwrap()[1].1 = bad;
    n.command(Command::DownloadUpdate);
    match update_event(&n).await {
        UpdateEvent::Failed(e) => assert!(e.contains("signature"), "{e}"),
        other => panic!("a tampered download must be refused, got {other:?}"),
    }
    assert!(!path.exists(), "the refused file was deleted");
    let _ = std::fs::remove_dir_all(&root);
}
