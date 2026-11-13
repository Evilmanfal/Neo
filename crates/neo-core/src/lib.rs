//! Neo core: identity, contacts, LAN discovery and the QUIC transfer engine.
//!
//! The engine runs on its own small tokio runtime thread. The UI talks to it
//! through [`NodeHandle`]: it sends [`Command`]s and receives [`Event`]s on an
//! async channel, so an idle app wakes up only when something happens.

mod backup;
pub mod contacts;
pub mod discovery;
pub mod gofile;
pub mod identity;
mod limit;
pub mod net;
mod partial;
pub mod proto;
mod social;
mod swarm;
pub mod tls;
mod transfer;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

pub use contacts::Contact;
pub use identity::{AccountId, DeviceId};
pub use net::Reachability;
pub use proto::FileMeta;

#[derive(Clone, Debug, PartialEq)]
pub struct Peer {
    pub id: DeviceId,
    /// The device's name.
    pub name: String,
    pub addr: SocketAddr,
    /// The account (person) this device proved it belongs to.
    pub account: Option<AccountId>,
    /// That account's public key (from its signed announcement/record).
    pub account_key: Option<[u8; 32]>,
    /// More addresses to try (internet peers usually have several).
    pub alt: Vec<SocketAddr>,
    /// Found over the internet rather than on this network.
    pub remote: bool,
    /// Its NAT type (internet peers), for hole punching.
    pub nat: Option<net::nat::NatInfo>,
}

pub type TransferId = u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Outgoing,
    Incoming,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TransferState {
    Connecting,
    /// Outgoing: waiting for the recipient to accept. Incoming: waiting for you.
    Waiting,
    Active,
    /// Connection dropped. Outgoing retries on its own; incoming continues
    /// from the last verified block when the sender reconnects.
    Interrupted,
    Done { saved: Vec<PathBuf> },
    Declined,
    Cancelled,
    Failed(String),
}

impl TransferState {
    pub fn is_finished(&self) -> bool {
        matches!(self, Self::Done { .. } | Self::Declined | Self::Cancelled | Self::Failed(_))
    }
}

#[derive(Clone, Debug)]
pub struct TransferInfo {
    pub id: TransferId,
    /// Outgoing transfers created by the same Send share a job id.
    pub job: u64,
    pub direction: Direction,
    pub peer_id: DeviceId,
    pub peer_name: String,
    pub peer_account: Option<AccountId>,
    pub files: Arc<Vec<FileMeta>>,
    pub total: u64,
}

#[derive(Clone, Debug)]
pub enum Event {
    /// Who this device belongs to (sent at start and whenever it changes,
    /// e.g. after linking or restoring a backup).
    Identity { account: AccountId, person_name: String },
    Contacts(Vec<Contact>),
    Peers(Vec<Peer>),
    Transfer(TransferInfo),
    State { id: TransferId, state: TransferState },
    /// A transfer was paused or resumed: `local` by you, `remote` by the
    /// other side (show "Paused by <their name>").
    Paused { id: TransferId, local: bool, remote: bool },
    /// Bytes done so far (verified bytes on the receiving side), emitted at
    /// most ~8 times a second while active. Jumps ahead when a transfer resumes.
    Progress { id: TransferId, done: u64 },
    /// Someone nearby wants to become a contact. Answer with `Command::Respond`.
    PairRequest { id: u64, name: String, account: AccountId, device_name: String },
    /// A device wants to join your identity. Show `sas`; the other screen
    /// shows the same number. Answer with `Command::Respond`.
    LinkRequest { id: u64, device_name: String, sas: String },
    /// This device asked to join `with`'s identity; show `sas` until done.
    LinkPending { with: String, sas: String },
    LinkFinished,
    /// A pair/link request was answered, withdrawn or timed out.
    RequestClosed { id: u64 },
    /// How this device can be reached from the internet (changes rarely).
    Reachability(Reachability),
    /// Gofile tab updates (account, uploads, progress).
    Gofile(gofile::GofileEvent),
    /// A contact sent you a Gofile link.
    LinkShared { from: String, title: String, url: String },
    /// Something went well (shown neutrally).
    Info(String),
    /// Something went wrong.
    Notice(String),
}

#[derive(Debug)]
pub enum Command {
    Send { paths: Vec<PathBuf>, peers: Vec<Peer> },
    /// Accept/decline an incoming transfer, pair request or link request.
    Respond { id: u64, accept: bool },
    Cancel { id: TransferId },
    /// Pause or resume a transfer (either side can).
    Pause { id: TransferId, paused: bool },
    /// Speed limits in MB/s for everything Neo sends / receives (None =
    /// unlimited). Applies at once, also to running transfers; saved.
    SetLimits { up: Option<f64>, down: Option<f64> },
    UpdateSettings { name: String, person_name: String, downloads: PathBuf, close_to_tray: bool },
    /// Ask a nearby device's owner to become contacts.
    AddContact { device: DeviceId },
    /// Same, by the code they shared (or a `neo://add/…` link).
    AddByCode { code: String },
    RemoveContact { account: AccountId },
    SetAutoAccept { account: AccountId, on: bool },
    /// Join the identity of the nearby device with this code (one of your
    /// own devices), so both act as "you".
    LinkWith { code: String },
    ExportIdentity { path: PathBuf, passphrase: String },
    /// Gofile tab actions.
    Gofile(gofile::GofileCommand),
    ImportIdentity { path: PathBuf, passphrase: String },
    /// Test hook: closes every transfer connection as if the network had
    /// dropped (both sides then treat it as an interruption and resume).
    #[doc(hidden)]
    DropConnections,
}

/// User-editable settings, stored as `settings.json` in the data folder.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Settings {
    /// This device's name.
    pub name: Option<String>,
    /// Your name, shown to people you pair with.
    #[serde(default)]
    pub person_name: Option<String>,
    pub downloads: Option<PathBuf>,
    /// Closing the window keeps Neo running in the tray (default on).
    #[serde(default)]
    pub close_to_tray: Option<bool>,
    /// Upload / download limits in MB/s (absent = unlimited).
    #[serde(default)]
    pub upload_limit: Option<f64>,
    #[serde(default)]
    pub download_limit: Option<f64>,
}

