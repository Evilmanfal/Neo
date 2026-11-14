//! Headless Neo node, for scripting and tests.
//!
//!   neo-cli receive [--profile P] [--downloads DIR] [--exit-after N]
//!   neo-cli send    [--profile P] [--peers N] FILE_OR_FOLDER...
//!   neo-cli pair    [--profile P] [--person NAME] CODE
//!   neo-cli link    [--profile P] CODE
//!   neo-cli sign-release [--key FILE] neo.exe
//!
//! `--no-lan` turns off LAN discovery and `--no-private` stops publishing
//! LAN addresses, to exercise the internet path on one machine.
//!
//! `receive` auto-accepts every offer. `send` waits until N peers are visible
//! on the LAN, then sends to all of them at once and exits when done. Only
//! contacts and your own devices are visible on the LAN unless both sides
//! run with `--discover-all` (testing only; it announces this device readably
//! to everyone on the network).
//! `pair` asks the person with CODE to become contacts; `link` asks the
//! device with CODE to share its identity with this one (printing the number
//! to compare). Both exit once answered.
//!
//! `sign-release` writes `neo.exe.sig` next to the exe for a GitHub release,
//! with the release key in `%APPDATA%\Neo-release\release.key` (created on
//! first use; keep it private and out of the repo). It prints the public key,
//! which must match `update::RELEASE_KEY` in the app.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use neo_core::{Command, Config, Direction, Event, TransferState};

