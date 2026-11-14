//! Send only what changed (rsync/casync style), for direct sends.
//!
//! - **Base:** when the receiver already has a file at the same path in its
//!   downloads folder (usually the previous version you sent), it cuts that
//!   file into content-defined chunks (FastCDC, ~64 KiB average) and sends
//!   their BLAKE3 hashes (16 bytes each) in its `Decision`. Chunking runs on a
//!   blocking thread at disk speed and is cached by path, size and mtime.
//! - **Sender:** cuts the new file the same way as it streams it. Chunks the
//!   receiver has become ~24-byte references; everything else goes as
//!   (compressed) data. Blocks stay 1 MiB, so resume works unchanged.
//! - **Receiver:** builds each block from new data plus its old file and
//!   checks the block's BLAKE3 hash (over the original bytes) before writing.
//!   Every block is checked, so the whole new file is verified before it is
//!   renamed into place. The old file is opened read-only and locked against
//!   writes while in use, and is never modified: the new version is saved
//!   next to it like any other re-send ("name (1).ext").
//! - Skipped for small files (< 4 MiB), files resuming mid-way, swarms, and
//!   when there's no base, so it never makes a send slower.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{anyhow, bail, Result};

use crate::proto::BLOCK;
use crate::wire::Seg;

/// Both the old and new file must be at least this big.
pub const MIN_FILE: u64 = 4 << 20;
const MIN: u32 = 16 << 10;
const AVG: u32 = 64 << 10;
const MAX: u32 = 256 << 10;
/// Chunk hashes one Decision may carry (~8 MB of hex; ~16 GB of base files).
pub const MAX_CHUNKS: usize = 250_000;
/// Cached indexes kept (oldest dropped).
const CACHE_KEEP: usize = 200;

pub type Hash16 = [u8; 16];

pub fn h16(data: &[u8]) -> Hash16 {
    blake3::hash(data).as_bytes()[..16].try_into().unwrap()
}

fn mtime(meta: &std::fs::Metadata) -> u128 {
    meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_nanos()).unwrap_or(0)
}

/// A base file's chunks: (hash, offset, length).
pub struct Index {
    pub size: u64,
    pub mtime: u128,
    pub chunks: Vec<(Hash16, u64, u32)>,
}

fn cache_path(dir: &Path, file: &Path) -> PathBuf {
    let key = blake3::hash(file.to_string_lossy().as_bytes());
    dir.join(format!("{}.cdc", &key.to_hex()[..32]))
}

fn load_cache(p: &Path, size: u64, mtime: u128) -> Option<Index> {
    let b = std::fs::read(p).ok()?;
    if b.len() < 32 || u64::from_le_bytes(b[..8].try_into().ok()?) != size || u128::from_le_bytes(b[8..24].try_into().ok()?) != mtime {
        return None;
    }
    let n = u64::from_le_bytes(b[24..32].try_into().ok()?) as usize;
    let body = &b[32..];
    if body.len() != n * 28 {
        return None;
    }
    let chunks = body
        .chunks_exact(28)
        .map(|c| (c[..16].try_into().unwrap(), u64::from_le_bytes(c[16..24].try_into().unwrap()), u32::from_le_bytes(c[24..28].try_into().unwrap())))
        .collect();
    Some(Index { size, mtime, chunks })
}

fn save_cache(dir: &Path, p: &Path, idx: &Index) {
    let _ = std::fs::create_dir_all(dir);
    let mut b = Vec::with_capacity(32 + idx.chunks.len() * 28);
    b.extend_from_slice(&idx.size.to_le_bytes());
    b.extend_from_slice(&idx.mtime.to_le_bytes());
    b.extend_from_slice(&(idx.chunks.len() as u64).to_le_bytes());
    for (h, off, len) in &idx.chunks {
        b.extend_from_slice(h);
        b.extend_from_slice(&off.to_le_bytes());
        b.extend_from_slice(&len.to_le_bytes());
    }
    let tmp = p.with_extension("tmp");
    if std::fs::write(&tmp, b).is_ok() {
        let _ = std::fs::rename(tmp, p);
    }
    // Keep the cache small.
    if let Ok(rd) = std::fs::read_dir(dir) {
        let mut all: Vec<(std::time::SystemTime, PathBuf)> =
            rd.flatten().filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path()))).collect();
        if all.len() > CACHE_KEEP {
            all.sort();
            for (_, p) in &all[..all.len() - CACHE_KEEP] {
                let _ = std::fs::remove_file(p);
            }
        }
    }
}

