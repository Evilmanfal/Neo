//! The Gofile tab against a local mock of the Gofile API (no real account,
//! no network). The mock records every call so the test can check exactly
//! what Neo asked Gofile to do.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use md5::{Digest, Md5};
use neo_core::gofile::{GofileCommand, GofileEvent, ShareOptions};
use neo_core::{Command, Config, Event, NodeHandle};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

#[derive(Clone, Debug)]
enum Call {
    Json { method: String, path: String, body: Value },
    Upload { folder: String, name: String, size: usize, md5: String },
}

#[derive(Default)]
struct Mock {
    calls: Vec<Call>,
    /// `/uploadfile` requests started (including ones dropped half-way).
    upload_requests: u32,
    folders: HashMap<String, String>, // id -> parent
    next: u32,
}

const ROOT: &str = "root-0000";

async fn start_mock() -> (String, Arc<Mutex<Mock>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let mock = Arc::new(Mutex::new(Mock::default()));
    let m = mock.clone();
    tokio::spawn(async move {
        loop {
            let (sock, _) = listener.accept().await.unwrap();
            let m = m.clone();
            tokio::spawn(async move {
                let (r, mut w) = sock.into_split();
                let mut r = BufReader::new(r);
                loop {
                    // Request line + headers.
                    let mut line = String::new();
                    if r.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let mut parts = line.split_whitespace();
                    let method = parts.next().unwrap_or_default().to_string();
                    let path = parts.next().unwrap_or_default().to_string();
                    let mut headers = HashMap::new();
                    loop {
                        let mut h = String::new();
                        r.read_line(&mut h).await.unwrap();
                        let h = h.trim_end().to_string();
                        if h.is_empty() {
                            break;
                        }
                        if let Some((k, v)) = h.split_once(':') {
                            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
                        }
                    }
                    let len: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
                    if path == "/uploadfile" {
                        m.lock().unwrap().upload_requests += 1;
                    }
                    let slow = path == "/uploadfile" && len > 4_000_000;
                    let mut body = vec![0u8; len];
                    let mut got = 0;
                    while got < len {
                        let end = if slow { (got + 65536).min(len) } else { len };
                        // Like a real server, drop an upload that stalls (a long pause).
                        let n = match tokio::time::timeout(Duration::from_secs(1), r.read(&mut body[got..end])).await {
                            Ok(n) => n.unwrap_or(0),
                            Err(_) => return,
                        };
                        if n == 0 {
                            return; // client went away (cancelled upload)
                        }
                        got += n;
                        if slow {
                            tokio::time::sleep(Duration::from_millis(20)).await;
                        }
                    }
                    let token = headers.get("authorization").map(|a| a.trim_start_matches("Bearer ").to_string());
                    let reply = respond(&m, &method, &path, &headers, &body, token.as_deref());
                    let text = reply.to_string();
                    let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{text}", text.len());
                    if w.write_all(resp.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    (base, mock)
}

fn respond(m: &Arc<Mutex<Mock>>, method: &str, path: &str, headers: &HashMap<String, String>, body: &[u8], token: Option<&str>) -> Value {
    let mut m = m.lock().unwrap();
    let account = match token {
        Some("GOOD") => ("acct-good", "me@example.com", "standard"),
        Some("GUEST") => ("acct-guest", "", "guest"),
        _ if path == "/accounts" => ("", "", ""),
        _ => return json!({ "status": "error-token" }),
    };
    if path == "/uploadfile" {
        let ctype = headers.get("content-type").cloned().unwrap_or_default();
        let boundary = format!("--{}", ctype.split("boundary=").nth(1).unwrap_or_default());
        let (mut folder, mut name, mut data) = (String::new(), String::new(), Vec::new());
        let text = body;
        let mut start = 0;
        let positions: Vec<usize> =
            text.windows(boundary.len()).enumerate().filter(|(_, w)| *w == boundary.as_bytes()).map(|(i, _)| i).collect();
        for &pos in &positions {
            if start > 0 {
                let part = &text[start..pos - 2]; // strip trailing CRLF
                let split = part.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
                let head = String::from_utf8_lossy(&part[..split]).to_string();
                let content = &part[split + 4..];
                if head.contains("name=\"folderId\"") {
                    folder = String::from_utf8_lossy(content).to_string();
                } else if head.contains("name=\"file\"") {
                    name = head.split("filename=\"").nth(1).unwrap_or_default().split('"').next().unwrap_or_default().to_string();
                    data = content.to_vec();
                }
            }
            start = pos + boundary.len() + 2;
        }
        let md5 = Md5::digest(&data).iter().map(|b| format!("{b:02x}")).collect::<String>();
        m.calls.push(Call::Upload { folder: folder.clone(), name: name.clone(), size: data.len(), md5: md5.clone() });
        m.next += 1;
        return json!({ "status": "ok", "data": { "id": format!("file-{}", m.next), "code": format!("F{}", m.next), "downloadPage": "https://gofile.io/d/x", "parentFolder": folder, "md5": md5 } });
    }
    let body: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    m.calls.push(Call::Json { method: method.into(), path: path.into(), body: body.clone() });
    match (method, path) {
        ("POST", "/accounts") => json!({ "status": "ok", "data": {} }),
        ("GET", "/accounts/getid") => json!({ "status": "ok", "data": { "id": account.0, "email": account.1, "tier": account.2 } }),
        ("GET", p) if p.starts_with("/accounts/") => json!({ "status": "ok", "data": {
            "id": account.0, "email": account.1, "tier": account.2, "rootFolder": ROOT,
            "statsCurrent": { "storage": 1234, "fileCount": 3, "folderCount": 2 } } }),
        ("POST", "/contents/createFolder") => {
            m.next += 1;
            let id = format!("folder-{}", m.next);
            m.folders.insert(id.clone(), body["parentFolderId"].as_str().unwrap_or_default().to_string());
            json!({ "status": "ok", "data": { "id": id, "code": format!("C{}", m.next), "type": "folder" } })
        }
        ("PUT", p) if p.ends_with("/update") => json!({ "status": "ok", "data": {} }),
        ("DELETE", "/contents") => json!({ "status": "ok", "data": {} }),
        _ => json!({ "status": "error-notFound" }),
    }
}

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
        lan_discovery: false,
        internet: false,
        dht_bootstrap: None,
        publish_lan: false,
        nat_traversal: false,
        check_updates: false,
        discover_all: true,
    })
    .unwrap()
}

async fn wait_for<T>(node: &NodeHandle, mut pick: impl FnMut(Event) -> Option<T>) -> T {
    loop {
        let e = tokio::time::timeout(Duration::from_secs(30), node.events.recv()).await.expect("timed out").unwrap();
        if std::env::var_os("NEO_DEBUG").is_some() { eprintln!("event: {e:?}"); }
        if let Some(t) = pick(e) {
            return t;
        }
    }
}

fn json_calls(m: &Arc<Mutex<Mock>>) -> Vec<(String, String, Value)> {
    m.lock()
        .unwrap()
        .calls
        .iter()
        .filter_map(|c| match c {
            Call::Json { method, path, body } => Some((method.clone(), path.clone(), body.clone())),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn gofile_tab_end_to_end_against_a_mock() {
    let (base, mock) = start_mock().await;
    std::env::set_var("NEO_GOFILE_API", &base);
    std::env::set_var("NEO_GOFILE_UPLOAD", &base);
    let root: PathBuf = std::env::temp_dir().join(format!("neo-gofile-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("album/sub")).unwrap();
    std::fs::write(root.join("album/top.txt"), b"hello gofile").unwrap();
    let big: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 251) as u8).collect();
    std::fs::write(root.join("album/sub/x.bin"), &big).unwrap();

    let n = node(&root, "me");
    let gof = |c: GofileCommand| n.command(Command::Gofile(c));

    // Starts disconnected, with no network calls.
    wait_for(&n, |e| matches!(e, Event::Gofile(GofileEvent::Account(None))).then_some(())).await;
    assert!(mock.lock().unwrap().calls.is_empty(), "no network while idle");

    // Guest tokens are refused.
    gof(GofileCommand::Connect { token: "GUEST".into() });
    let notice = wait_for(&n, |e| match e {
        Event::Notice(t) => Some(t),
        _ => None,
    })
    .await;
    assert!(notice.contains("guest"), "{notice}");

    // A real account connects, and the token is stored encrypted.
    gof(GofileCommand::Connect { token: "GOOD".into() });
    let account = wait_for(&n, |e| match e {
        Event::Gofile(GofileEvent::Account(Some(a))) => Some(a),
        _ => None,
    })
    .await;
    assert_eq!((account.email.as_str(), account.tier.as_str(), account.storage), ("me@example.com", "standard", 1234));
    let token_file = std::fs::read(root.join("me/data/gofile.token")).unwrap();
    assert!(!token_file.windows(4).any(|w| w == b"GOOD"), "token must not be stored in plain text");

    // Upload a folder with sharing options.
    let expiry = 1_900_000_000u64;
    gof(GofileCommand::Upload {
        paths: vec![root.join("album")],
        options: ShareOptions { public: false, password: Some("hunter22".into()), expiry: Some(expiry), description: "Holiday".into() },
    });
    let record = wait_for(&n, |e| match e {
        Event::Gofile(GofileEvent::UploadFinished { record, error, .. }) => Some(record.ok_or(error)),
        _ => None,
    })
    .await
    .expect("upload succeeds");
    assert_eq!(record.name, "album");
    assert_eq!(record.files, 2);
    assert!(record.link.starts_with("https://gofile.io/d/"));
    assert!(record.password && !record.public && record.expiry == Some(expiry));

    let calls = json_calls(&mock);
    let creates: Vec<&Value> = calls.iter().filter(|(_, p, _)| p == "/contents/createFolder").map(|(_, _, b)| b).collect();
    assert_eq!(creates[0]["parentFolderId"], ROOT);
    assert_eq!(creates[0]["folderName"], "album");
    assert_eq!(creates[0]["public"], false);
    assert_eq!(creates[1]["folderName"], "sub");
    assert_eq!(creates[1]["parentFolderId"], record.id.as_str(), "subfolder lives in the upload's folder");
    let updates: HashMap<String, Value> = calls
        .iter()
        .filter(|(_, p, _)| p.ends_with("/update"))
        .map(|(_, _, b)| (b["attribute"].as_str().unwrap().to_string(), b["attributeValue"].clone()))
        .collect();
    assert_eq!(updates["description"], "Holiday");
    assert_eq!(updates["expiry"], expiry);
    assert_eq!(updates["password"], neo_core::gofile::password_hash("hunter22"), "only the SHA-256 is sent");
    let uploads: Vec<Call> = mock.lock().unwrap().calls.iter().filter(|c| matches!(c, Call::Upload { .. })).cloned().collect();
    assert_eq!(uploads.len(), 2);
    for u in &uploads {
        let Call::Upload { folder, name, size, md5 } = u else { unreachable!() };
        match name.as_str() {
            "top.txt" => assert_eq!((folder.as_str(), *size), (record.id.as_str(), 12)),
            "x.bin" => {
                assert_ne!(folder, &record.id, "x.bin goes into the sub folder");
                assert_eq!(*size, big.len());
                assert_eq!(md5, &Md5::digest(&big).iter().map(|b| format!("{b:02x}")).collect::<String>());
            }
            other => panic!("unexpected upload {other}"),
        }
    }

    // Rename and delete.
    gof(GofileCommand::Rename { id: record.id.clone(), name: "Holiday 2026".into() });
    wait_for(&n, |e| matches!(&e, Event::Gofile(GofileEvent::Records(r)) if r.iter().any(|x| x.name == "Holiday 2026")).then_some(())).await;
    gof(GofileCommand::Delete { id: record.id.clone() });
    wait_for(&n, |e| matches!(&e, Event::Gofile(GofileEvent::Records(r)) if r.is_empty()).then_some(())).await;
    assert!(json_calls(&mock).iter().any(|(m, p, b)| m == "DELETE" && p == "/contents" && b["contentsId"] == record.id.as_str()));

    // Cancel a slow upload: it stops and the half-made folder is removed.
    std::fs::write(root.join("slow.bin"), vec![1u8; 6_000_000]).unwrap();
    gof(GofileCommand::Upload { paths: vec![root.join("slow.bin")], options: ShareOptions { public: true, ..Default::default() } });
    let upload = wait_for(&n, |e| match e {
        Event::Gofile(GofileEvent::UploadProgress { upload, done }) if done > 0 => Some(upload),
        _ => None,
    })
    .await;
    let asked = std::time::Instant::now();
    gof(GofileCommand::Cancel { upload });
    let cancelled = wait_for(&n, |e| match e {
        Event::Gofile(GofileEvent::UploadFinished { cancelled, record, .. }) => Some((cancelled, record.is_none())),
        _ => None,
    })
    .await;
    assert_eq!(cancelled, (true, true));
    assert!(asked.elapsed() < Duration::from_millis(600), "cancel took {:?}", asked.elapsed());
    // The half-made folder is removed in the background.
    let mut deletes = 0;
    for _ in 0..50 {
        deletes = json_calls(&mock).iter().filter(|(m, _, _)| m == "DELETE").count();
        if deletes == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(deletes, 2, "the cancelled upload's folder was cleaned up");

    // Upload limit: 3 MB at 2 MB/s takes about 1.5 s.
    std::fs::write(root.join("three.bin"), vec![3u8; 3_000_000]).unwrap();
    n.command(Command::SetLimits { up: Some(2.0), down: None });
    gof(GofileCommand::Upload { paths: vec![root.join("three.bin")], options: ShareOptions { public: true, ..Default::default() } });
    wait_for(&n, |e| matches!(e, Event::Gofile(GofileEvent::UploadStarted { .. })).then_some(())).await;
    let t = std::time::Instant::now();
    wait_for(&n, |e| matches!(e, Event::Gofile(GofileEvent::UploadFinished { record: Some(_), .. })).then_some(())).await;
    let rate = 3.0 / t.elapsed().as_secs_f64();
    eprintln!("gofile upload limit 2 MB/s: measured {rate:.2} MB/s");
    assert!((1.8..=2.2).contains(&rate), "upload capped at 2 MB/s, measured {rate:.2} MB/s");
    n.command(Command::SetLimits { up: None, down: None });

    // Pause and resume: the small file is done and never re-sent; the big
    // one is held so long the server drops it, so it starts over from 0 in
    // the same folder, and the upload keeps its folder and link.
    std::fs::create_dir_all(root.join("pausing")).unwrap();
    std::fs::write(root.join("pausing/done.txt"), b"finished early").unwrap();
    let held: Vec<u8> = (0..6_000_000u32).map(|i| (i * 13 % 241) as u8).collect();
    std::fs::write(root.join("pausing/held.bin"), &held).unwrap();
    let (calls_before, requests_before) = {
        let m = mock.lock().unwrap();
        (m.calls.len(), m.upload_requests)
    };
    gof(GofileCommand::Upload { paths: vec![root.join("pausing")], options: ShareOptions { public: true, ..Default::default() } });
    let upload = wait_for(&n, |e| match e {
        Event::Gofile(GofileEvent::UploadProgress { upload, done }) if done > 1_500_000 => Some(upload),
        _ => None,
    })
    .await;
    gof(GofileCommand::Pause { upload, paused: true });
    wait_for(&n, |e| matches!(e, Event::Gofile(GofileEvent::UploadPaused { paused: true, .. })).then_some(())).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    // Nothing moves while paused (2.5 s: the mock drops the stalled upload after 1 s).
    let mut moved = Vec::new();
    let until = tokio::time::Instant::now() + Duration::from_millis(2500);
    while let Ok(Ok(e)) = tokio::time::timeout_at(until, n.events.recv()).await {
        if let Event::Gofile(GofileEvent::UploadProgress { done, .. }) = e {
            moved.push(done);
        }
    }
    assert!(moved.windows(2).all(|w| w[1] <= w[0]), "no data went up while paused: {moved:?}");
    gof(GofileCommand::Pause { upload, paused: false });
    let record = wait_for(&n, |e| match e {
        Event::Gofile(GofileEvent::UploadFinished { record, error, .. }) => Some(record.ok_or(error)),
        _ => None,
    })
    .await
    .expect("paused upload finishes after resuming");
    let (calls, requests) = {
        let m = mock.lock().unwrap();
        (m.calls[calls_before..].to_vec(), m.upload_requests - requests_before)
    };
    let creates = calls.iter().filter(|c| matches!(c, Call::Json { path, .. } if path == "/contents/createFolder")).count();
    assert_eq!(creates, 1, "one folder for the whole upload, kept across the restart");
    let done: Vec<(String, String, usize, String)> = calls
        .iter()
        .filter_map(|c| match c {
            Call::Upload { folder, name, size, md5 } => Some((folder.clone(), name.clone(), *size, md5.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(done.len(), 2, "each file arrived once: {done:?}");
    assert!(done.iter().all(|(folder, ..)| folder == &record.id), "both in the upload's folder");
    let big = done.iter().find(|(_, name, ..)| name == "held.bin").expect("held.bin arrived");
    assert_eq!((big.2, big.3.clone()), (held.len(), Md5::digest(&held).iter().map(|b| format!("{b:02x}")).collect::<String>()));
    assert_eq!(requests, 3, "done.txt once, held.bin dropped once and sent again");
    assert!(record.link.starts_with("https://gofile.io/d/"));

    // Cancel still works at once while paused.
    gof(GofileCommand::Upload { paths: vec![root.join("slow.bin")], options: ShareOptions { public: true, ..Default::default() } });
    let upload = wait_for(&n, |e| match e {
        Event::Gofile(GofileEvent::UploadProgress { upload, done }) if done > 0 => Some(upload),
        _ => None,
    })
    .await;
    gof(GofileCommand::Pause { upload, paused: true });
    wait_for(&n, |e| matches!(e, Event::Gofile(GofileEvent::UploadPaused { paused: true, .. })).then_some(())).await;
    let asked = std::time::Instant::now();
    gof(GofileCommand::Cancel { upload });
    let cancelled = wait_for(&n, |e| match e {
        Event::Gofile(GofileEvent::UploadFinished { cancelled, .. }) => Some(cancelled),
        _ => None,
    })
    .await;
    assert!(cancelled && asked.elapsed() < Duration::from_millis(600), "cancel while paused took {:?}", asked.elapsed());

    // Disconnect forgets the token.
    gof(GofileCommand::Disconnect);
    wait_for(&n, |e| matches!(e, Event::Gofile(GofileEvent::Account(None))).then_some(())).await;
    assert!(!root.join("me/data/gofile.token").exists());
    let _ = std::fs::remove_dir_all(&root);
}
