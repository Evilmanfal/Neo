//! Pairing (becoming contacts) and linking (adding one of your own devices).
//!
//! Both run over the same pinned QUIC/TLS connections as transfers.
//!
//! - **Pairing** needs consent on both sides: the requester asked, the other
//!   person accepts on their screen. Each side then stores the other's account
//!   with the name they gave. People who already know each other are
//!   re-confirmed silently.
//! - **Linking** copies your account key to a new device. The new device asks
//!   the existing one, both screens show the same 6-digit number, and the
//!   owner accepts on the existing device. The number makes sure the device
//!   being approved is the one in front of you.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};

use crate::identity::{self, hex, unhex, Account, Attest, DeviceId};
use crate::proto::{read_frame, write_frame, Msg};
use crate::{contacts, Event, Peer, Shared};

/// How long a request waits for the other person.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// The number both screens show while linking.
pub fn sas(existing: DeviceId, joining: DeviceId) -> String {
    let mut h = blake3::Hasher::new();
    h.update(b"neo-link-v1");
    h.update(&existing.0);
    h.update(&joining.0);
    let n = u32::from_le_bytes(h.finalize().as_bytes()[..4].try_into().unwrap()) % 1_000_000;
    format!("{:03} {:03}", n / 1000, n % 1000)
}

/// Shows a request in the UI and waits for the answer (or the other side
/// giving up, or a timeout).
async fn ask(shared: &Shared, conn: &quinn::Connection, event: impl FnOnce(u64) -> Event) -> bool {
    let id = shared.next_id();
    let (tx, rx) = tokio::sync::oneshot::channel();
    shared.pending.lock().unwrap().insert(id, tx);
    shared.emit(event(id));
    let answer = tokio::select! {
        r = rx => r.unwrap_or(false),
        _ = conn.closed() => false,
        _ = tokio::time::sleep(REQUEST_TIMEOUT) => false,
    };
    shared.pending.lock().unwrap().remove(&id);
    shared.emit(Event::RequestClosed { id });
    answer
}

async fn finish(conn: &quinn::Connection, mut tx: quinn::SendStream) {
    let _ = tx.finish();
    let _ = tokio::time::timeout(Duration::from_secs(3), conn.closed()).await;
}

// ------------------------------------------------------------------ pairing

pub async fn request_pair(shared: Arc<Shared>, peer: Peer) {
    if peer.account == Some(shared.my_account()) {
        return shared.emit(Event::Notice(format!("{} is one of your own devices", peer.name)));
    }
    let result: Result<()> = async {
        let conn = shared.connect_peer(&peer).await?;
        let (mut tx, mut rx) = conn.open_bi().await?;
        let me = shared.config().person_name;
        write_frame(&mut tx, &Msg::Pair { name: me, attest: shared.attest() }).await?;
        shared.emit(Event::Info(format!("Waiting for {} to accept…", peer.name)));
        let reply = tokio::time::timeout(REQUEST_TIMEOUT, read_frame::<Msg>(&mut rx))
            .await
            .map_err(|_| anyhow!("no answer"))??;
        conn.close(0u32.into(), b"done");
        match reply {
            Msg::PairReply { accept: true, name, attest: Some(attest) } => {
                let account = attest.verify(peer.id).ok_or_else(|| anyhow!("their identity didn't check out"))?;
                shared.update_contacts(|c| {
                    contacts::upsert(c, account, &name, Some(&attest.account));
                });
                shared.emit(Event::Info(format!("{name} is now a contact")));
            }
            Msg::PairReply { .. } => shared.emit(Event::Notice(format!("{} declined", peer.name))),
            _ => bail!("unexpected reply"),
        }
        Ok(())
    }
    .await;
    if let Err(e) = result {
        shared.emit(Event::Notice(format!("Couldn't add {}: {e:#}", peer.name)));
    }
}

pub async fn handle_pair(
    shared: Arc<Shared>,
    conn: quinn::Connection,
    device: DeviceId,
    mut tx: quinn::SendStream,
    name: String,
    attest: Attest,
) {
    let Some(account) = attest.verify(device) else {
        return conn.close(0u32.into(), b"bad identity");
    };
    let name: String = name.trim().chars().filter(|c| !c.is_control()).take(64).collect();
    let mine = shared.my_account();
    let known = account == mine || shared.contacts.lock().unwrap().iter().any(|c| c.account == account);
    let device_name = shared.peers.lock().unwrap().iter().find(|p| p.id == device).map(|p| p.name.clone());
    let accept = known
        || ask(&shared, &conn, |id| Event::PairRequest {
            id,
            name: name.clone(),
            account,
            device_name: device_name.unwrap_or_else(|| device.code()),
        })
        .await;
    let reply = Msg::PairReply { accept, name: shared.config().person_name, attest: accept.then(|| shared.attest()) };
    if write_frame(&mut tx, &reply).await.is_err() {
        return;
    }
    if accept && account != mine {
        let was_new = !known;
        shared.update_contacts(|c| {
            contacts::upsert(c, account, &name, Some(&attest.account));
        });
        if was_new {
            shared.emit(Event::Info(format!("{name} is now a contact")));
        }
    }
    finish(&conn, tx).await;
}

// --------------------------------------------------------------- links

