//! Receiver-side resume state, one small JSON file per incoming batch in
//! `<data_dir>/partials/<fingerprint>.json`. It remembers, for each file in
//! the batch, where its `.neopart` file is, how many bytes are verified and
//! written, and which files are already complete. An interrupted batch then
//! continues from there, even after a restart on either side. The record is
//! deleted once the batch completes.
//!
//! The fingerprint covers the sender's DeviceId and each file's name, size
//! and sender-side fingerprint (path, size, mtime), so a changed file never
//! resumes onto old data and one sender can't resume into another's files.
//!
//! Data is not fsynced while receiving (that stalls the pipeline). Records
//! remember when they were written; if the machine has rebooted since, data
//! the record calls verified may never have reached the disk, so unfinished
//! files restart from zero. App crashes and dropped connections lose nothing.
//! The record is only written while something changed, at most twice a second.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::identity::DeviceId;
use crate::proto::FileMeta;

const KEEP_FOR: Duration = Duration::from_secs(7 * 24 * 3600);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileState {
    pub part: PathBuf,
    pub final_path: PathBuf,
    pub size: u64,
    /// Bytes at the start of `part` that are hash-verified and written.
    pub verified: u64,
    pub complete: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Batch {
    pub files: Vec<Option<FileState>>,
    /// Unix seconds when this record was last saved.
    #[serde(default)]
    pub saved_at: u64,
}

pub type Fingerprint = [u8; 32];

pub fn fingerprint(peer: DeviceId, files: &[FileMeta]) -> Fingerprint {
    let mut h = blake3::Hasher::new();
    h.update(&peer.0);
    for f in files {
        h.update(f.key.as_bytes());
        h.update(&(f.name.len() as u64).to_le_bytes());
        h.update(f.name.as_bytes());
        h.update(&f.size.to_le_bytes());
    }
    *h.finalize().as_bytes()
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Unix seconds when the machine last booted.
fn boot_time() -> u64 {
    #[cfg(windows)]
    {
        let uptime_ms = unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount64() };
        now_secs().saturating_sub(uptime_ms / 1000)
    }
    #[cfg(not(windows))]
    {
        0
    }
}

fn path(dir: &Path, fp: &Fingerprint) -> PathBuf {
    let hex: String = fp[..16].iter().map(|b| format!("{b:02x}")).collect();
    dir.join(format!("{hex}.json"))
}

/// Loads a batch, keeping only entries whose files still look right.
pub fn load(dir: &Path, fp: &Fingerprint, files: &[FileMeta]) -> Option<Batch> {
    let mut batch: Batch = serde_json::from_slice(&std::fs::read(path(dir, fp)).ok()?).ok()?;
    if batch.files.len() != files.len() {
        return None;
    }
    let rebooted = boot_time() + 5 >= batch.saved_at;
    for (slot, meta) in batch.files.iter_mut().zip(files) {
        let ok = match slot {
            None => true,
            Some(f) if f.size != meta.size => false,
            Some(f) if f.complete => std::fs::metadata(&f.final_path).map(|m| m.len() == f.size).unwrap_or(false),
            Some(f) => {
                if rebooted {
                    f.verified = 0; // unsynced data may be gone
                }
                std::fs::metadata(&f.part).map(|m| m.len() >= f.verified && f.verified <= f.size).unwrap_or(false)
            }
        };
        if !ok {
            *slot = None;
        }
    }
    Some(batch)
}

pub fn save(dir: &Path, fp: &Fingerprint, batch: &Batch) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let target = path(dir, fp);
    let tmp = target.with_extension("tmp");
    let batch = Batch { saved_at: now_secs(), files: batch.files.clone() };
    std::fs::write(&tmp, serde_json::to_vec(&batch)?)?;
    std::fs::rename(tmp, target)
}

pub fn remove(dir: &Path, fp: &Fingerprint) {
    let _ = std::fs::remove_file(path(dir, fp));
}

fn discard_file(record: &Path) {
    if let Some(batch) = std::fs::read(record).ok().and_then(|b| serde_json::from_slice::<Batch>(&b).ok()) {
        for f in batch.files.iter().flatten() {
            if !f.complete {
                let _ = std::fs::remove_file(&f.part);
            }
        }
    }
    let _ = std::fs::remove_file(record);
}

/// Deletes a batch's unfinished partial files and its record (used on cancel).
pub fn discard(dir: &Path, fp: &Fingerprint) {
    discard_file(&path(dir, fp));
}

/// Drops records untouched for a week, with their partial files.
pub fn prune(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let now = SystemTime::now();
    for e in entries.flatten() {
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .map(|t| now.duration_since(t).unwrap_or_default() > KEEP_FOR)
            .unwrap_or(false);
        if old {
            discard_file(&e.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(size: u64) -> FileMeta {
        FileMeta { name: "a.bin".into(), size, key: "k".into() }
    }

    #[test]
    fn keeps_progress_unless_rebooted_or_changed() {
        let dir = std::env::temp_dir().join(format!("neo-partial-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let part = dir.join("a.bin.neopart");
        std::fs::write(&part, vec![0u8; 4096]).unwrap();
        let files = vec![meta(10_000)];
        let fp = fingerprint(DeviceId([1; 32]), &files);
        let state = FileState { part, final_path: dir.join("a.bin"), size: 10_000, verified: 4096, complete: false };
        let batch = Batch { files: vec![Some(state)], saved_at: 0 };

        // Freshly saved: progress kept.
        save(&dir, &fp, &batch).unwrap();
        assert_eq!(load(&dir, &fp, &files).unwrap().files[0].as_ref().unwrap().verified, 4096);

        // Written before the last boot: unsynced data may be gone, restart the file.
        std::fs::write(path(&dir, &fp), serde_json::to_vec(&batch).unwrap()).unwrap();
        assert_eq!(load(&dir, &fp, &files).unwrap().files[0].as_ref().unwrap().verified, 0);

        // Different file size: the entry is dropped (and the fingerprint differs anyway).
        save(&dir, &fp, &batch).unwrap();
        assert!(load(&dir, &fp, &[meta(9_999)]).unwrap().files[0].is_none());
        assert_ne!(fingerprint(DeviceId([1; 32]), &[meta(9_999)]), fp);

        discard(&dir, &fp);
        assert!(load(&dir, &fp, &files).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
