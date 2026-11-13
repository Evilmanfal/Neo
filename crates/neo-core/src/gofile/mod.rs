//! The Gofile tab: upload to the user's *own* Gofile account and share
//! Gofile download links.
//!
//! - **Account:** a real (email) Gofile account is required. The user creates
//!   one on gofile.io (or asks Gofile to email a sign-in link) and pastes the
//!   API token from their profile. The token is checked with
//!   `/accounts/getid` and stored with DPAPI (`secret.rs`). Guest tokens are
//!   refused.
//! - **Uploads:** each upload becomes one folder in the account (Gofile's
//!   password, expiry and description apply to folders), with subfolders
//!   recreated, files streamed from disk, and progress, speed and cancel.
//!   The folder's download page is the shared link.
//! - **My uploads:** Neo keeps a local record (`gofile.json`), since free
//!   accounts can't list folders through the API. Premium accounts also get
//!   the real listing of their root folder.
//! - **No network while idle:** nothing is fetched until the user connects,
//!   opens the tab (refresh), uploads or edits something.
//! - **Upload server:** "Auto" (default) uses the regional server with the
//!   quickest connection (probed before an upload, cached 30 minutes); the
//!   user can also pin a region or use Gofile's own pick.
//! - **Parallel files:** up to 3 big or 6 small files of one upload go up at
//!   once. Gofile
//!   has no chunked upload, so one big file is always a single connection.
//! - Files go to Gofile as-is (no Neo encryption), since people download them
//!   from Gofile's web page.
//! - **Pause / limits:** uploads count toward the upload speed limit, and can
//!   be paused. Gofile can't resume a file, so a long pause may make the file
//!   in progress start over (in the same folder; the link stays the same).

mod api;
mod secret;

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;

pub use api::{password_hash, region_label, AccountInfo, REGIONS};
use api::Api;

use crate::{Event, Peer, Shared};

/// How an upload (one Gofile folder) is shared.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ShareOptions {
    /// Reachable by link (otherwise only you, signed in to Gofile).
    pub public: bool,
    /// Plain text from the user; only its SHA-256 is sent, nothing is stored.
    pub password: Option<String>,
    /// Unix seconds. On free accounts Gofile deletes the folder then.
    pub expiry: Option<u64>,
    pub description: String,
}

/// One upload as remembered by Neo ("My uploads").
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UploadRecord {
    /// Gofile content id (UUID) of the upload's folder (or file).
    pub id: String,
    pub code: String,
    pub name: String,
    pub link: String,
    pub size: u64,
    pub files: u32,
    /// Unix seconds.
    pub created: u64,
    pub public: bool,
    #[serde(default)]
    pub password: bool,
    #[serde(default)]
    pub expiry: Option<u64>,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug)]
pub enum GofileCommand {
    /// Validate and store an API token.
    Connect { token: String },
    /// Ask Gofile to email a sign-in link for a new account.
    CreateAccount { email: String },
    Disconnect,
    /// Re-read account details (and the folder listing on Premium).
    Refresh,
    Upload { paths: Vec<PathBuf>, options: ShareOptions },
    Cancel { upload: u64 },
    Pause { upload: u64, paused: bool },
    Rename { id: String, name: String },
    Delete { id: String },
    SetPublic { id: String, public: bool },
    SetPassword { id: String, password: Option<String> },
    SetExpiry { id: String, expiry: Option<u64> },
    SetDescription { id: String, text: String },
    /// Send an upload's link to a Neo contact (or your own device).
    ShareLink { id: String, peer: Peer },
    /// Upload server: "" = Auto (quickest region), "gofile" = Gofile's own
    /// pick, or a region key from `REGIONS`.
    SetRegion { region: String },
}