impl Settings {
    pub fn load(data_dir: &Path) -> Self {
        std::fs::read(data_dir.join("settings.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self, data_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(data_dir)?;
        let tmp = data_dir.join("settings.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(tmp, data_dir.join("settings.json"))?;
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    /// This device's name.
    pub name: String,
    /// Your name, shown to contacts.
    pub person_name: String,
    pub data_dir: PathBuf,
    pub downloads: PathBuf,
    /// Accept every incoming offer without asking (headless use / tests).
    pub auto_accept: bool,
    pub close_to_tray: bool,
    /// Speed limits in MB/s (None = unlimited).
    pub upload_limit: Option<f64>,
    pub download_limit: Option<f64>,
    /// Find devices on this network via multicast.
    pub lan_discovery: bool,
    /// Reach contacts over the internet (DHT presence, STUN, UPnP, IPv6).
    pub internet: bool,
    /// Use these DHT nodes instead of the public bootstrap nodes (tests).
    pub dht_bootstrap: Option<Vec<String>>,
    /// Also publish private LAN addresses (helps when multicast is blocked).
    pub publish_lan: bool,
    /// Ask public STUN servers and the router (UPnP) about our public address.
    pub nat_traversal: bool,
}

impl Config {
    /// Default locations plus saved settings. A `profile` lets several
    /// instances share one machine.
    pub fn for_profile(profile: Option<&str>) -> Self {
        let appdata = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
        let home = std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
        let host = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "Neo device".into());
        let user = std::env::var("USERNAME").map(|u| title_case(&u)).unwrap_or_else(|_| "Me".into());
        let mut data_dir = appdata.join("Neo");
        let mut downloads = home.join("Downloads").join("Neo");
        let mut name = title_case(&host);
        if let Some(p) = profile {
            data_dir = data_dir.join("profiles").join(p);
            downloads = downloads.join(p);
            name = format!("{name} ({p})");
        }
        let saved = Settings::load(&data_dir);
        Config {
            name: saved.name.filter(|n| !n.trim().is_empty()).unwrap_or(name),
            person_name: saved.person_name.filter(|n| !n.trim().is_empty()).unwrap_or(user),
            downloads: saved.downloads.unwrap_or(downloads),
            close_to_tray: saved.close_to_tray.unwrap_or(true),
            upload_limit: saved.upload_limit.filter(|v| *v > 0.0),
            download_limit: saved.download_limit.filter(|v| *v > 0.0),
            data_dir,
            auto_accept: false,
            lan_discovery: true,
            internet: true,
            dht_bootstrap: None,
            publish_lan: true,
            nat_traversal: true,
        }
    }

    /// The UDP port used last time (stable ports keep published addresses valid).
    fn saved_port(&self) -> u16 {
        std::fs::read_to_string(self.data_dir.join("port")).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0)
    }

    pub(crate) fn partials_dir(&self) -> PathBuf {
        self.data_dir.join("partials")
    }

    fn settings(&self) -> Settings {
        Settings {
            name: Some(self.name.clone()),
            person_name: Some(self.person_name.clone()),
            downloads: Some(self.downloads.clone()),
            close_to_tray: Some(self.close_to_tray),
            upload_limit: self.upload_limit,
            download_limit: self.download_limit,
        }
    }
}

fn title_case(s: &str) -> String {
    let lower = s.to_lowercase();
    let mut c = lower.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

fn clean_name(s: &str) -> String {
    s.trim().chars().filter(|c| !c.is_control()).take(64).collect()
}

pub struct NodeHandle {
    pub me: DeviceId,
    /// Values at startup (the UI tracks later changes through events).
    pub account: AccountId,
    pub name: String,
    pub person_name: String,
    pub downloads: PathBuf,
    pub close_to_tray: bool,
    pub upload_limit: Option<f64>,
    pub download_limit: Option<f64>,
    pub events: async_channel::Receiver<Event>,
    commands: async_channel::Sender<Command>,
}

impl NodeHandle {
    pub fn command(&self, cmd: Command) {
        let _ = self.commands.try_send(cmd);
    }
}

/// Starts the engine on a background thread and returns once it is listening.
pub fn start(config: Config) -> Result<NodeHandle> {
    let identity = identity::Identity::load_or_create(&config.data_dir).context("loading device identity")?;
    let account = identity::load_or_create_account(&config.data_dir).context("loading account key")?;
    let me = identity.id;
    let (ev_tx, ev_rx) = async_channel::unbounded();
    let (cmd_tx, cmd_rx) = async_channel::unbounded();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();
    let handle = NodeHandle {
        me,
        account: account.id(),
        name: config.name.clone(),
        person_name: config.person_name.clone(),
        downloads: config.downloads.clone(),
        close_to_tray: config.close_to_tray,
        upload_limit: config.upload_limit,
        download_limit: config.download_limit,
        events: ev_rx,
        commands: cmd_tx,
    };
    let threads = std::thread::available_parallelism().map(|n| n.get().clamp(2, 4)).unwrap_or(2);
    std::thread::Builder::new().name("neo-net".into()).spawn(move || {
        let rt = match tokio::runtime::Builder::new_multi_thread()
            .worker_threads(threads)
            .max_blocking_threads(16)
            .thread_name("neo-io")
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                let _ = ready_tx.send(Err(e.into()));
                return;
            }
        };
        rt.block_on(async move {
            let shared = match Shared::bind(identity, account, config, ev_tx) {
                Ok(s) => s,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            let _ = ready_tx.send(Ok(()));
            shared.run(cmd_rx).await;
        });
    })?;
    ready_rx.recv().context("network thread exited")??;
    Ok(handle)
}

pub(crate) struct Active {
    pub progress: Arc<AtomicU64>,
    pub reported: u64,
    pub conn: Option<quinn::Connection>,
    pub cancelled: bool,
}

/// An incoming transfer that lost its connection and can be resumed.
pub(crate) struct Interrupted {
    pub id: TransferId,
}

pub(crate) struct Shared {
    pub identity: identity::Identity,
    pub account: RwLock<identity::Account>,
    pub contacts: Mutex<Vec<Contact>>,
    pub config: RwLock<Config>,
    pub endpoint: quinn::Endpoint,
    pub events: async_channel::Sender<Event>,
    pub peers: Mutex<Vec<Peer>>,
    pub active: Mutex<HashMap<TransferId, Active>>,
    pub pending: Mutex<HashMap<u64, tokio::sync::oneshot::Sender<bool>>>,
    /// Keyed by a fingerprint of (sender, files) so a resumed offer reuses its card.
    pub interrupted: Mutex<HashMap<[u8; 32], Interrupted>>,
    /// Incoming batches in progress, so a sender that reconnects before we
    /// noticed its old connection died can take over cleanly.
    pub receiving: Mutex<HashMap<[u8; 32], (quinn::Connection, tokio::sync::watch::Receiver<bool>)>>,
    pub wake_ticker: tokio::sync::Notify,
    pub reach: Arc<net::Reach>,
    pub presence: Option<Arc<net::presence::Presence>>,
    /// Found by multicast on this network / via the DHT; `peers` is the merge.
    lan_peers: Mutex<Vec<Peer>>,
    remote_peers: Mutex<Vec<Peer>>,
    wake_publish: tokio::sync::Notify,
    wake_lookup: tokio::sync::Notify,
    /// Addresses to send packets to for a while because someone knocked.
    punch: Mutex<Vec<(SocketAddr, Instant)>>,
    /// Ourselves, for tasks spawned from `&self` methods.
    me: std::sync::OnceLock<std::sync::Weak<Shared>>,
    /// Batches being received as part of a swarm, for links from co-recipients.
    pub swarms: Mutex<HashMap<[u8; 16], Arc<swarm::SwarmRecv>>>,
    pub gofile: gofile::Gofile,
    /// Shared upload/download speed limits.
    pub limits: Arc<limit::Limits>,
    /// Pause state per transfer (kept across reconnects).
    gates: Mutex<HashMap<TransferId, Arc<limit::Gate>>>,
    next_id: AtomicU64,
}

impl Shared {
    fn bind(
        identity: identity::Identity,
        account: identity::Account,
        config: Config,
        events: async_channel::Sender<Event>,
    ) -> Result<Arc<Self>> {
        let server = tls::server_config(&identity)?;
        let runtime: Arc<dyn quinn::Runtime> = Arc::new(quinn::TokioRuntime);
        let socket = match net::socket::NeoSocket::bind(runtime.as_ref(), config.saved_port()) {
            Ok(s) => s,
            Err(_) => net::socket::NeoSocket::bind(runtime.as_ref(), 0).context("opening UDP socket")?,
        };
        let _ = std::fs::write(config.data_dir.join("port"), socket.port().to_string());
        let endpoint = quinn::Endpoint::new_with_abstract_socket(
            quinn::EndpointConfig::default(),
            Some(server),
            socket.clone(),
            runtime,
        )
        .context("opening QUIC endpoint")?;
        partial::prune(&config.partials_dir());
        let contacts = contacts::load(&config.data_dir);
        let gofile = gofile::Gofile::load(&config.data_dir);
        let limits = Arc::new(limit::Limits::new(limit::to_rate(config.upload_limit), limit::to_rate(config.download_limit)));
        let presence = if config.internet {
            net::presence::Presence::start(config.dht_bootstrap.as_deref()).ok().map(Arc::new)
        } else {
            None
        };
        let shared = Arc::new(Shared {
            identity,
            account: RwLock::new(account),
            contacts: Mutex::new(contacts),
            config: RwLock::new(config),
            endpoint,
            events,
            peers: Mutex::new(Vec::new()),
            active: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            interrupted: Mutex::new(HashMap::new()),
            receiving: Mutex::new(HashMap::new()),
            wake_ticker: tokio::sync::Notify::new(),
            reach: net::Reach::new(socket),
            presence,
            lan_peers: Mutex::new(Vec::new()),
            remote_peers: Mutex::new(Vec::new()),
            wake_publish: tokio::sync::Notify::new(),
            wake_lookup: tokio::sync::Notify::new(),
            punch: Mutex::new(Vec::new()),
            me: std::sync::OnceLock::new(),
            swarms: Mutex::new(HashMap::new()),
            gofile,
            limits,
            gates: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        });
        let _ = shared.me.set(Arc::downgrade(&shared));
        Ok(shared)
    }

    pub fn config(&self) -> Config {
        self.config.read().unwrap().clone()
    }

    pub fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    pub fn emit(&self, ev: Event) {
        let _ = self.events.try_send(ev);
    }

    pub fn my_account(&self) -> AccountId {
        self.account.read().unwrap().id()
    }

    pub fn attest(&self) -> identity::Attest {
        self.account.read().unwrap().attest(self.identity.id)
    }

    /// Files from this account are accepted without asking: your own devices,
    /// and contacts you marked that way.
    pub fn trusts(&self, account: Option<AccountId>) -> bool {
        let Some(account) = account else { return false };
        account == self.my_account() || self.contacts.lock().unwrap().iter().any(|c| c.account == account && c.auto_accept)
    }

    pub fn emit_identity(&self) {
        self.emit(Event::Identity { account: self.my_account(), person_name: self.config().person_name });
    }

    /// Changes the contact list, saves it and tells the UI.
    pub fn update_contacts(&self, f: impl FnOnce(&mut Vec<Contact>)) {
        let list = {
            let mut contacts = self.contacts.lock().unwrap();
            f(&mut contacts);
            contacts.clone()
        };
        if let Err(e) = contacts::save(&self.config().data_dir, &list) {
            self.emit(Event::Notice(format!("Couldn't save contacts: {e}")));
        }
        self.emit(Event::Contacts(list));
        self.wake_lookup.notify_one();
    }

    /// The current view of a device (its addresses may have changed).
    pub fn peer(&self, id: DeviceId) -> Option<Peer> {
        self.peers.lock().unwrap().iter().find(|p| p.id == id).cloned()
    }

    /// Recomputes `peers` from LAN + internet finds; tells the UI if changed.
    fn merge_peers(&self) {
        let lan = self.lan_peers.lock().unwrap().clone();
        let mut merged = lan.clone();
        for r in self.remote_peers.lock().unwrap().iter() {
            if !lan.iter().any(|p| p.id == r.id) {
                merged.push(r.clone());
            }
        }
        merged.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then(a.id.cmp(&b.id)));
        let changed = {
            let mut peers = self.peers.lock().unwrap();
            let changed = *peers != merged;
            *peers = merged.clone();
            changed
        };
        if changed {
            self.emit(Event::Peers(merged));
        }
    }

    fn set_lan_peers(&self, peers: Vec<Peer>) {
        // Remember contacts' full keys when we see them nearby.
        let missing: Vec<(AccountId, String)> = {
            let contacts = self.contacts.lock().unwrap();
            peers
                .iter()
                .filter_map(|p| Some((p.account?, p.account_key?)))
                .filter(|(a, _)| contacts.iter().any(|c| c.account == *a && c.public.is_none()))
                .map(|(a, k)| (a, identity::hex(&k)))
                .collect()
        };
        if !missing.is_empty() {
            self.update_contacts(|c| {
                for (a, k) in missing {
                    contacts::upsert(c, a, "", Some(&k));
                }
            });
        }
        *self.lan_peers.lock().unwrap() = peers;
        self.merge_peers();
    }

    /// Connects to a device, trying all its known addresses at once (LAN,
    /// router-mapped, IPv6, public) and keeping the first that works.
    pub async fn connect_peer(&self, peer: &Peer) -> Result<quinn::Connection> {
        let first = self.try_connect(peer).await;
        let (Err(err), true, Some(key), Some(presence)) = (&first, peer.remote, peer.account_key, self.presence.clone()) else {
            return first;
        };
        // Their router drops packets from strangers. Knock through the DHT
        // (with our NAT type and a punch token) so their side joins in, then
        // punch with the strategy both NAT types call for (`net/punch.rs`),
        // while plain retries keep going for the easy cases. No relay.
        debug(&format!("direct connect to {} failed ({err:#}); knocking and punching", peer.name));
        let reach = self.reach.get();
        let mine = reach.candidates(self.config().publish_lan);
        let mut token = [0u8; 8];
        let _ = ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut token);
        let _ = presence.knock(&key, mine, Some(reach.nat.clone()), Some(token)).await;
        let mut addrs = vec![peer.addr];
        addrs.extend(peer.alt.iter().copied());
        let plan = net::punch::plan(&reach.nat, &peer.nat.clone().unwrap_or_default(), &addrs, u64::from_le_bytes(token));
        debug(&format!("punch plan: {} known, {} guesses, {} extra sockets", plan.base.len(), plan.spray.len(), plan.extras));
        let retry = async {
            loop {
                tokio::time::sleep(Duration::from_secs(3)).await;
                if let Ok(c) = self.try_connect(peer).await {
                    return c;
                }
            }
        };
        let found = tokio::select! {
            c = retry => return Ok(c),
            f = net::punch::run(&self.reach.socket, token, &plan, PUNCH_INITIATOR) => f,
        };
        let cfg = tls::client_config(&self.identity, peer.id)?;
        let connect = match found {
            Some(net::punch::Found::Main(addr)) => {
                debug(&format!("punched through to {addr} (main socket)"));
                self.endpoint.connect_with(cfg, addr, tls::SERVER_NAME)?
            }
            Some(net::punch::Found::Extra(sock, addr)) => {
                debug(&format!("punched through to {addr} (birthday socket)"));
                self.adopt_socket(sock)?.connect_with(cfg, addr, tls::SERVER_NAME)?
            }
            None => {
                return Err(anyhow!(
                    "{}'s network and yours both block direct connections (strict routers or carrier-grade NAT). \
                     It may work over IPv6, from a different network (for example a phone hotspot), or if either \
                     router allows UPnP or NAT-PMP.",
                    peer.name
                ))
            }
        };
        Ok(tokio::time::timeout(Duration::from_secs(10), connect).await.map_err(|_| anyhow!("connection timed out"))??)
    }

