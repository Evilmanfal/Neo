//! Presence records in the public BitTorrent DHT (BEP 44 mutable items), so
//! contacts can find each other across the internet with no server.
//!
//! - **Presence**: stored under the person's account key (salt
//!   `neo/presence/v1`), signed by that key. It lists each of their devices
//!   with its current addresses. The value is encrypted with a key derived
//!   from the account *public* key, which only people who paired with you
//!   (or saw your device on a LAN) know. DHT nodes see an opaque blob.
//! - **Code record**: maps a 12-character code to the full account key, so
//!   "Add by code" works over the internet too. It is stored under a key
//!   derived from the code itself. Anyone with the code could overwrite it,
//!   but not forge it: the answer must hash to the code (60 bits).

use std::net::SocketAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use mainline::async_dht::AsyncDht;
use mainline::{MutableItem, SigningKey};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};

use super::nat::NatInfo;
use crate::identity::{normalize_code, Account, AccountId, DeviceId};

const PRESENCE_SALT: &[u8] = b"neo/presence/v1";
const KNOCK_SALT: &[u8] = b"neo/knock/v1";
/// BEP 44 caps values at 1000 bytes.
const MAX_VALUE: usize = 1000;
/// Entries older than this are dropped when someone republishes.
const KEEP: Duration = Duration::from_secs(3 * 3600);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeviceEntry {
    pub id: DeviceId,
    #[serde(rename = "n")]
    pub name: String,
    #[serde(rename = "a")]
    pub addrs: Vec<SocketAddr>,
    /// Unix seconds of the last update.
    pub t: u64,
    /// NAT type, so contacts can pick a punching strategy.
    #[serde(rename = "x", default, skip_serializing_if = "Option::is_none")]
    pub nat: Option<NatInfo>,
}

#[derive(Default, Serialize, Deserialize)]
struct Record {
    d: Vec<DeviceEntry>,
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn cipher(account_public: &[u8; 32]) -> XChaCha20Poly1305 {
    XChaCha20Poly1305::new((&blake3::derive_key("neo presence v1", account_public)).into())
}

fn seal(account_public: &[u8; 32], plain: &[u8]) -> Result<Vec<u8>> {
    let mut nonce = [0u8; 24];
    SystemRandom::new().fill(&mut nonce).map_err(|_| anyhow!("no randomness"))?;
    let mut out = nonce.to_vec();
    out.extend(cipher(account_public).encrypt(XNonce::from_slice(&nonce), plain).map_err(|_| anyhow!("encrypt"))?);
    Ok(out)
}

fn open(account_public: &[u8; 32], data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < 24 {
        return None;
    }
    cipher(account_public).decrypt(XNonce::from_slice(&data[..24]), &data[24..]).ok()
}

/// "Someone is trying to reach you, here's where": lets a first contact get
/// through routers that drop unsolicited packets. The recipient checks for
/// knocks and sends packets toward these addresses, which opens its side.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Knock {
    #[serde(rename = "a")]
    pub addrs: Vec<SocketAddr>,
    pub t: u64,
    /// The knocker's NAT type (for the punching plan).
    #[serde(rename = "x", default)]
    pub nat: Option<NatInfo>,
    /// Punch token (hex, 8 bytes): marks both sides' probes for this attempt.
    #[serde(rename = "k", default)]
    pub token: Option<String>,
}

/// Anyone who knows your account key can derive this signer (that's the
/// point: strangers with your code can knock).
fn knock_signer(target_public: &[u8; 32]) -> SigningKey {
    SigningKey::from_bytes(&blake3::derive_key("neo knock key v1", target_public))
}

fn code_signer(code: &str) -> SigningKey {
    SigningKey::from_bytes(&blake3::derive_key("neo code key v1", normalize_code(code).as_bytes()))
}

pub struct Presence {
    dht: AsyncDht,
}

impl Presence {
    /// Joins the public DHT, or with `bootstrap` a local test network
    /// (which runs on localhost).
    pub fn start(bootstrap: Option<&[String]>) -> Result<Self> {
        let mut builder = mainline::Dht::builder();
        if let Some(nodes) = bootstrap {
            builder.bootstrap(nodes).bind_address(std::net::Ipv4Addr::LOCALHOST);
        }
        Ok(Presence { dht: builder.build()?.as_async() })
    }

    pub async fn ready(&self) -> bool {
        self.dht.bootstrapped().await
    }

    async fn read(&self, public: &[u8; 32]) -> (Option<i64>, Record) {
        match self.dht.get_mutable_most_recent(public, Some(PRESENCE_SALT)).await {
            Some(item) => {
                let record = open(public, item.value()).and_then(|p| serde_json::from_slice(&p).ok()).unwrap_or_default();
                (Some(item.seq()), record)
            }
            None => (None, Record::default()),
        }
    }

