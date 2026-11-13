//! Passphrase-encrypted identity backup: the account key, your name and your
//! contacts, so a lost or reinstalled device can be restored as "you".
//! Argon2id (64 MiB, 3 passes) derives the key; XChaCha20-Poly1305 encrypts.
//! Nothing here ever leaves the device unless you move the file yourself.

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};

use crate::contacts::Contact;
use crate::identity::{hex, unhex, Account};

const MEMORY_KIB: u32 = 64 * 1024;
const PASSES: u32 = 3;

#[derive(Serialize, Deserialize)]
struct File {
    neo_backup: u32,
    kdf: String,
    memory_kib: u32,
    passes: u32,
    salt: String,
    nonce: String,
    data: String,
}

#[derive(Serialize, Deserialize)]
struct Payload {
    account_pkcs8: String,
    person_name: String,
    contacts: Vec<Contact>,
}

pub struct Restored {
    pub account: Account,
    pub person_name: String,
    pub contacts: Vec<Contact>,
}

fn derive(passphrase: &str, salt: &[u8], memory_kib: u32, passes: u32) -> Result<[u8; 32]> {
    let params = Params::new(memory_kib, passes, 1, Some(32)).map_err(|e| anyhow!("{e}"))?;
    let mut key = [0u8; 32];
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(passphrase.as_bytes(), salt, &mut key)
        .map_err(|e| anyhow!("{e}"))?;
    Ok(key)
}

pub fn export(path: &Path, passphrase: &str, account: &Account, person_name: &str, contacts: &[Contact]) -> Result<()> {
    if passphrase.chars().count() < 8 {
        bail!("Use a passphrase of at least 8 characters");
    }
    let rng = SystemRandom::new();
    let mut salt = [0u8; 16];
    let mut nonce = [0u8; 24];
    rng.fill(&mut salt).map_err(|_| anyhow!("no randomness"))?;
    rng.fill(&mut nonce).map_err(|_| anyhow!("no randomness"))?;
    let key = derive(passphrase, &salt, MEMORY_KIB, PASSES)?;
    let payload = serde_json::to_vec(&Payload {
        account_pkcs8: hex(account.pkcs8()),
        person_name: person_name.to_string(),
        contacts: contacts.to_vec(),
    })?;
    let data = XChaCha20Poly1305::new((&key).into())
        .encrypt(XNonce::from_slice(&nonce), payload.as_slice())
        .map_err(|_| anyhow!("encryption failed"))?;
    let file = File {
        neo_backup: 1,
        kdf: "argon2id".into(),
        memory_kib: MEMORY_KIB,
        passes: PASSES,
        salt: hex(&salt),
        nonce: hex(&nonce),
        data: hex(&data),
    };
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&file)?).with_context(|| format!("writing {}", path.display()))?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn import(path: &Path, passphrase: &str) -> Result<Restored> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let file: File = serde_json::from_slice(&bytes).map_err(|_| anyhow!("That isn't a Neo backup file"))?;
    if file.neo_backup != 1 || file.kdf != "argon2id" {
        bail!("Unsupported backup version");
    }
    // Refuse absurd parameters from a tampered file instead of exhausting memory.
    if file.memory_kib > 1024 * 1024 || file.passes > 20 {
        bail!("Backup file parameters look wrong");
    }
    let salt = unhex(&file.salt).ok_or_else(|| anyhow!("Damaged backup file"))?;
    let nonce = unhex(&file.nonce).filter(|n| n.len() == 24).ok_or_else(|| anyhow!("Damaged backup file"))?;
    let data = unhex(&file.data).ok_or_else(|| anyhow!("Damaged backup file"))?;
    let key = derive(passphrase, &salt, file.memory_kib, file.passes)?;
    let plain = XChaCha20Poly1305::new((&key).into())
        .decrypt(XNonce::from_slice(&nonce), data.as_slice())
        .map_err(|_| anyhow!("Wrong passphrase, or the file is damaged"))?;
    let payload: Payload = serde_json::from_slice(&plain)?;
    let account = Account::from_pkcs8(unhex(&payload.account_pkcs8).ok_or_else(|| anyhow!("Damaged backup file"))?)?;
    Ok(Restored { account, person_name: payload.person_name, contacts: payload.contacts })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_wrong_passphrase() {
        let dir = std::env::temp_dir().join(format!("neo-backup-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("me.neobackup");
        let account = Account::generate().unwrap();
        let contacts = vec![Contact { account: Account::generate().unwrap().id(), name: "Sam".into(), auto_accept: true, added: 1, public: None }];
        export(&path, "correct horse battery", &account, "Jay", &contacts).unwrap();
        let restored = import(&path, "correct horse battery").unwrap();
        assert_eq!(restored.account.id(), account.id());
        assert_eq!(restored.person_name, "Jay");
        assert_eq!(restored.contacts, contacts);
        assert!(import(&path, "wrong passphrase").is_err());
        assert!(export(&path, "short", &account, "Jay", &contacts).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