/// Drops a cached index (e.g. when its file turned out to differ).
pub fn forget(cache_dir: &Path, file: &Path) {
    let _ = std::fs::remove_file(cache_path(cache_dir, file));
}

/// Chunks `file` (or loads the cached chunks if it hasn't changed).
pub fn index(cache_dir: &Path, file: &Path) -> Result<Index> {
    let meta = std::fs::metadata(file)?;
    let (size, mt) = (meta.len(), mtime(&meta));
    let cp = cache_path(cache_dir, file);
    if let Some(idx) = load_cache(&cp, size, mt) {
        return Ok(idx);
    }
    let f = std::fs::File::open(file)?;
    let mut chunks = Vec::with_capacity((size / AVG as u64) as usize + 1);
    for c in fastcdc::v2020::StreamCDC::new(f, MIN, AVG, MAX) {
        let c = c.map_err(|e| anyhow!("reading {}: {e:?}", file.display()))?;
        chunks.push((h16(&c.data), c.offset, c.length as u32));
        if chunks.len() > MAX_CHUNKS {
            bail!("too many chunks");
        }
    }
    let idx = Index { size, mtime: mt, chunks };
    save_cache(cache_dir, &cp, &idx);
    Ok(idx)
}

/// The receiver's old file, opened for reading chunks while it is in use.
pub struct BaseFile {
    file: std::fs::File,
    pub path: PathBuf,
    map: HashMap<Hash16, (u64, u32)>,
}

impl BaseFile {
    /// Opens `path` (locked against writes) and checks it still matches `idx`.
    pub fn open(path: &Path, idx: &Index) -> Result<BaseFile> {
        let mut opts = std::fs::OpenOptions::new();
        opts.read(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            opts.share_mode(1); // FILE_SHARE_READ: others may read, not write
        }
        let file = opts.open(path)?;
        let meta = file.metadata()?;
        if meta.len() != idx.size || mtime(&meta) != idx.mtime {
            bail!("{} changed", path.display());
        }
        let map = idx.chunks.iter().map(|(h, off, len)| (*h, (*off, *len))).collect();
        Ok(BaseFile { file, path: path.to_path_buf(), map })
    }
}

impl crate::wire::Base for BaseFile {
    fn read(&self, hash: &Hash16, off: u32, len: u32, out: &mut Vec<u8>) -> Result<()> {
        let &(start, clen) = self.map.get(hash).ok_or_else(|| anyhow!("unknown chunk"))?;
        if off as u64 + len as u64 > clen as u64 {
            bail!("reference past its chunk");
        }
        let at = out.len();
        out.resize(at + len as usize, 0);
        read_at(&self.file, &mut out[at..], start + off as u64)?;
        Ok(())
    }
}

#[cfg(windows)]
fn read_at(file: &std::fs::File, buf: &mut [u8], mut offset: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut done = 0;
    while done < buf.len() {
        let n = file.seek_read(&mut buf[done..], offset)?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        done += n;
        offset += n as u64;
    }
    Ok(())
}

#[cfg(unix)]
fn read_at(file: &std::fs::File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}

/// Hex of all chunk hashes (what goes in the Decision).
pub fn to_wire(idx: &Index) -> String {
    let mut s = String::with_capacity(idx.chunks.len() * 32);
    for (h, ..) in &idx.chunks {
        for b in h {
            s.push_str(&format!("{b:02x}"));
        }
    }
    s
}

