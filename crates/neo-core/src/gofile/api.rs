//! Minimal Gofile REST client (https://gofile.io/api).
//!
//! - Every call sends `Authorization: Bearer TOKEN` and checks the JSON
//!   `status` field, not just the HTTP code.
//! - `error-rateLimit` backs off and retries a couple of times; requests are
//!   otherwise sequential and modest (repeatedly exceeding limits can get an
//!   IP banned).
//! - Uploads stream the file from disk as multipart (known length, so no
//!   buffering), counting progress, hashing MD5 to check against Gofile's
//!   answer, and stopping promptly when cancelled.
//! - The upload body honours the shared upload limit and pauses: a paused
//!   upload simply stops feeding its body. Gofile has no resumable uploads,
//!   so if its server drops the stalled connection, that one file starts
//!   again from 0 after resuming (files already finished are never re-sent).
//! - Upload server: Gofile's own pick (`upload.gofile.io`) can land far away
//!   and crawl, so by default Neo times a fresh HTTPS connection to each
//!   regional upload server and uses the quickest (`probe_regions`).

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use futures_util::StreamExt;
use md5::{Digest, Md5};

use crate::limit::Gate;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// An error status from Gofile (e.g. `error-notPremium`).
#[derive(Debug)]
pub struct ApiError(pub String);

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let friendly = match self.0.as_str() {
            "error-token" => "Gofile rejected the token. Copy it again from your gofile.io profile.",
            "error-notPremium" => "That needs a Gofile Premium account.",
            "error-rateLimit" => "Gofile asked us to slow down. Try again in a minute.",
            "error-limits" => "Your Gofile account is full (storage or item limit).",
            "error-notFound" => "Gofile couldn't find that item (it may have been deleted).",
            "error-owner" | "error-notOwner" => "That item belongs to another Gofile account.",
            other => return write!(f, "Gofile said: {other}"),
        };
        f.write_str(friendly)
    }
}

impl std::error::Error for ApiError {}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AccountInfo {
    pub id: String,
    pub email: String,
    pub tier: String,
    #[serde(default)]
    pub root_folder: String,
    #[serde(default)]
    pub storage: u64,
    #[serde(default)]
    pub file_count: u64,
    #[serde(default)]
    pub folder_count: u64,
    /// Plan limits/usage fields Gofile returned, shown as-is (names vary).
    #[serde(default)]
    pub extra: Vec<(String, u64)>,
}

impl AccountInfo {
    pub fn is_guest(&self) -> bool {
        self.tier.eq_ignore_ascii_case("guest") || self.email.is_empty()
    }