    /// Turns a socket that punched through into a QUIC endpoint (it can both
    /// accept and connect). It closes itself once idle for two minutes.
    fn adopt_socket(&self, sock: std::net::UdpSocket) -> Result<quinn::Endpoint> {
        let server = tls::server_config(&self.identity)?;
        let ep = quinn::Endpoint::new(quinn::EndpointConfig::default(), Some(server), sock, Arc::new(quinn::TokioRuntime))?;
        if let Some(me) = self.me.get().and_then(std::sync::Weak::upgrade) {
            let ep2 = ep.clone();
            tokio::spawn(async move {
                let mut idle = 0;
                loop {
                    tokio::select! {
                        inc = ep2.accept() => match inc {
                            Some(incoming) => {
                                let me = me.clone();
                                tokio::spawn(async move { route(me, incoming).await });
                            }
                            None => break,
                        },
                        _ = tokio::time::sleep(Duration::from_secs(30)) => {
                            idle = if ep2.open_connections() == 0 { idle + 1 } else { 0 };
                            if idle >= 4 {
                                ep2.close(0u32.into(), b"idle");
                                break;
                            }
                        }
                    }
                }
            });
        }
        Ok(ep)
    }

    async fn try_connect(&self, peer: &Peer) -> Result<quinn::Connection> {
        let mut addrs = vec![peer.addr];
        for a in &peer.alt {
            if !addrs.contains(a) {
                addrs.push(*a);
            }
        }
        if addrs.len() == 1 && !peer.remote {
            return self.connect(peer.id, peer.addr).await;
        }
        // Open our side of the NAT toward every candidate first (hole punching;
        // the other side does the same while we're in each other's contacts).
        for a in &addrs {
            self.reach.socket.poke(*a);
        }
        let mut set = tokio::task::JoinSet::new();
        for (i, a) in addrs.into_iter().enumerate() {
            let cfg = tls::client_config(&self.identity, peer.id)?;
            let connecting = self.endpoint.connect_with(cfg, a, tls::SERVER_NAME);
            set.spawn(async move {
                tokio::time::sleep(Duration::from_millis(150 * i as u64)).await;
                Ok::<_, anyhow::Error>(connecting?.await?)
            });
        }
        let mut last = None;
        while let Some(r) = set.join_next().await {
            match r {
                Ok(Ok(conn)) => {
                    set.abort_all();
                    return Ok(conn);
                }
                Ok(Err(e)) => last = Some(e),
                Err(_) => {}
            }
        }
        Err(last.unwrap_or_else(|| anyhow!("no address worked")))
    }