#[derive(Clone, Debug)]
pub enum GofileEvent {
    /// Connected account (or None when disconnected / never connected).
    Account(Option<AccountInfo>),
    Records(Vec<UploadRecord>),
    /// Gofile emailed a sign-in link to this address.
    EmailSent(String),
    UploadStarted { upload: u64, name: String, total: u64, files: u32 },
    UploadProgress { upload: u64, done: u64 },
    UploadPaused { upload: u64, paused: bool },
    UploadFinished { upload: u64, record: Option<UploadRecord>, error: Option<String>, cancelled: bool },
    /// A request is in flight (connect, refresh, edit), for a spinner.
    Busy(bool),
    /// The upload-server choice, and the region Auto last picked.
    Region { choice: String, fastest: Option<String> },
}

#[derive(Default, Serialize, Deserialize)]
struct Store {
    account: Option<AccountInfo>,
    uploads: Vec<UploadRecord>,
    #[serde(default)]
    region: String,
}

pub(crate) struct Gofile {
    dir: PathBuf,
    api: OnceLock<Api>,
    token: Mutex<Option<String>>,
    store: Mutex<Store>,
    /// Running uploads: (cancel flag, pause gate).
    uploads: Mutex<HashMap<u64, (Arc<AtomicBool>, Arc<crate::limit::Gate>)>>,
    /// Auto's last probe: (region key, when).
    fastest: Mutex<Option<(String, std::time::Instant)>>,
}

/// Upload slots per upload: a big file takes 2, a small one 1, so 3 big or
/// up to 6 small files go at once. Small files are mostly per-request
/// overhead (Gofile finalizes each one before answering), so more of them in
/// flight keeps the line busy and the speed steady.
const SLOTS: usize = 6;
/// Files at least this big count as big.
const BIG_FILE: u64 = 16 * 1024 * 1024;