    /// Adds/refreshes this device in its owner's record (linked devices
    /// share one record and merge into it).
    pub async fn publish(&self, account: &Account, me: DeviceEntry) -> Result<()> {
        let public = account.public();
        let seed = account.seed().ok_or_else(|| anyhow!("account key can't sign DHT records"))?;
        for _attempt in 0..3 {
            let (seq, mut record) = self.read(&public).await;
            let cutoff = now().saturating_sub(KEEP.as_secs());
            record.d.retain(|d| d.id != me.id && d.t >= cutoff);
            record.d.insert(0, me.clone());
            let value = loop {
                let sealed = seal(&public, &serde_json::to_vec(&record)?)?;
                if sealed.len() <= MAX_VALUE || record.d.len() <= 1 {
                    break sealed;
                }
                record.d.pop(); // drop the oldest device
            };
            let next = seq.map(|s| s + 1).unwrap_or(0).max(now() as i64);
            let item = MutableItem::new(SigningKey::from_bytes(&seed), &value, next, Some(PRESENCE_SALT));
            match self.dht.put_mutable(item, seq).await {
                Ok(_) => return Ok(()),
                Err(mainline::errors::PutMutableError::Concurrency(_)) => continue, // someone else wrote; merge again
                Err(e) => return Err(anyhow!("{e}")),
            }
        }
        Err(anyhow!("couldn't publish presence (busy)"))
    }

    /// Devices of the account with this public key, updated within `fresh`.
    pub async fn lookup(&self, public: &[u8; 32], fresh: Duration) -> Vec<DeviceEntry> {
        let (_, record) = self.read(public).await;
        let cutoff = now().saturating_sub(fresh.as_secs());
        record.d.into_iter().filter(|d| d.t >= cutoff).collect()
    }

    /// Lets people resolve your code to your account key.
    pub async fn publish_code(&self, account: &Account) -> Result<()> {
        let item = MutableItem::new(code_signer(&account.id().code()), &account.public(), now() as i64, None);
        self.dht.put_mutable(item, None).await.map_err(|e| anyhow!("{e}"))?;
        Ok(())
    }

    /// Asks the owner of `target_public` to open a path toward `addrs`.
    pub async fn knock(
        &self,
        target_public: &[u8; 32],
        addrs: Vec<SocketAddr>,
        nat: Option<NatInfo>,
        token: Option<[u8; 8]>,
    ) -> Result<()> {
        let token = token.map(|t| crate::identity::hex(&t));
        let value = seal(target_public, &serde_json::to_vec(&Knock { addrs, t: now(), nat, token })?)?;
        let item = MutableItem::new(knock_signer(target_public), &value, now() as i64, Some(KNOCK_SALT));
        self.dht.put_mutable(item, None).await.map_err(|e| anyhow!("{e}"))?;
        Ok(())
    }

    /// The latest knock for us, if it's recent.
    pub async fn knocks(&self, my_public: &[u8; 32], fresh: Duration) -> Option<Knock> {
        let signer = knock_signer(my_public);
        let item = self.dht.get_mutable_most_recent(signer.verifying_key().as_bytes(), Some(KNOCK_SALT)).await?;
        let knock: Knock = serde_json::from_slice(&open(my_public, item.value())?).ok()?;
        (knock.t + fresh.as_secs() >= now()).then_some(knock)
    }

    /// The account key behind a code, verified against the code.
    pub async fn resolve_code(&self, code: &str) -> Option<[u8; 32]> {
        let signer = code_signer(code);
        let item = self.dht.get_mutable_most_recent(signer.verifying_key().as_bytes(), None).await?;
        let public: [u8; 32] = item.value().try_into().ok()?;
        AccountId::of_public(&public).matches_code(code).then_some(public)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_are_private_to_people_with_the_key() {
        let a = Account::generate().unwrap();
        let b = Account::generate().unwrap();
        let sealed = seal(&a.public(), b"hello").unwrap();
        assert_eq!(open(&a.public(), &sealed).as_deref(), Some(&b"hello"[..]));
        assert_eq!(open(&b.public(), &sealed), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn publish_lookup_and_codes_on_a_test_dht() {
        let testnet = mainline::Testnet::builder(10).build().unwrap();
        let alice = Presence::start(Some(&testnet.bootstrap)).unwrap();
        let bob = Presence::start(Some(&testnet.bootstrap)).unwrap();
        assert!(alice.ready().await && bob.ready().await);
        let account = Account::generate().unwrap();
        let laptop = DeviceEntry { id: DeviceId([1; 32]), name: "Laptop".into(), addrs: vec!["203.0.113.7:4000".parse().unwrap()], t: now(), nat: None };
        let desktop = DeviceEntry { id: DeviceId([2; 32]), name: "Desktop".into(), addrs: vec!["[2001:db8::1]:4000".parse().unwrap()], t: now(), nat: None };
        alice.publish(&account, laptop.clone()).await.unwrap();
        alice.publish(&account, desktop.clone()).await.unwrap();
        let found = bob.lookup(&account.public(), Duration::from_secs(600)).await;
        assert_eq!(found.len(), 2, "both linked devices are listed: {found:?}");
        assert!(found.contains(&laptop) && found.contains(&desktop));

        let knock_from: Vec<SocketAddr> = vec!["198.51.100.9:5000".parse().unwrap()];
        bob.knock(&account.public(), knock_from.clone(), None, Some([7; 8])).await.unwrap();
        assert_eq!(alice.knocks(&account.public(), Duration::from_secs(60)).await.map(|k| k.addrs), Some(knock_from));

        alice.publish_code(&account).await.unwrap();
        assert_eq!(bob.resolve_code(&account.id().code()).await, Some(account.public()));
        assert_eq!(bob.resolve_code("0000-0000-0000").await, None);
    }
}