    /// Finds the person with this code over the internet (DHT): resolves the
    /// code to their key, then their devices' current addresses.
    async fn find_by_code_online(&self, code: &str) -> Option<Peer> {
        let presence = self.presence.clone()?;
        let typed = code.trim();
        let typed = typed.rsplit_once("neo://add/").map(|(_, c)| c).unwrap_or(typed);
        let t0 = Instant::now();
        // DHT writes take a moment to spread; try a few times.
        let mut public = None;
        for attempt in 0..3 {
            public = presence.resolve_code(typed).await;
            debug(&format!("resolve {typed} (try {attempt}): {} in {:?}", public.is_some(), t0.elapsed()));
            if public.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
        let public = public?;
        let entries = presence.lookup(&public, PRESENCE_FRESH).await;
        debug(&format!("lookup: {entries:?} in {:?}", t0.elapsed()));
        entries.into_iter().filter(|e| e.id != self.identity.id).find_map(|e| remote_peer(e, public))
    }

    // ------------------------------------------------------------- internet

    /// Keeps reachability fresh: interfaces every minute, STUN every 5
    /// minutes, the router mapping every 30 (its lease is an hour).
    async fn reach_loop(self: Arc<Self>) {
        let mut last_stun: Option<Instant> = None;
        let mut last_map: Option<Instant> = None;
        let nat = self.config().nat_traversal;
        loop {
            let mut changed = self.reach.refresh_interfaces();
            if nat && last_stun.is_none_or(|t| t.elapsed() > Duration::from_secs(300)) {
                changed |= self.reach.refresh_stun().await;
                last_stun = Some(Instant::now());
            }
            if nat && last_map.is_none_or(|t| t.elapsed() > Duration::from_secs(1800)) {
                changed |= self.reach.refresh_mapping().await;
                last_map = Some(Instant::now());
            }
            if changed {
                self.emit(Event::Reachability(self.reach.get()));
                self.wake_publish.notify_one();
            }
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    }

    /// Publishes where this device can be reached, every 5 minutes or when
    /// that changes, plus the code record every 30 minutes.
    async fn publish_loop(self: Arc<Self>, presence: Arc<net::presence::Presence>) {
        while !presence.ready().await {
            tokio::time::sleep(Duration::from_secs(15)).await;
        }
        if self.reach.set_dht(true) {
            self.emit(Event::Reachability(self.reach.get()));
        }
        self.wake_lookup.notify_one();
        let mut last_code: Option<Instant> = None;
        loop {
            let cfg = self.config();
            let addrs = self.reach.get().candidates(cfg.publish_lan);
            if !addrs.is_empty() {
                let account = self.account.read().unwrap().clone();
                let nat = self.reach.get().nat;
                let me = net::presence::DeviceEntry {
                    id: self.identity.id,
                    name: cfg.name.chars().take(32).collect(),
                    addrs,
                    t: net::presence::now(),
                    nat: (nat.kind != net::nat::NatKind::Unknown).then_some(nat),
                };
                let t0 = Instant::now();
                // The code record first: it's what lets people add you by code.
                if last_code.is_none_or(|t| t.elapsed() > Duration::from_secs(1800)) {
                    let r = presence.publish_code(&account).await;
                    debug(&format!("publish code {}: {r:?} in {:?}", account.id().code(), t0.elapsed()));
                    if r.is_ok() {
                        last_code = Some(Instant::now());
                    }
                }
                let r = presence.publish(&account, me).await;
                debug(&format!("publish presence: {r:?} in {:?}", t0.elapsed()));
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(300)) => {}
                _ = self.wake_publish.notified() => tokio::time::sleep(Duration::from_secs(2)).await,
            }
        }
    }

    /// Looks up contacts (and your other devices) in the DHT every 90 s, or
    /// when contacts change.
    async fn lookup_loop(self: Arc<Self>, presence: Arc<net::presence::Presence>) {
        loop {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(90)) => {}
                _ = self.wake_lookup.notified() => {}
            }
            if !self.reach.get().dht {
                continue;
            }
            let mut keys: Vec<[u8; 32]> = self
                .contacts
                .lock()
                .unwrap()
                .iter()
                .filter_map(|c| identity::unhex(c.public.as_deref()?)?.try_into().ok())
                .collect();
            keys.push(self.account.read().unwrap().public());
            let mut set = tokio::task::JoinSet::new();
            for key in keys {
                let presence = presence.clone();
                set.spawn(async move { (key, presence.lookup(&key, PRESENCE_FRESH).await) });
            }
            let mut found = Vec::new();
            while let Some(Ok((key, entries))) = set.join_next().await {
                found.extend(entries.into_iter().filter(|e| e.id != self.identity.id).filter_map(|e| remote_peer(e, key)));
            }
            *self.remote_peers.lock().unwrap() = found;
            self.merge_peers();
        }
    }

    /// Every 15 s, sends a tiny packet to each internet peer's addresses so
    /// both NATs keep a path open (they do the same toward us).
    async fn poke_loop(self: Arc<Self>) {
        let mut tick = 0u32;
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            tick += 1;
            let targets: Vec<SocketAddr> = {
                let mut punch = self.punch.lock().unwrap();
                punch.retain(|(_, until)| *until > Instant::now());
                punch.iter().map(|(a, _)| *a).collect()
            };
            for a in targets {
                self.reach.socket.poke(a);
            }
            if tick % 8 == 0 {
                let peers = self.remote_peers.lock().unwrap().clone();
                for p in peers {
                    self.reach.socket.poke(p.addr);
                    for a in p.alt {
                        self.reach.socket.poke(a);
                    }
                }
            }
        }
    }

    /// Checks every 15 s whether someone knocked; if so, opens a path toward
    /// them for 90 s and joins their punch session.
    async fn knock_loop(self: Arc<Self>, presence: Arc<net::presence::Presence>) {
        let mut seen = 0u64;
        loop {
            tokio::time::sleep(Duration::from_secs(15)).await;
            if !self.reach.get().dht {
                continue;
            }
            let me = self.account.read().unwrap().public();
            if let Some(knock) = presence.knocks(&me, Duration::from_secs(120)).await {
                if knock.t > seen {
                    seen = knock.t;
                    debug(&format!("knocked by {:?} ({:?})", knock.addrs, knock.nat));
                    let token: Option<[u8; 8]> =
                        knock.token.as_deref().and_then(identity::unhex).and_then(|t| t.try_into().ok());
                    if let Some(token) = token {
                        let my_nat = self.reach.get().nat;
                        let plan = net::punch::plan(&my_nat, &knock.nat.clone().unwrap_or_default(), &knock.addrs, u64::from_le_bytes(token) ^ 0x9e37);
                        let s = self.clone();
                        tokio::spawn(async move {
                            if let Some(net::punch::Found::Extra(sock, addr)) =
                                net::punch::run(&s.reach.socket, token, &plan, PUNCH_RESPONDER).await
                            {
                                debug(&format!("punched through to {addr} (birthday socket); waiting for them"));
                                let _ = s.adopt_socket(sock);
                            }
                        });
                    }
                    let until = Instant::now() + Duration::from_secs(90);
                    let mut punch = self.punch.lock().unwrap();
                    for a in knock.addrs.into_iter().take(6) {
                        self.reach.socket.poke(a);
                        punch.push((a, until));
                    }
                }
            }
        }
    }

    pub fn save_settings(&self) {
        let c = self.config();
        if let Err(e) = c.settings().save(&c.data_dir) {
            self.emit(Event::Notice(format!("Couldn't save settings: {e:#}")));
        }
    }

    /// Opens a connection to `device`, pinned to its identity.
    pub async fn connect(&self, device: DeviceId, addr: SocketAddr) -> Result<quinn::Connection> {
        let cfg = tls::client_config(&self.identity, device)?;
        Ok(self.endpoint.connect_with(cfg, addr, tls::SERVER_NAME)?.await?)
    }

    /// A nearby device by the code someone shared: their account code
    /// (preferred) or a device code. Accepts `neo://add/CODE` links too.
    pub fn find_by_code(&self, typed: &str) -> Option<Peer> {
        let typed = typed.trim();
        let typed = typed.rsplit_once("neo://add/").map(|(_, c)| c).unwrap_or(typed);
        let code = identity::normalize_code(typed);
        if code.len() != 12 {
            return None;
        }
        let peers = self.peers.lock().unwrap();
        peers
            .iter()
            .find(|p| p.account.is_some_and(|a| a.matches_code(&code)))
            .or_else(|| peers.iter().find(|p| identity::normalize_code(&p.id.code()) == code))
            .cloned()
    }

    pub fn set_state(&self, id: TransferId, state: TransferState) {
        if state.is_finished() || state == TransferState::Interrupted {
            self.flush_progress(id);
        }
        if state.is_finished() {
            self.active.lock().unwrap().remove(&id);
            self.pending.lock().unwrap().remove(&id);
            if let Some(g) = self.gates.lock().unwrap().remove(&id) {
                g.poke();
            }
        }
        self.emit(Event::State { id, state });
    }

    fn flush_progress(&self, id: TransferId) {
        let mut active = self.active.lock().unwrap();
        if let Some(a) = active.get_mut(&id) {
            let done = a.progress.load(Ordering::Relaxed);
            if done != a.reported {
                a.reported = done;
                self.emit(Event::Progress { id, done });
            }
        }
    }

    /// Registers a transfer for progress reporting and returns its byte
    /// counter. With `announce`, the UI is told about a new transfer;
    /// without, an existing (interrupted) card is reused.
    pub fn register(&self, info: TransferInfo, announce: bool) -> Arc<AtomicU64> {
        let progress = Arc::new(AtomicU64::new(0));
        let id = info.id;
        self.active.lock().unwrap().insert(id, Active { progress: progress.clone(), reported: u64::MAX, conn: None, cancelled: false });
        if announce {
            self.emit(Event::Transfer(info));
        }
        self.wake_ticker.notify_one();
        progress
    }

    pub fn attach_conn(&self, id: TransferId, conn: &quinn::Connection) {
        if let Some(a) = self.active.lock().unwrap().get_mut(&id) {
            a.conn = Some(conn.clone());
        }
    }

    /// This transfer's pause state.
    pub(crate) fn gate(&self, id: TransferId) -> Arc<limit::Gate> {
        self.gates.lock().unwrap().entry(id).or_default().clone()
    }

    fn emit_pause(&self, id: TransferId, gate: &limit::Gate) {
        let (local, remote) = gate.get();
        self.emit(Event::Paused { id, local, remote });
    }

    /// The other side paused or resumed (or a new connection reset it).
    pub(crate) fn set_remote_pause(&self, id: TransferId, on: bool) {
        let gate = self.gate(id);
        if gate.set_remote(on) {
            self.emit_pause(id, &gate);
        }
    }

    fn pause(&self, id: TransferId, on: bool) {
        let active = self.active.lock().unwrap().contains_key(&id);
        let known = active || self.interrupted.lock().unwrap().values().any(|i| i.id == id);
        if !known {
            return;
        }
        let gate = self.gate(id);
        if gate.set_local(on) {
            self.emit_pause(id, &gate);
        }
    }

    /// True once the transfer was cancelled locally (or already finished).
    pub fn is_gone(&self, id: TransferId) -> bool {
        self.active.lock().unwrap().get(&id).map(|a| a.cancelled).unwrap_or(true)
    }

    fn cancel(&self, id: TransferId) {
        // An interrupted incoming transfer: drop its partial files.
        let stale = {
            let mut map = self.interrupted.lock().unwrap();
            let fp = map.iter().find(|(_, v)| v.id == id).map(|(k, _)| *k);
            if let Some(fp) = fp {
                map.remove(&fp);
            }
            fp
        };
        if let Some(fp) = stale {
            partial::discard(&self.config().partials_dir(), &fp);
            self.set_state(id, TransferState::Cancelled);
            return;
        }
        let conn = {
            let mut active = self.active.lock().unwrap();
            active.get_mut(&id).map(|a| {
                a.cancelled = true;
                a.conn.clone()
            })
        };
        let Some(conn) = conn else { return };
        if let Some(g) = self.gates.lock().unwrap().get(&id) {
            g.poke(); // wake anything waiting on a pause
        }
        if let Some(tx) = self.pending.lock().unwrap().remove(&id) {
            let _ = tx.send(false);
        }
        if let Some(conn) = conn {
            conn.close(transfer::CANCELLED, b"cancelled");
        }
        self.set_state(id, TransferState::Cancelled);
    }

    async fn run(self: Arc<Self>, commands: async_channel::Receiver<Command>) {
        let port = self.endpoint.local_addr().map(|a| a.port()).unwrap_or(0);
        self.emit_identity();
        self.emit(Event::Contacts(self.contacts.lock().unwrap().clone()));
        self.emit(Event::Reachability(self.reach.get()));
        for e in self.gofile.initial_events() {
            self.emit(e);
        }

        // Internet: reachability, presence publishing, contact lookups, keepalives.
        if self.config().internet {
            tokio::spawn(self.clone().reach_loop());
            tokio::spawn(self.clone().poke_loop());
            if let Some(presence) = self.presence.clone() {
                tokio::spawn(self.clone().publish_loop(presence.clone()));
                tokio::spawn(self.clone().knock_loop(presence.clone()));
                tokio::spawn(self.clone().lookup_loop(presence));
            }
        }

        // LAN discovery.
        let s = self.clone();
        let lan = self.config().lan_discovery;
        tokio::spawn(async move {
            if !lan {
                return;
            }
            let (s1, s2) = (s.clone(), s.clone());
            let result = discovery::run(
                s.identity.id,
                move || (s1.config.read().unwrap().name.clone(), s1.attest()),
                port,
                move |peers| s2.set_lan_peers(peers),
            )
            .await;
            if let Err(e) = result {
                s.emit(Event::Notice(format!("Local discovery unavailable: {e}")));
            }
        });

        // Progress ticker: parks on a Notify while nothing is active.
        let s = self.clone();
        tokio::spawn(async move {
            loop {
                let idle = s.active.lock().unwrap().is_empty();
                if idle {
                    s.wake_ticker.notified().await;
                    continue;
                }
                tokio::time::sleep(std::time::Duration::from_millis(125)).await;
                let mut updates = Vec::new();
                for (id, a) in s.active.lock().unwrap().iter_mut() {
                    let done = a.progress.load(Ordering::Relaxed);
                    if done != a.reported {
                        a.reported = done;
                        updates.push(Event::Progress { id: *id, done });
                    }
                }
                for u in updates {
                    s.emit(u);
                }
            }
        });

        // Incoming connections: the first control frame says what they're for.
        let s = self.clone();
        tokio::spawn(async move {
            while let Some(incoming) = s.endpoint.accept().await {
                let s = s.clone();
                tokio::spawn(async move { route(s, incoming).await });
            }
        });

        while let Ok(cmd) = commands.recv().await {
            self.clone().handle(cmd);
        }
    }

    fn handle(self: Arc<Self>, cmd: Command) {
        match cmd {
            Command::Send { paths, peers } => {
                tokio::spawn(async move { transfer::send_job(self, paths, peers).await });
            }
            Command::Respond { id, accept } => {
                if let Some(tx) = self.pending.lock().unwrap().remove(&id) {
                    let _ = tx.send(accept);
                }
            }
            Command::Cancel { id } => self.cancel(id),
            Command::Pause { id, paused } => self.pause(id, paused),
            Command::DropConnections => {
                let conns: Vec<quinn::Connection> = self.active.lock().unwrap().values().filter_map(|a| a.conn.clone()).collect();
                for c in conns {
                    c.close(4u32.into(), b"network dropped (test)");
                }
            }
            Command::SetLimits { up, down } => {
                let (up, down) = (up.filter(|v| *v > 0.0), down.filter(|v| *v > 0.0));
                self.limits.up.set_rate(limit::to_rate(up));
                self.limits.down.set_rate(limit::to_rate(down));
                {
                    let mut c = self.config.write().unwrap();
                    c.upload_limit = up;
                    c.download_limit = down;
                }
                self.save_settings();
            }
            Command::UpdateSettings { name, person_name, downloads, close_to_tray } => {
                {
                    let mut c = self.config.write().unwrap();
                    let name = clean_name(&name);
                    if !name.is_empty() {
                        c.name = name;
                    }
                    let person = clean_name(&person_name);
                    if !person.is_empty() {
                        c.person_name = person;
                    }
                    c.downloads = downloads;
                    c.close_to_tray = close_to_tray;
                }
                self.save_settings();
            }
            Command::AddContact { device } => {
                let peer = self.peers.lock().unwrap().iter().find(|p| p.id == device).cloned();
                match peer {
                    Some(peer) => {
                        tokio::spawn(async move { social::request_pair(self, peer).await });
                    }
                    None => self.emit(Event::Notice("That device isn't nearby anymore".into())),
                }
            }
            Command::AddByCode { code } => match self.find_by_code(&code) {
                Some(peer) => {
                    tokio::spawn(async move { social::request_pair(self, peer).await });
                }
                None if self.presence.is_some() => {
                    self.emit(Event::Info(format!("Looking for {} over the internet…", code.trim())));
                    tokio::spawn(async move {
                        match self.find_by_code_online(&code).await {
                            Some(peer) => social::request_pair(self, peer).await,
                            None => self.emit(Event::Notice(format!(
                                "Couldn't find {} nearby or online. They need Neo open; check the code.",
                                code.trim()
                            ))),
                        }
                    });
                }
                None => self.emit(Event::Notice(format!(
                    "No one with code {} is nearby. They need Neo open on this network.",
                    code.trim()
                ))),
            },
            Command::RemoveContact { account } => self.update_contacts(|c| c.retain(|x| x.account != account)),
            Command::SetAutoAccept { account, on } => self.update_contacts(|c| {
                if let Some(x) = c.iter_mut().find(|x| x.account == account) {
                    x.auto_accept = on;
                }
            }),
            Command::LinkWith { code } => match self.find_by_code(&code) {
                Some(peer) => {
                    tokio::spawn(async move { social::request_link(self, peer).await });
                }
                None if self.presence.is_some() => {
                    self.emit(Event::Info(format!("Looking for {} over the internet…", code.trim())));
                    tokio::spawn(async move {
                        match self.find_by_code_online(&code).await {
                            Some(peer) => social::request_link(self, peer).await,
                            None => self.emit(Event::Notice(format!(
                                "Couldn't find a device with code {} nearby or online. Open Neo on it.",
                                code.trim()
                            ))),
                        }
                    });
                }
                None => self.emit(Event::Notice(format!(
                    "No device with code {} is nearby. Open Neo on it, on this network.",
                    code.trim()
                ))),
            },
            Command::Gofile(cmd) => {
                tokio::spawn(gofile::handle(self, cmd));
            }
            Command::ExportIdentity { path, passphrase } => {
                tokio::task::spawn_blocking(move || {
                    let contacts = self.contacts.lock().unwrap().clone();
                    let result = {
                        let account = self.account.read().unwrap();
                        backup::export(&path, &passphrase, &account, &self.config().person_name, &contacts)
                    };
                    self.emit(match result {
                        Ok(()) => Event::Info(format!("Backup saved to {}", path.display())),
                        Err(e) => Event::Notice(format!("Backup failed: {e:#}")),
                    });
                });
            }
            Command::ImportIdentity { path, passphrase } => {
                tokio::task::spawn_blocking(move || match backup::import(&path, &passphrase) {
                    Ok(restored) => {
                        let data_dir = self.config().data_dir;
                        if let Err(e) = identity::save_account(&data_dir, &restored.account) {
                            return self.emit(Event::Notice(format!("Couldn't save the restored identity: {e:#}")));
                        }
                        *self.account.write().unwrap() = restored.account;
                        if !restored.person_name.trim().is_empty() {
                            self.config.write().unwrap().person_name = clean_name(&restored.person_name);
                            self.save_settings();
                        }
                        self.update_contacts(|c| contacts::merge(c, restored.contacts));
                        self.emit_identity();
                        self.emit(Event::Info("Identity restored. Your contacts will recognize this device as you.".into()));
                    }
                    Err(e) => self.emit(Event::Notice(format!("Restore failed: {e:#}"))),
                });
            }
        }
    }
}

