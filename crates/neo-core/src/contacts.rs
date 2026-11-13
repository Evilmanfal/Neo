//! Contacts are people (accounts), stored in `<data_dir>/contacts.json`.
//! A contact exists only after both sides agreed (pairing), and only you
//! decide whether files from them are accepted without asking.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::identity::AccountId;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Contact {
    pub account: AccountId,
    pub name: String,
    /// Accept files from this person without asking.
    #[serde(default)]
    pub auto_accept: bool,
    /// Unix seconds.
    #[serde(default)]
    pub added: u64,
    /// Their account public key (hex), learned when pairing or seeing their
    /// device nearby. Needed to find them over the internet.
    #[serde(default)]
    pub public: Option<String>,
}

pub fn load(dir: &Path) -> Vec<Contact> {
    std::fs::read(dir.join("contacts.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub fn save(dir: &Path, contacts: &[Contact]) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join("contacts.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(contacts)?)?;
    std::fs::rename(tmp, dir.join("contacts.json"))
}

/// Adds or refreshes a contact; returns true if the list changed.
pub fn upsert(list: &mut Vec<Contact>, account: AccountId, name: &str, public: Option<&str>) -> bool {
    let name: String = name.trim().chars().take(64).collect();
    let public = public.map(str::to_string);
    match list.iter_mut().find(|c| c.account == account) {
        Some(c) => {
            let mut changed = false;
            if !name.is_empty() && c.name != name {
                c.name = name;
                changed = true;
            }
            if public.is_some() && c.public != public {
                c.public = public;
                changed = true;
            }
            changed
        }
        None => {
            let added = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let name = if name.is_empty() { account.code() } else { name };
            list.push(Contact { account, name, auto_accept: false, added, public });
            list.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
            true
        }
    }
}

/// Adds contacts we don't have yet (used when linking devices / restoring).
pub fn merge(list: &mut Vec<Contact>, incoming: Vec<Contact>) {
    for c in incoming {
        if !list.iter().any(|x| x.account == c.account) {
            list.push(c);
        }
    }
    list.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
}