    pub fn is_premium(&self) -> bool {
        self.tier.eq_ignore_ascii_case("premium")
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Created {
    pub id: String,
    pub code: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Uploaded {
    pub id: String,
    pub code: String,
    pub download_page: String,
    pub parent_folder: String,
    pub md5: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Child {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub code: String,
    pub size: u64,
    pub create_time: u64,
    pub public: Option<bool>,
}

/// Gofile's regional upload servers: (key, label). The host is
/// `upload-{key}.gofile.io`.
pub const REGIONS: &[(&str, &str)] = &[
    ("na-nyc", "New York"),
    ("na-phx", "Phoenix"),
    ("eu-par", "Paris"),
    ("sa-sao", "São Paulo"),
    ("ap-sgp", "Singapore"),
    ("ap-hkg", "Hong Kong"),
    ("ap-tyo", "Tokyo"),
    ("ap-syd", "Sydney"),
];

/// Gofile's own server choice.
pub const GOFILE_AUTO: &str = "https://upload.gofile.io";

pub fn region_url(key: &str) -> String {
    format!("https://upload-{key}.gofile.io")
}

pub fn region_label(key: &str) -> Option<&'static str> {
    REGIONS.iter().find(|(k, _)| *k == key).map(|(_, l)| *l)
}

pub struct Api {
    http: reqwest::Client,
    api: String,
    /// Fixed upload base from `NEO_GOFILE_UPLOAD` (tests); disables probing.
    upload_override: Option<String>,
}

impl Api {
    /// Base URLs can be overridden (`NEO_GOFILE_API`, `NEO_GOFILE_UPLOAD`) for tests.
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .user_agent(concat!("Neo/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("http client");
        Api {
            http,
            api: std::env::var("NEO_GOFILE_API").unwrap_or_else(|_| "https://api.gofile.io".into()),
            upload_override: std::env::var("NEO_GOFILE_UPLOAD").ok().filter(|u| !u.is_empty()),
        }
    }

    /// The fixed upload base set for tests, if any.
    pub fn upload_override(&self) -> Option<&str> {
        self.upload_override.as_deref()
    }

    /// Times a fresh HTTPS connection (TCP + TLS + one request) to every
    /// regional upload server at once and returns the quickest region key.
    /// Each server gets two tries (the first also warms DNS); the best one
    /// counts. Costs a few KB; only runs before an upload, at most every
    /// 30 minutes.
    pub async fn probe_regions(&self) -> Option<(String, Duration)> {
        let client = reqwest::Client::builder()
            .pool_max_idle_per_host(0)
            .connect_timeout(Duration::from_secs(4))
            .timeout(Duration::from_secs(5))
            .user_agent(concat!("Neo/", env!("CARGO_PKG_VERSION")))
            .build()
            .ok()?;
        let probes = REGIONS.iter().map(|(key, _)| {
            let client = client.clone();
            async move {
                let url = format!("{}/", region_url(key));
                let mut best: Option<Duration> = None;
                for _ in 0..2 {
                    let start = std::time::Instant::now();
                    if client.get(&url).send().await.is_ok() {
                        let t = start.elapsed();
                        best = Some(best.map_or(t, |b| b.min(t)));
                    }
                }
                best.map(|t| (key.to_string(), t))
            }
        });
        futures_util::future::join_all(probes).await.into_iter().flatten().min_by_key(|(_, t)| *t)
    }

    async fn call(&self, method: reqwest::Method, path: &str, token: Option<&str>, body: Option<Value>) -> Result<Value> {
        let mut wait = Duration::from_secs(3);
        for attempt in 0..3 {
            let mut req = self.http.request(method.clone(), format!("{}{path}", self.api)).timeout(Duration::from_secs(30));
            if let Some(t) = token {
                req = req.bearer_auth(t);
            }
            if let Some(b) = &body {
                req = req.json(b);
            }
            let resp: Value = req.send().await?.json().await.map_err(|e| anyhow!("unexpected answer from Gofile: {e}"))?;
            match resp.get("status").and_then(Value::as_str) {
                Some("ok") => return Ok(resp),
                Some("error-rateLimit") if attempt < 2 => {
                    tokio::time::sleep(wait).await;
                    wait *= 3;
                }
                Some(other) => return Err(ApiError(other.to_string()).into()),
                None => bail!("unexpected answer from Gofile"),
            }
        }
        Err(ApiError("error-rateLimit".into()).into())
    }

    /// Asks Gofile to email a sign-in link for a new standard account.
    pub async fn create_account(&self, email: &str) -> Result<()> {
        self.call(reqwest::Method::POST, "/accounts", None, Some(json!({ "email": email }))).await?;
        Ok(())
    }

    /// Validates a token and returns the full account details.
    pub async fn account(&self, token: &str) -> Result<AccountInfo> {
        let id = self.call(reqwest::Method::GET, "/accounts/getid", Some(token), None).await?;
        let id = id["data"]["id"].as_str().ok_or_else(|| anyhow!("Gofile didn't return an account id"))?.to_string();
        let full = self.call(reqwest::Method::GET, &format!("/accounts/{id}"), Some(token), None).await?;
        let d = &full["data"];
        let stats = &d["statsCurrent"];
        let mut extra = Vec::new();
        if let Some(obj) = d.as_object() {
            for (k, v) in obj {
                let key = k.to_ascii_lowercase();
                if (key.contains("limit") || key.contains("traffic")) && v.is_u64() {
                    extra.push((k.clone(), v.as_u64().unwrap_or(0)));
                }
            }
        }
        if let Some(obj) = stats.as_object() {
            for (k, v) in obj {
                if k.to_ascii_lowercase().contains("traffic") && v.is_u64() {
                    extra.push((k.clone(), v.as_u64().unwrap_or(0)));
                }
            }
        }
        Ok(AccountInfo {
            id,
            email: d["email"].as_str().unwrap_or_default().to_string(),
            tier: d["tier"].as_str().unwrap_or_default().to_string(),
            root_folder: d["rootFolder"].as_str().unwrap_or_default().to_string(),
            storage: stats["storage"].as_u64().unwrap_or(0),
            file_count: stats["fileCount"].as_u64().unwrap_or(0),
            folder_count: stats["folderCount"].as_u64().unwrap_or(0),
            extra,
        })
    }

    pub async fn create_folder(&self, token: &str, parent: &str, name: &str, public: Option<bool>) -> Result<Created> {
        let mut body = json!({ "parentFolderId": parent, "folderName": name });
        if let Some(p) = public {
            body["public"] = json!(p);
        }
        let r = self.call(reqwest::Method::POST, "/contents/createFolder", Some(token), Some(body)).await?;
        Ok(Created {
            id: r["data"]["id"].as_str().ok_or_else(|| anyhow!("no folder id"))?.to_string(),
            code: r["data"]["code"].as_str().unwrap_or_default().to_string(),
        })
    }

    /// Updates one attribute (`name`, `description`, `public`, `password`,
    /// `expiry`, `tags`). Returns the data object (e.g. a share code).
    pub async fn update(&self, token: &str, id: &str, attribute: &str, value: Value) -> Result<Value> {
        let r = self
            .call(
                reqwest::Method::PUT,
                &format!("/contents/{id}/update"),
                Some(token),
                Some(json!({ "attribute": attribute, "attributeValue": value })),
            )
            .await?;
        Ok(r["data"].clone())
    }

    pub async fn delete(&self, token: &str, ids: &[String]) -> Result<()> {
        self.call(reqwest::Method::DELETE, "/contents", Some(token), Some(json!({ "contentsId": ids.join(",") }))).await?;
        Ok(())
    }

    /// Premium only: the children of a folder (first 1000).
    pub async fn list(&self, token: &str, folder: &str) -> Result<Vec<Child>> {
        let r = self.call(reqwest::Method::GET, &format!("/contents/{folder}?page=1&pageSize=1000"), Some(token), None).await?;
        let mut out = Vec::new();
        if let Some(children) = r["data"]["children"].as_object() {
            for c in children.values() {
                out.push(Child {
                    id: c["id"].as_str().unwrap_or_default().to_string(),
                    kind: c["type"].as_str().unwrap_or_default().to_string(),
                    name: c["name"].as_str().unwrap_or_default().to_string(),
                    code: c["code"].as_str().unwrap_or_default().to_string(),
                    size: c["size"].as_u64().unwrap_or(0),
                    create_time: c["createTime"].as_u64().unwrap_or(0),
                    public: c["public"].as_bool(),
                });
            }
        }
        Ok(out)
    }

    /// Streams one file into `folder`. `sent` counts bytes as they go out.
    /// Setting `cancel` drops the connection at once (checked every 100 ms,
    /// even while the socket is blocked or paused). `error-rateLimit` and
    /// dropped connections are retried twice with backoff; bytes from a failed
    /// try are taken back off `sent`. A try that failed after being paused
    /// starts the file over once resumed, without counting as a retry.
    #[allow(clippy::too_many_arguments)]
    pub async fn upload(
        &self,
        base: &str,
        token: &str,
        folder: &str,
        path: &Path,
        name: &str,
        sent: Arc<AtomicU64>,
        cancel: Arc<AtomicBool>,
        gate: Arc<Gate>,
        limit: Arc<crate::limit::Limits>,
    ) -> Result<Uploaded> {
        let mut wait = Duration::from_secs(3);
        let mut attempt = 0;
        loop {
            tokio::select! {
                _ = gate.wait() => {}
                _ = cancelled(&cancel) => bail!("cancelled"),
            }
            let mine = Arc::new(AtomicU64::new(0));
            let held = Arc::new(AtomicBool::new(false));
            let body = Body { gate: gate.clone(), limit: limit.clone(), held: held.clone() };
            let result = tokio::select! {
                r = self.upload_once(base, token, folder, path, name, sent.clone(), mine.clone(), body) => r,
                _ = cancelled(&cancel) => Err(anyhow!("cancelled")),
            };
            match result {
                Ok(done) => return Ok(done),
                Err(e) => {
                    sent.fetch_sub(mine.load(Ordering::Relaxed), Ordering::Relaxed);
                    if cancel.load(Ordering::Relaxed) {
                        return Err(e);
                    }
                    if held.load(Ordering::Relaxed) || gate.paused() {
                        crate::debug(&format!("{name}: connection dropped while paused; starting the file over ({e:#})"));
                        continue;
                    }
                    let retry = e.downcast_ref::<ApiError>().is_some_and(|a| a.0 == "error-rateLimit")
                        || e.downcast_ref::<reqwest::Error>().is_some_and(|r| r.is_connect() || r.is_request());
                    attempt += 1;
                    if !retry || attempt == 3 {
                        return Err(e);
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(wait) => {}
                        _ = cancelled(&cancel) => bail!("cancelled"),
                    }
                    wait *= 3;
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn upload_once(
        &self,
        base: &str,
        token: &str,
        folder: &str,
        path: &Path,
        name: &str,
        sent: Arc<AtomicU64>,
        mine: Arc<AtomicU64>,
        body: Body,
    ) -> Result<Uploaded> {
        let file = tokio::fs::File::open(path).await.map_err(|e| anyhow!("Can't open {}: {e}", path.display()))?;
        let size = file.metadata().await?.len();
        let hasher = Arc::new(Mutex::new(Md5::new()));
        let h = hasher.clone();
        let stream = tokio_util::io::ReaderStream::with_capacity(file, 256 * 1024);
        // Smaller pieces under a limit (smooth pace), then pause and limit.
        let piece = body.limit.up.piece();
        let limited = body.limit.up.limited();
        let stream = stream
            .flat_map(move |chunk| {
                let pieces: Vec<std::io::Result<bytes::Bytes>> = match chunk {
                    Ok(c) if limited && c.len() > piece => {
                        (0..c.len()).step_by(piece).map(|at| Ok(c.slice(at..(at + piece).min(c.len())))).collect()
                    }
                    other => vec![other],
                };
                futures_util::stream::iter(pieces)
            })
            .then(move |chunk| {
                let body = body.clone();
                async move {
                    if body.gate.paused() {
                        body.held.store(true, Ordering::Relaxed);
                        body.gate.wait().await;
                    }
                    if let Ok(c) = &chunk {
                        body.limit.up.take(c.len()).await;
                    }
                    chunk
                }
            });
        let stream = stream.map(move |chunk| {
            if let Ok(c) = &chunk {
                h.lock().unwrap().update(c);
                sent.fetch_add(c.len() as u64, Ordering::Relaxed);
                mine.fetch_add(c.len() as u64, Ordering::Relaxed);
            }
            chunk
        });
        let part = reqwest::multipart::Part::stream_with_length(reqwest::Body::wrap_stream(stream), size)
            .file_name(name.to_string())
            .mime_str("application/octet-stream")?;
        let form = reqwest::multipart::Form::new().text("folderId", folder.to_string()).part("file", part);
        let resp: Value = self
            .http
            .post(format!("{base}/uploadfile"))
            .bearer_auth(token)
            .multipart(form)
            .send()
            .await?
            .json()
            .await
            .map_err(|e| anyhow!("unexpected answer from Gofile: {e}"))?;
        match resp.get("status").and_then(Value::as_str) {
            Some("ok") => {}
            Some(other) => return Err(ApiError(other.to_string()).into()),
            None => bail!("unexpected answer from Gofile"),
        }
        let d = &resp["data"];
        let ours = hex::encode(hasher.lock().unwrap().clone().finalize());
        let theirs = d["md5"].as_str().unwrap_or_default().to_ascii_lowercase();
        if !theirs.is_empty() && theirs != ours {
            bail!("{name} arrived damaged at Gofile (checksum mismatch)");
        }
        Ok(Uploaded {
            id: d["id"].as_str().unwrap_or_default().to_string(),
            code: d["code"].as_str().unwrap_or_default().to_string(),
            download_page: d["downloadPage"].as_str().unwrap_or_default().to_string(),
            parent_folder: d["parentFolder"].as_str().unwrap_or_default().to_string(),
            md5: ours,
        })
    }
}

/// What an upload body waits on: the pause gate and the shared upload limit.
#[derive(Clone)]
pub(crate) struct Body {
    gate: Arc<Gate>,
    limit: Arc<crate::limit::Limits>,
    /// Set once the body waited at the gate (so a dropped connection after a
    /// pause restarts the file instead of failing the upload).
    held: Arc<AtomicBool>,
}

/// Resolves once `flag` is set (polled every 100 ms; only runs during uploads).
pub async fn cancelled(flag: &AtomicBool) {
    while !flag.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Gofile convention: folder passwords are sent as the SHA-256 hex of the text.
pub fn password_hash(plain: &str) -> String {
    use sha2::Sha256;
    hex::encode(Sha256::digest(plain.as_bytes()))
}

mod hex {
    pub fn encode(bytes: impl AsRef<[u8]>) -> String {
        bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
    }
}