/// Reads the first control frame of an incoming connection and dispatches it.
async fn route(shared: Arc<Shared>, incoming: quinn::Incoming) {
    let Ok(conn) = incoming.await else { return };
    let Some(peer_id) = tls::peer_id(&conn) else { return };
    let Ok((ctl_tx, mut ctl_rx)) = conn.accept_bi().await else { return };
    let Ok(first) = proto::read_frame::<proto::Msg>(&mut ctl_rx).await else { return };
    match first {
        proto::Msg::Offer { sender_name, files, total, attest, swarm, paused } => {
            let account = attest.and_then(|a| a.verify(peer_id));
            let offer = transfer::Offer { sender_name, files, total, swarm, paused };
            transfer::receive_offer(shared, conn, peer_id, account, ctl_tx, ctl_rx, offer).await
        }
        proto::Msg::ShareLink { title, url, attest } => social::handle_shared_link(shared, conn, peer_id, ctl_tx, title, url, attest).await,
        proto::Msg::SwarmHello { batch, proof } => swarm::handle_hello(shared, conn, peer_id, ctl_tx, ctl_rx, batch, proof).await,
        proto::Msg::Pair { name, attest } => social::handle_pair(shared, conn, peer_id, ctl_tx, name, attest).await,
        proto::Msg::LinkRequest { device_name } => social::handle_link(shared, conn, peer_id, ctl_tx, device_name).await,
        _ => conn.close(0u32.into(), b"unexpected"),
    }
}

/// How long each side punches. The caller starts first and the other side
/// joins when it next checks for knocks (every 15 s), so the caller waits longer.
const PUNCH_INITIATOR: Duration = Duration::from_secs(35);
const PUNCH_RESPONDER: Duration = Duration::from_secs(20);

/// How recent a device's published presence must be to count as online.
const PRESENCE_FRESH: Duration = Duration::from_secs(12 * 60);

fn remote_peer(entry: net::presence::DeviceEntry, public: [u8; 32]) -> Option<Peer> {
    let mut addrs = entry.addrs.into_iter();
    let addr = addrs.next()?;
    Some(Peer {
        id: entry.id,
        name: entry.name,
        addr,
        account: Some(AccountId::of_public(&public)),
        account_key: Some(public),
        alt: addrs.collect(),
        remote: true,
        nat: entry.nat,
    })
}

/// Diagnostics to stderr when `NEO_DEBUG` is set.
pub(crate) fn debug(msg: &str) {
    if std::env::var_os("NEO_DEBUG").is_some() {
        eprintln!("[neo] {msg}");
    }
}