/// Aborts a spawned task when dropped (so a cancelled upload stops its ticker).
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}
/// How long Auto trusts its last probe.
const PROBE_TTL: Duration = Duration::from_secs(30 * 60);

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl Gofile {
    pub fn load(dir: &Path) -> Self {
        let store: Store = std::fs::read(dir.join("gofile.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let token = secret::load(dir);
        Gofile {
            dir: dir.to_path_buf(),
            api: OnceLock::new(),
            store: Mutex::new(if token.is_some() { store } else { Store { account: None, ..store } }),
            token: Mutex::new(token),
            uploads: Mutex::new(HashMap::new()),
            fastest: Mutex::new(None),
        }
    }

    fn region_event(&self) -> Event {
        let choice = self.store.lock().unwrap().region.clone();
        let fastest = self.fastest.lock().unwrap().as_ref().map(|(k, _)| k.clone());
        Event::Gofile(GofileEvent::Region { choice, fastest })
    }

    /// Where this upload's files go (see `SetRegion`).
    async fn upload_base(&self, shared: &Shared) -> String {
        if let Some(fixed) = self.api().upload_override() {
            return fixed.to_string();
        }
        let choice = self.store.lock().unwrap().region.clone();
        match choice.as_str() {
            "gofile" => api::GOFILE_AUTO.to_string(),
            key if region_label(key).is_some() => api::region_url(key),
            _ => {
                let cached = self.fastest.lock().unwrap().clone().filter(|(_, at)| at.elapsed() < PROBE_TTL);
                let key = match cached {
                    Some((key, _)) => Some(key),
                    None => {
                        let found = self.api().probe_regions().await.map(|(key, _)| key);
                        if let Some(key) = &found {
                            *self.fastest.lock().unwrap() = Some((key.clone(), std::time::Instant::now()));
                            shared.emit(self.region_event());
                        }
                        found
                    }
                };
                key.map(|k| api::region_url(&k)).unwrap_or_else(|| api::GOFILE_AUTO.to_string())
            }
        }
    }

    fn api(&self) -> &Api {
        self.api.get_or_init(Api::new)
    }

    fn token(&self) -> Result<String> {
        self.token.lock().unwrap().clone().ok_or_else(|| anyhow!("Connect your Gofile account first"))
    }

    fn save(&self) {
        let store = self.store.lock().unwrap();
        let _ = std::fs::create_dir_all(&self.dir);
        let tmp = self.dir.join("gofile.json.tmp");
        if std::fs::write(&tmp, serde_json::to_vec_pretty(&*store).unwrap_or_default()).is_ok() {
            let _ = std::fs::rename(tmp, self.dir.join("gofile.json"));
        }
    }

    /// What the UI shows at startup (from disk; no network).
    pub fn initial_events(&self) -> Vec<Event> {
        let store = self.store.lock().unwrap();
        vec![
            Event::Gofile(GofileEvent::Account(store.account.clone())),
            Event::Gofile(GofileEvent::Records(store.uploads.clone())),
            Event::Gofile(GofileEvent::Region { choice: store.region.clone(), fastest: None }),
        ]
    }

    fn records(&self) -> Event {
        Event::Gofile(GofileEvent::Records(self.store.lock().unwrap().uploads.clone()))
    }

    fn update_record(&self, id: &str, f: impl FnOnce(&mut UploadRecord)) {
        if let Some(r) = self.store.lock().unwrap().uploads.iter_mut().find(|r| r.id == id) {
            f(r);
        }
        self.save();
    }
}

/// Runs a Gofile command; results arrive as events.
pub(crate) async fn handle(shared: Arc<Shared>, cmd: GofileCommand) {
    let g = &shared.gofile;
    let busy = !matches!(cmd, GofileCommand::Upload { .. } | GofileCommand::Cancel { .. } | GofileCommand::Pause { .. } | GofileCommand::ShareLink { .. } | GofileCommand::SetRegion { .. });
    if busy {
        shared.emit(Event::Gofile(GofileEvent::Busy(true)));
    }
    let result: Result<()> = async {
        match cmd {
            GofileCommand::Connect { token } => {
                let token = token.trim().to_string();
                if token.is_empty() {
                    bail!("Paste the API token from your gofile.io profile");
                }
                let account = g.api().account(&token).await?;
                if account.is_guest() {
                    bail!("That's a guest Gofile account. Please sign up with your email on gofile.io, then paste that account's token.");
                }
                secret::save(&g.dir, &token)?;
                *g.token.lock().unwrap() = Some(token);
                g.store.lock().unwrap().account = Some(account.clone());
                g.save();
                shared.emit(Event::Gofile(GofileEvent::Account(Some(account.clone()))));
                shared.emit(Event::Info(format!("Connected Gofile account {}", account.email)));
                if account.is_premium() {
                    refresh_listing(&shared, &account).await?;
                }
            }
            GofileCommand::CreateAccount { email } => {
                let email = email.trim().to_string();
                if !email.contains('@') {
                    bail!("Enter a valid email address");
                }
                g.api().create_account(&email).await?;
                shared.emit(Event::Gofile(GofileEvent::EmailSent(email)));
            }
            GofileCommand::Disconnect => {
                secret::forget(&g.dir);
                *g.token.lock().unwrap() = None;
                g.store.lock().unwrap().account = None;
                g.save();
                shared.emit(Event::Gofile(GofileEvent::Account(None)));
            }
            GofileCommand::Refresh => {
                let token = g.token()?;
                let account = g.api().account(&token).await?;
                g.store.lock().unwrap().account = Some(account.clone());
                g.save();
                shared.emit(Event::Gofile(GofileEvent::Account(Some(account.clone()))));
                if account.is_premium() {
                    refresh_listing(&shared, &account).await?;
                }
            }
            GofileCommand::Upload { paths, options } => {
                upload(shared.clone(), paths, options).await;
            }
            GofileCommand::Cancel { upload } => {
                if let Some((flag, _)) = g.uploads.lock().unwrap().get(&upload) {
                    flag.store(true, Ordering::Relaxed);
                }
            }
            GofileCommand::Pause { upload, paused } => {
                let gate = g.uploads.lock().unwrap().get(&upload).map(|(_, gate)| gate.clone());
                if gate.is_some_and(|gate| gate.set_local(paused)) {
                    shared.emit(Event::Gofile(GofileEvent::UploadPaused { upload, paused }));
                }
            }
            GofileCommand::Rename { id, name } => {
                let name = name.trim().chars().take(255).collect::<String>();
                if name.is_empty() {
                    bail!("The name can't be empty");
                }
                g.api().update(&g.token()?, &id, "name", json!(name)).await?;
                g.update_record(&id, |r| r.name = name);
                shared.emit(g.records());
            }
            GofileCommand::Delete { id } => {
                g.api().delete(&g.token()?, std::slice::from_ref(&id)).await?;
                g.store.lock().unwrap().uploads.retain(|r| r.id != id);
                g.save();
                shared.emit(g.records());
            }
            GofileCommand::SetPublic { id, public } => {
                let data = g.api().update(&g.token()?, &id, "public", json!(public)).await?;
                let code = data["code"].as_str().map(str::to_string);
                g.update_record(&id, |r| {
                    r.public = public;
                    if let Some(c) = code.filter(|c| !c.is_empty()) {
                        r.link = format!("https://gofile.io/d/{c}");
                        r.code = c;
                    }
                });
                shared.emit(g.records());
            }
            GofileCommand::SetPassword { id, password } => {
                let value = password.as_deref().filter(|p| !p.is_empty()).map(password_hash).unwrap_or_default();
                if !value.is_empty() && password.as_deref().is_some_and(|p| p.chars().count() < 4) {
                    bail!("Passwords need at least 4 characters");
                }
                g.api().update(&g.token()?, &id, "password", json!(value)).await?;
                g.update_record(&id, |r| r.password = !value.is_empty());
                shared.emit(g.records());
            }
            GofileCommand::SetExpiry { id, expiry } => {
                let value = expiry.map(|e| json!(e)).unwrap_or(json!(""));
                g.api().update(&g.token()?, &id, "expiry", value).await?;
                g.update_record(&id, |r| r.expiry = expiry);
                shared.emit(g.records());
            }
            GofileCommand::SetDescription { id, text } => {
                let text = text.chars().take(2000).collect::<String>();
                g.api().update(&g.token()?, &id, "description", json!(text)).await?;
                g.update_record(&id, |r| r.description = text);
                shared.emit(g.records());
            }
            GofileCommand::ShareLink { id, peer } => {
                let record = g.store.lock().unwrap().uploads.iter().find(|r| r.id == id).cloned();
                let record = record.ok_or_else(|| anyhow!("That upload isn't in your list anymore"))?;
                crate::social::send_link(shared.clone(), peer, record.name, record.link).await;
            }
            GofileCommand::SetRegion { region } => {
                let region = if region == "gofile" || region_label(&region).is_some() { region } else { String::new() };
                g.store.lock().unwrap().region = region;
                g.save();
                shared.emit(g.region_event());
            }
        }
        Ok(())
    }
    .await;
    if busy {
        shared.emit(Event::Gofile(GofileEvent::Busy(false)));
    }
    if let Err(e) = result {
        shared.emit(Event::Notice(format!("{e:#}")));
    }
}

/// Premium: replace "My uploads" with what's really in the root folder,
/// keeping Neo's extra details (options, file counts) where known.
async fn refresh_listing(shared: &Shared, account: &AccountInfo) -> Result<()> {
    let g = &shared.gofile;
    let children = g.api().list(&g.token()?, &account.root_folder).await?;
    {
        let mut store = g.store.lock().unwrap();
        let known: HashMap<String, UploadRecord> = store.uploads.drain(..).map(|r| (r.id.clone(), r)).collect();
        let mut list: Vec<UploadRecord> = children
            .into_iter()
            .map(|c| {
                let mut r = known.get(&c.id).cloned().unwrap_or(UploadRecord {
                    id: c.id.clone(),
                    code: c.code.clone(),
                    name: c.name.clone(),
                    link: String::new(),
                    size: c.size,
                    files: u32::from(c.kind == "file"),
                    created: c.create_time,
                    public: c.public.unwrap_or(true),
                    password: false,
                    expiry: None,
                    description: String::new(),
                });
                r.name = c.name;
                if !c.code.is_empty() {
                    r.link = format!("https://gofile.io/d/{}", c.code);
                    r.code = c.code;
                }
                if let Some(p) = c.public {
                    r.public = p;
                }
                r
            })
            .collect();
        list.sort_by(|a, b| b.created.cmp(&a.created));
        store.uploads = list;
    }
    g.save();
    shared.emit(g.records());
    Ok(())
}

async fn upload(shared: Arc<Shared>, paths: Vec<PathBuf>, options: ShareOptions) {
    let g = &shared.gofile;
    let upload = shared.next_id();
    let cancel = Arc::new(AtomicBool::new(false));
    let gate = Arc::new(crate::limit::Gate::default());
    g.uploads.lock().unwrap().insert(upload, (cancel.clone(), gate.clone()));
    let sent = Arc::new(AtomicU64::new(0));
    let mut folder_id: Option<String> = None;
    // Cancel drops everything in flight at once, whatever step it's on.
    let result = tokio::select! {
        r = upload_inner(&shared, upload, &paths, &options, &sent, &cancel, &gate, &mut folder_id) => r,
        _ = api::cancelled(&cancel) => Err(anyhow!("cancelled")),
    };
    g.uploads.lock().unwrap().remove(&upload);
    let cancelled = cancel.load(Ordering::Relaxed);
    let event = match result {
        Ok(record) => {
            {
                let mut store = g.store.lock().unwrap();
                store.uploads.insert(0, record.clone());
            }
            g.save();
            shared.emit(g.records());
            GofileEvent::UploadFinished { upload, record: Some(record), error: None, cancelled: false }
        }
        Err(e) => {
            // Don't leave a half-filled folder behind in their account (in
            // the background, so the UI hears about the cancel right away).
            if let (Some(id), Ok(token)) = (folder_id, g.token()) {
                let shared = shared.clone();
                tokio::spawn(async move {
                    let _ = shared.gofile.api().delete(&token, &[id]).await;
                });
            }
            GofileEvent::UploadFinished {
                upload,
                record: None,
                error: (!cancelled).then(|| format!("{e:#}")),
                cancelled,
            }
        }
    };
    shared.emit(Event::Gofile(event));
}

async fn upload_inner(
    shared: &Arc<Shared>,
    upload: u64,
    paths: &[PathBuf],
    options: &ShareOptions,
    sent: &Arc<AtomicU64>,
    cancel: &Arc<AtomicBool>,
    gate: &Arc<crate::limit::Gate>,
    folder_id: &mut Option<String>,
) -> Result<UploadRecord> {
    let g = &shared.gofile;
    let token = g.token()?;
    if let Some(p) = options.password.as_deref().filter(|p| !p.is_empty()) {
        if p.chars().count() < 4 {
            bail!("Passwords need at least 4 characters");
        }
    }
    let list = {
        let paths = paths.to_vec();
        tokio::task::spawn_blocking(move || crate::transfer::expand(&paths)).await??
    };
    let total: u64 = list.iter().map(|(_, m)| m.size).sum();
    let single_folder = paths.len() == 1 && paths[0].is_dir();
    let name = match paths {
        [one] => one.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "Upload".into()),
        [first, rest @ ..] => format!(
            "{} and {} more",
            first.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            rest.len()
        ),
        [] => bail!("Nothing to upload"),
    };
    shared.emit(Event::Gofile(GofileEvent::UploadStarted { upload, name: name.clone(), total, files: list.len() as u32 }));

    // Progress, ~4 times a second, only while this upload runs.
    let ticker = {
        let (shared, sent) = (shared.clone(), sent.clone());
        AbortOnDrop(tokio::spawn(async move {
            let mut last = u64::MAX;
            loop {
                tokio::time::sleep(Duration::from_millis(250)).await;
                let done = sent.load(Ordering::Relaxed);
                if done != last {
                    last = done;
                    shared.emit(Event::Gofile(GofileEvent::UploadProgress { upload, done }));
                }
            }
        }))
    };
    let result: Result<UploadRecord> = async {
        // The account's root folder (needs a fresh lookup once).
        let root = {
            let cached = g.store.lock().unwrap().account.as_ref().map(|a| a.root_folder.clone()).unwrap_or_default();
            if cached.is_empty() {
                let account = g.api().account(&token).await?;
                let root = account.root_folder.clone();
                g.store.lock().unwrap().account = Some(account);
                root
            } else {
                cached
            }
        };
        // One folder per upload: Gofile's sharing options live on folders.
        let folder = g.api().create_folder(&token, &root, &name, Some(options.public)).await?;
        *folder_id = Some(folder.id.clone());
        if !options.description.trim().is_empty() {
            g.api().update(&token, &folder.id, "description", json!(options.description.chars().take(2000).collect::<String>())).await?;
        }
        if let Some(p) = options.password.as_deref().filter(|p| !p.is_empty()) {
            g.api().update(&token, &folder.id, "password", json!(password_hash(p))).await?;
        }
        if let Some(e) = options.expiry {
            g.api().update(&token, &folder.id, "expiry", json!(e)).await?;
        }

        // Recreate subfolders (parents first) inside the upload's folder.
        let rel = |name: &str| -> String {
            if single_folder {
                name.split_once('/').map(|(_, r)| r.to_string()).unwrap_or_default()
            } else {
                name.to_string()
            }
        };
        let mut dirs = BTreeSet::new();
        for (_, meta) in &list {
            let r = rel(&meta.name);
            let mut acc = String::new();
            for part in r.split('/').collect::<Vec<_>>().split_last().map(|(_, d)| d.to_vec()).unwrap_or_default() {
                acc = if acc.is_empty() { part.to_string() } else { format!("{acc}/{part}") };
                dirs.insert(acc.clone());
            }
        }
        let mut folder_ids: HashMap<String, String> = HashMap::new();
        folder_ids.insert(String::new(), folder.id.clone());
        for dir in &dirs {
            if cancel.load(Ordering::Relaxed) {
                bail!("cancelled");
            }
            let (parent, leaf) = dir.rsplit_once('/').unwrap_or(("", dir));
            let parent_id = folder_ids.get(parent).cloned().ok_or_else(|| anyhow!("missing parent folder"))?;
            let sub = g.api().create_folder(&token, &parent_id, leaf, None).await?;
            folder_ids.insert(dir.clone(), sub.id);
        }

        // Several files at once (see SLOTS); the first failure drops the rest.
        let base = g.upload_base(shared).await;
        let mut jobs = Vec::with_capacity(list.len());
        for (path, meta) in &list {
            let r = rel(&meta.name);
            let (dir, file_name) = r.rsplit_once('/').unwrap_or(("", &r));
            let target = folder_ids.get(dir).cloned().ok_or_else(|| anyhow!("missing folder for {r}"))?;
            jobs.push((path.clone(), target, file_name.to_string(), meta.size));
        }
        {
            use futures_util::TryStreamExt;
            let (base, token) = (base.as_str(), token.as_str());
            let slots = tokio::sync::Semaphore::new(SLOTS);
            let slots = &slots;
            futures_util::stream::iter(jobs.into_iter().map(Ok::<_, anyhow::Error>))
                .try_for_each_concurrent(SLOTS, |(path, target, file_name, size)| {
                    let (sent, cancel, gate) = (sent.clone(), cancel.clone(), gate.clone());
                    let limits = shared.limits.clone();
                    async move {
                        let _slot = slots.acquire_many(if size >= BIG_FILE { 2 } else { 1 }).await?;
                        if cancel.load(Ordering::Relaxed) {
                            bail!("cancelled");
                        }
                        g.api().upload(base, token, &target, &path, &file_name, sent, cancel, gate, limits).await.map(|_| ())
                    }
                })
                .await?;
        }

        Ok(UploadRecord {
            id: folder.id.clone(),
            link: format!("https://gofile.io/d/{}", folder.code),
            code: folder.code,
            name: name.clone(),
            size: total,
            files: list.len() as u32,
            created: now(),
            public: options.public,
            password: options.password.as_deref().is_some_and(|p| !p.is_empty()),
            expiry: options.expiry,
            description: options.description.clone(),
        })
    }
    .await;
    drop(ticker);
    shared.emit(Event::Gofile(GofileEvent::UploadProgress { upload, done: sent.load(Ordering::Relaxed) }));
    result
}