/// Sends a Gofile link to a contact's device.
pub async fn send_link(shared: Arc<Shared>, peer: Peer, title: String, url: String) {
    let result: Result<()> = async {
        let conn = shared.connect_peer(&peer).await?;
        let (mut tx, mut rx) = conn.open_bi().await?;
        write_frame(&mut tx, &Msg::ShareLink { title: title.clone(), url, attest: shared.attest() }).await?;
        let _ = tx.finish();
        // Wait for their side to read it and close.
        let _ = tokio::time::timeout(Duration::from_secs(10), rx.read_to_end(64)).await;
        conn.close(0u32.into(), b"done");
        Ok(())
    }
    .await;
    match result {
        Ok(()) => shared.emit(Event::Info(format!("Sent the link for {title} to {}", peer.name))),
        Err(e) => shared.emit(Event::Notice(format!("Couldn't send the link to {}: {e:#}", peer.name))),
    }
}

/// A link from someone: shown only if they're a contact or one of your own
/// devices, and only Gofile links (no arbitrary URLs).
pub async fn handle_shared_link(
    shared: Arc<Shared>,
    conn: quinn::Connection,
    device: DeviceId,
    tx: quinn::SendStream,
    title: String,
    url: String,
    attest: Attest,
) {
    let Some(account) = attest.verify(device) else {
        return conn.close(0u32.into(), b"bad identity");
    };
    let from = if account == shared.my_account() {
        Some("your other device".to_string())
    } else {
        shared.contacts.lock().unwrap().iter().find(|c| c.account == account).map(|c| c.name.clone())
    };
    let ok_url = url.starts_with("https://gofile.io/") && url.len() < 200 && !url.chars().any(char::is_whitespace);
    if let (Some(from), true) = (from, ok_url) {
        let title: String = title.chars().filter(|c| !c.is_control()).take(120).collect();
        shared.emit(Event::LinkShared { from, title, url });
    }
    finish(&conn, tx).await;
}

// ------------------------------------------------------------------ linking

/// Runs on the new device: ask `peer` (one of your devices) for the identity.
pub async fn request_link(shared: Arc<Shared>, peer: Peer) {
    if peer.account == Some(shared.my_account()) {
        return shared.emit(Event::Info(format!("{} is already linked to this device", peer.name)));
    }
    let result: Result<()> = async {
        let expected = peer.account.ok_or_else(|| anyhow!("that device didn't say who it belongs to"))?;
        let conn = shared.connect_peer(&peer).await?;
        let (mut tx, mut rx) = conn.open_bi().await?;
        shared.emit(Event::LinkPending { with: peer.name.clone(), sas: sas(peer.id, shared.identity.id) });
        write_frame(&mut tx, &Msg::LinkRequest { device_name: shared.config().name }).await?;
        let reply = tokio::time::timeout(REQUEST_TIMEOUT, read_frame::<Msg>(&mut rx))
            .await
            .map_err(|_| anyhow!("no answer"))??;
        conn.close(0u32.into(), b"done");
        let Msg::LinkReply { accept, account_pkcs8, person_name, contacts: theirs } = reply else {
            bail!("unexpected reply");
        };
        if !accept {
            shared.emit(Event::Notice("Linking was declined on the other device".into()));
            return Ok(());
        }
        let key = account_pkcs8.and_then(|k| unhex(&k)).ok_or_else(|| anyhow!("no identity received"))?;
        let account = Account::from_pkcs8(key)?;
        if account.id() != expected {
            bail!("the other device sent a different identity than it announced");
        }
        identity::save_account(&shared.config().data_dir, &account)?;
        *shared.account.write().unwrap() = account;
        if !person_name.trim().is_empty() {
            shared.config.write().unwrap().person_name = person_name.trim().chars().take(64).collect();
            shared.save_settings();
        }
        shared.update_contacts(|c| contacts::merge(c, theirs));
        shared.emit_identity();
        shared.emit(Event::Info(format!("Linked with {}. This device now acts as you.", peer.name)));
        Ok(())
    }
    .await;
    shared.emit(Event::LinkFinished);
    if let Err(e) = result {
        shared.emit(Event::Notice(format!("Couldn't link with {}: {e:#}", peer.name)));
    }
}

/// Runs on the existing device: the owner approves (or not) the new device.
pub async fn handle_link(
    shared: Arc<Shared>,
    conn: quinn::Connection,
    device: DeviceId,
    mut tx: quinn::SendStream,
    device_name: String,
) {
    let device_name: String = device_name.trim().chars().filter(|c| !c.is_control()).take(64).collect();
    let number = sas(shared.identity.id, device);
    let accept = ask(&shared, &conn, |id| Event::LinkRequest { id, device_name: device_name.clone(), sas: number }).await;
    let reply = if accept {
        let key = hex(shared.account.read().unwrap().pkcs8());
        Msg::LinkReply {
            accept: true,
            account_pkcs8: Some(key),
            person_name: shared.config().person_name,
            contacts: shared.contacts.lock().unwrap().clone(),
        }
    } else {
        Msg::LinkReply { accept: false, account_pkcs8: None, person_name: String::new(), contacts: Vec::new() }
    };
    if write_frame(&mut tx, &reply).await.is_ok() && accept {
        shared.emit(Event::Info(format!("{device_name} is now linked to you")));
    }
    finish(&conn, tx).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_numbers_match_on_both_sides_only() {
        let (a, b, c) = (DeviceId([1; 32]), DeviceId([2; 32]), DeviceId([3; 32]));
        assert_eq!(sas(a, b), sas(a, b));
        assert_ne!(sas(a, b), sas(a, c));
        assert_eq!(sas(a, b).len(), 7);
    }
}