pub fn from_wire(s: &str) -> Option<HashSet<Hash16>> {
    if s.is_empty() || s.len() % 32 != 0 {
        return None;
    }
    let bytes = crate::identity::unhex(s)?;
    Some(bytes.chunks_exact(16).map(|c| c.try_into().unwrap()).collect())
}

/// Streams `source` (a whole file, from its start) as 1 MiB blocks split into
/// pieces: `emit(block, pieces)` for each, where pieces reference chunks in
/// `known` or ranges of new bytes. Stops early if `emit` returns false.
pub fn split(source: impl Read, known: &HashSet<Hash16>, mut emit: impl FnMut(&[u8], &[Seg]) -> Result<bool>) -> Result<()> {
    let mut block: Vec<u8> = Vec::with_capacity(BLOCK as usize);
    let mut segs: Vec<Seg> = Vec::new();
    for c in fastcdc::v2020::StreamCDC::new(source, MIN, AVG, MAX) {
        let c = c.map_err(|e| anyhow!("reading: {e:?}"))?;
        let hash = h16(&c.data);
        let hit = known.contains(&hash);
        let mut at = 0usize;
        while at < c.data.len() {
            let take = (BLOCK as usize - block.len()).min(c.data.len() - at);
            let start = block.len();
            block.extend_from_slice(&c.data[at..at + take]);
            if hit {
                segs.push(Seg::Ref { hash, off: at as u32, len: take as u32 });
            } else if let Some(Seg::Lit { end, .. }) = segs.last_mut() {
                *end = start + take;
            } else {
                segs.push(Seg::Lit { start, end: start + take });
            }
            at += take;
            if block.len() == BLOCK as usize {
                if !emit(&block, &segs)? {
                    return Ok(());
                }
                block.clear();
                segs.clear();
            }
        }
    }
    if !block.is_empty() {
        emit(&block, &segs)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{Base, Decoder, Encoder, PIECES};

    fn noise(n: usize, seed: u64) -> Vec<u8> {
        let mut x = seed | 1;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    #[test]
    fn an_edited_file_travels_as_its_changes() {
        let dir = std::env::temp_dir().join(format!("neo-delta-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let old = noise(12 << 20, 3);
        let old_path = dir.join("old.bin");
        std::fs::write(&old_path, &old).unwrap();

        // Insert near the start, delete in the middle, append at the end.
        let mut new = old[..1_000_000].to_vec();
        new.extend_from_slice(&noise(5000, 9));
        new.extend_from_slice(&old[1_000_000..6_000_000]);
        new.extend_from_slice(&old[6_300_000..]);
        new.extend_from_slice(&noise(70_000, 11));

        let idx = index(&dir.join("cache"), &old_path).unwrap();
        let known = from_wire(&to_wire(&idx)).unwrap();
        let base = BaseFile::open(&old_path, &idx).unwrap();
        let (mut enc, mut dec) = (Encoder::new("new.bin"), Decoder::new().unwrap());
        let (mut rebuilt, mut wire) = (Vec::new(), 0usize);
        split(&new[..], &known, |block, segs| {
            let (tag, body) = enc.pieces(block, segs);
            assert_eq!(tag, PIECES);
            wire += body.len();
            let (raw, _) = dec.decode(tag, body.to_vec(), block.len(), Some(&base as &dyn Base)).unwrap();
            assert_eq!(raw, block);
            rebuilt.extend_from_slice(&raw);
            Ok(true)
        })
        .unwrap();
        assert_eq!(rebuilt, new);
        // About 75 KB of new bytes plus a few chunks around each edit.
        assert!(wire < 600_000, "sent {wire} bytes for a {} byte file", new.len());

        // The cache answers the second time; a changed file is re-chunked.
        let again = index(&dir.join("cache"), &old_path).unwrap();
        assert_eq!(again.chunks, idx.chunks);
        drop(base);
        std::fs::write(&old_path, &new).unwrap();
        let changed = index(&dir.join("cache"), &old_path).unwrap();
        assert_eq!(changed.size, new.len() as u64);
        assert!(BaseFile::open(&old_path, &idx).is_err(), "a stale index is refused");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
