//! Wire protocol (ALPN `neo/5`; `neo/4` added swarm fan-out, see `swarm`;
//! `neo/5` added pausing: `Pause` frames both ways on the control stream).
//!
//! One QUIC connection per (sender, recipient). On it:
//! - a bidirectional *control* stream carrying length-prefixed JSON frames:
//!   sender `Offer` → recipient `Decision` → … → recipient `Complete`;
//! - one unidirectional stream per file still needed: `u32 LE index`,
//!   `u64 LE start offset`, then the file from that offset in blocks of
//!   `BLOCK` bytes (the last one shorter), each followed by its 32-byte
//!   BLAKE3 hash. Every block is verified before it counts, so an interrupted
//!   file resumes from its last verified block.

use anyhow::{bail, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::contacts::Contact;
use crate::identity::Attest;

pub const MAX_FRAME: usize = 16 << 20;
pub const BLOCK: u64 = 1 << 20;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FileMeta {
    /// Relative path using `/` separators (e.g. `photos/a.jpg`).
    pub name: String,
    pub size: u64,
    /// Sender's fingerprint of the file (path, size, mtime) for resuming.
    #[serde(default)]
    pub key: String,
}

/// The first frame on a connection's control stream says what it's for:
/// `Offer` (send files), `Pair` (become contacts) or `LinkRequest` (join the
/// receiver's account as one more of its owner's devices).
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Msg {
    Offer {
        sender_name: String,
        files: Vec<FileMeta>,
        total: u64,
        /// Which account the sending device belongs to.
        #[serde(default)]
        attest: Option<Attest>,
        /// Part of a multi-recipient swarm: after the Decision, expect a
        /// `Swarm` message and block streams instead of per-file streams.
        #[serde(default)]
        swarm: bool,
        /// The sender has this transfer paused.
        #[serde(default)]
        paused: bool,
    },
    Decision {
        accept: bool,
        /// Per file: where to start (a multiple of `BLOCK`).
        #[serde(default)]
        offsets: Vec<u64>,
        /// Indices of files the recipient already has in full.
        #[serde(default)]
        have: Vec<u32>,
        /// The recipient has this transfer paused.
        #[serde(default)]
        paused: bool,
    },
    /// Either side, any time after the Decision: paused or resumed here.
    Pause { paused: bool },
    Complete { error: Option<String> },
    /// Sender -> recipient: who else gets this batch, and the batch secret.
    Swarm { batch: String, secret: String, roster: Vec<crate::swarm::RosterEntry> },
    /// Recipient -> sender: peers I'm linked with (I forward to them).
    SwarmPeers { ids: Vec<crate::DeviceId> },
    /// Recipient -> sender: bytes verified and written so far.
    SwarmHave { done: u64 },
    /// Recipient -> sender: please send these blocks directly.
    Need { file: u32, blocks: Vec<u64> },
    /// Sender -> recipient: every block has been handed out at least once.
    AllAssigned,
    /// "Here's a Gofile link" from a contact (or one of your devices).
    ShareLink { title: String, url: String, attest: Attest },
    /// Recipient -> recipient: link for one batch (proof from its secret).
    SwarmHello { batch: String, proof: String },
    SwarmWelcome { ok: bool },
    /// "Let's be contacts." `name` is the person's name.
    Pair { name: String, attest: Attest },
    PairReply { accept: bool, name: String, attest: Option<Attest> },
    /// A new device asks to join the receiving device's account.
    LinkRequest { device_name: String },
    /// On acceptance carries the account key (over the pinned, encrypted
    /// connection), the owner's name and contacts.
    LinkReply {
        accept: bool,
        #[serde(default)]
        account_pkcs8: Option<String>,
        #[serde(default)]
        person_name: String,
        #[serde(default)]
        contacts: Vec<Contact>,
    },
}

pub async fn write_frame<T: Serialize>(s: &mut quinn::SendStream, msg: &T) -> Result<()> {
    let body = serde_json::to_vec(msg)?;
    let mut frame = Vec::with_capacity(body.len() + 4);
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    frame.extend_from_slice(&body);
    s.write_all(&frame).await?;
    Ok(())
}

pub async fn read_frame<T: DeserializeOwned>(r: &mut quinn::RecvStream) -> Result<T> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        bail!("control frame too large ({len} bytes)");
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    Ok(serde_json::from_slice(&body)?)
}

/// Turns an untrusted relative path from a peer into something safe to create
/// under the downloads folder: no `..`, no roots/drives, no reserved names.
pub fn sanitize_rel_path(name: &str) -> std::path::PathBuf {
    const RESERVED: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let mut out = std::path::PathBuf::new();
    for part in name.split(['/', '\\']) {
        let cleaned: String = part
            .chars()
            .filter(|c| !c.is_control() && !matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
            .collect();
        let cleaned = cleaned.trim().trim_end_matches(['.', ' ']).to_string();
        if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
            continue;
        }
        let stem = cleaned.split('.').next().unwrap_or("").to_ascii_uppercase();
        if RESERVED.contains(&stem.as_str()) {
            out.push(format!("_{cleaned}"));
        } else {
            out.push(cleaned);
        }
    }
    if out.as_os_str().is_empty() {
        out.push("file");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::sanitize_rel_path;
    use std::path::PathBuf;

    #[test]
    fn sanitizes_hostile_paths() {
        assert_eq!(sanitize_rel_path("../../etc/passwd"), PathBuf::from("etc").join("passwd"));
        assert_eq!(sanitize_rel_path("C:\\Windows\\x.dll"), PathBuf::from("C").join("Windows").join("x.dll"));
        assert_eq!(sanitize_rel_path("con.txt"), PathBuf::from("_con.txt"));
        assert_eq!(sanitize_rel_path(".."), PathBuf::from("file"));
        assert_eq!(sanitize_rel_path("a/b.txt"), PathBuf::from("a").join("b.txt"));
    }
}