fn sign_release(args: Vec<String>) -> anyhow::Result<()> {
    let mut key = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_default().join("Neo-release").join("release.key");
    let mut exe = None;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--key" => key = it.next().map(PathBuf::from).unwrap_or(key),
            _ => exe = Some(PathBuf::from(a)),
        }
    }
    let exe = exe.ok_or_else(|| anyhow::anyhow!("usage: neo-cli sign-release [--key FILE] neo.exe"))?;
    let new_key = !key.exists();
    let (sig, public) = neo_core::update::sign(&exe, &key)?;
    let mut out = exe.clone().into_os_string();
    out.push(".sig");
    std::fs::write(&out, &sig)?;
    let hex: String = public.iter().map(|b| format!("{b:02x}")).collect();
    if new_key {
        println!("Created a new release key at {} (keep it private, back it up, never commit it).", key.display());
    }
    println!("Signed {} -> {}", exe.display(), PathBuf::from(&out).display());
    println!("Public key: {hex}");
    if hex != neo_core::update::RELEASE_KEY {
        println!("WARNING: this isn't the key built into the app (update::RELEASE_KEY); installed copies would refuse this release.");
    }
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_default();
    if mode == "sign-release" {
        return sign_release(args.collect());
    }
    let (mut profile, mut downloads, mut peers_wanted, mut exit_after, mut person) = (None, None, 1usize, 0usize, None);
    let mut rest = Vec::new();
    let (mut no_lan, mut no_private, mut discover_all) = (false, false, false);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--profile" => profile = args.next(),
            "--downloads" => downloads = args.next().map(PathBuf::from),
            "--peers" => peers_wanted = args.next().and_then(|v| v.parse().ok()).unwrap_or(1),
            "--exit-after" => exit_after = args.next().and_then(|v| v.parse().ok()).unwrap_or(0),
            "--person" => person = args.next(),
            "--no-lan" => no_lan = true,
            "--no-private" => no_private = true,
            "--discover-all" => discover_all = true,
            _ => rest.push(a),
        }
    }
    if !["receive", "send", "pair", "link"].contains(&mode.as_str()) {
        eprintln!("usage: neo-cli receive|send|pair|link [--profile P] [--downloads DIR] [--peers N] [--exit-after N] [--person NAME] [FILES... | CODE]");
        std::process::exit(2);
    }
    let mut config = Config::for_profile(profile.as_deref());
    if let Some(d) = downloads {
        config.downloads = d;
    }
    if let Some(p) = person {
        config.person_name = p;
    }
    config.auto_accept = mode == "receive";
    config.check_updates = false;
    config.discover_all = discover_all;
    config.lan_discovery = !no_lan;
    config.publish_lan = !no_private;
    let paths: Vec<PathBuf> = rest.iter().map(PathBuf::from).collect();
    let code = rest.first().cloned().unwrap_or_default();
    let node = neo_core::start(config)?;
    println!("{} ({}) is {} · device {}", node.person_name, node.name, node.account.code(), node.me.code());

    let mut asked = false;
    let mut sent = false;
    let mut totals: HashMap<u64, (String, u64, Instant)> = HashMap::new();
    let mut finished = 0usize;
    let mut failures = 0usize;
    let mut outgoing = 0usize;
    while let Ok(ev) = node.events.recv_blocking() {
        match ev {
            Event::Peers(peers) => {
                println!(
                    "peers: {}",
                    peers
                        .iter()
                        .map(|p| format!("{} [{}]", p.name, p.account.map(|a| a.code()).unwrap_or_default()))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                if mode == "send" && !sent && peers.len() >= peers_wanted {
                    sent = true;
                    outgoing = peers.len();
                    node.command(Command::Send { paths: paths.clone(), peers });
                } else if (mode == "pair" || mode == "link") && !asked {
                    let wanted = neo_core::identity::normalize_code(&code);
                    if peers.iter().any(|p| p.account.is_some_and(|a| a.matches_code(&wanted))) {
                        asked = true;
                        node.command(if mode == "pair" {
                            Command::AddByCode { code: code.clone() }
                        } else {
                            Command::LinkWith { code: code.clone() }
                        });
                    }
                }
            }
            Event::Contacts(list) if mode == "pair" && asked => {
                println!("contacts: {}", list.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(", "));
            }
            Event::Reachability(r) => {
                println!(
                    "reachability: dht={} mapped={:?} public={:?} ipv6={:?} lan={:?}",
                    r.dht, r.mapped, r.public, r.ipv6, r.lan
                );
                // Not on this network? Once the DHT is up, look them up online.
                if (mode == "pair" || mode == "link") && !asked && r.dht {
                    asked = true;
                    node.command(if mode == "pair" {
                        Command::AddByCode { code: code.clone() }
                    } else {
                        Command::LinkWith { code: code.clone() }
                    });
                }
            }
            Event::LinkPending { with, sas } => println!("linking with {with}: compare the number {sas}"),
            Event::Identity { account, person_name } => {
                println!("identity: {person_name} {}", account.code());
                if mode == "link" && asked {
                    std::process::exit(0);
                }
            }
            Event::Transfer(t) => {
                let dir = if t.direction == Direction::Outgoing { "to" } else { "from" };
                println!("#{} {} {}: {} file(s), {} bytes", t.id, dir, t.peer_name, t.files.len(), t.total);
                totals.insert(t.id, (t.peer_name, t.total, Instant::now()));
            }
            Event::Progress { id, done } => {
                if let Some((name, total, _)) = totals.get(&id) {
                    println!("#{id} {name}: {:.1}%", done as f64 * 100.0 / (*total).max(1) as f64);
                }
            }
            Event::State { id, state } => {
                if state == TransferState::Active {
                    if let Some(t) = totals.get_mut(&id) {
                        t.2 = Instant::now();
                    }
                }
                if !state.is_finished() {
                    println!("#{id} {state:?}");
                }
                if state.is_finished() {
                    finished += 1;
                    if let Some((name, total, started)) = totals.get(&id) {
                        let secs = started.elapsed().as_secs_f64();
                        println!("#{id} {name}: {state:?} in {secs:.2}s ({:.0} MB/s)", *total as f64 / secs / 1e6);
                    }
                    if !matches!(state, TransferState::Done { .. }) {
                        failures += 1;
                    }
                    if (mode == "send" && finished >= outgoing) || (exit_after > 0 && finished >= exit_after) {
                        std::process::exit(if failures == 0 { 0 } else { 1 });
                    }
                }
            }
            Event::Notice(n) => {
                println!("notice: {n}");
                if (mode == "pair" || mode == "link") && asked {
                    std::process::exit(1);
                }
            }
            Event::Info(n) => {
                println!("info: {n}");
                if mode == "pair" && n.ends_with("is now a contact") {
                    std::process::exit(0);
                }
            }
            _ => {}
        }
    }
    Ok(())
}
