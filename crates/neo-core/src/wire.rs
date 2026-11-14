//! How a 1 MiB block travels: raw, zstd-compressed, or (delta) as pieces of
//! new data plus references to chunks the receiver already has. Whatever the
//! form, the BLAKE3 hash is always over the original bytes and the receiver
//! checks it before writing, so the saved file is byte-identical to the
//! sender's. Compression and delta are only a wire format.
//!
//! On a file stream every block is `tag u8` then:
//! - `RAW`: the block's bytes (the receiver knows its length), hash.
//! - `ZSTD`: `u32` payload length, the compressed bytes, hash.
//! - `PIECES`: `u32` payload length, payload, hash. The payload is a list of
//!   `0 len u32 bytes` (raw), `1 len u32 clen u32 bytes` (zstd) and
//!   `2 hash16 off u32 len u32` (bytes `off..off+len` of the receiver's chunk
//!   with that hash).
//!
//! **When to compress:** zstd level 1 (hundreds of MB/s per core), one context
//! per stream. Every file starts with a real test: if a block doesn't shrink by
//! at least 8%, it goes raw, and after two misses the file goes raw, re-trying
//! one block in 64 (mixed content, archives with stored parts). Names like
//! `.zip`/`.mp4`/`.jpg` start in the "raw, re-try now and then" state instead
//! of trying every block.
//!
//! **CPU never sets the pace:** the reader thread measures itself over windows
//! of 16 blocks. If it was almost never held back by the network (so the CPU
//! is the bottleneck, as on a fast LAN), it tries 16 blocks raw and keeps
//! whichever way actually moved more bytes per second, checking again later.

use std::time::Instant;

use anyhow::{anyhow, bail, Result};
use bytes::{BufMut, Bytes, BytesMut};

pub const RAW: u8 = 0;
pub const ZSTD: u8 = 1;
pub const PIECES: u8 = 2;

const SEG_RAW: u8 = 0;
const SEG_ZSTD: u8 = 1;
const SEG_REF: u8 = 2;

/// Compressed must be at most this share of the original to be worth it.
const WORTH: f64 = 0.92;
/// Raw files re-try compression on one block in this many.
const RETRY_EVERY: u32 = 64;
const LEVEL: i32 = 1;

/// Largest body a block can have on the wire (references add a little).
pub const MAX_BODY: usize = crate::proto::BLOCK as usize + (64 << 10);

/// Extensions that are almost always already compressed.
fn compressed_kind(name: &str) -> bool {
    const EXT: &[&str] = &[
        "zip", "7z", "rar", "gz", "tgz", "bz2", "xz", "zst", "lz4", "br", "cab", "jar", "apk", "aab", "ipa", "xpi", "whl",
        "nupkg", "msix", "appx", "dmg", "iso", "jpg", "jpeg", "png", "gif", "webp", "avif", "heic", "heif", "jxl", "mp4",
        "m4v", "mkv", "webm", "mov", "avi", "wmv", "flv", "mp3", "m4a", "aac", "ogg", "opus", "flac", "wma", "pdf", "docx",
        "xlsx", "pptx", "odt", "ods", "odp", "epub", "woff", "woff2",
    ];
    name.rsplit_once('.').is_some_and(|(_, e)| EXT.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

/// Compression is on unless `NEO_COMPRESS=0` (benchmarks).
pub fn enabled() -> bool {
    std::env::var("NEO_COMPRESS").map(|v| v != "0").unwrap_or(true)
}

/// Whether the CPU lets us compress right now (see the module docs).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Cpu {
    Compress,
    /// Trying raw; compressed managed this many bytes per second.
    TryRaw(f64),
    /// Raw was faster; windows left before trying compression again.
    Raw(u32),
}

/// Blocks per measuring window.
const WINDOW: u32 = 16;

/// Per-file compression state, used on one reader thread.
pub struct Encoder {
    ctx: Option<zstd::bulk::Compressor<'static>>,
    /// The content compresses (or hasn't been shown not to).
    on: bool,
    misses: u32,
    /// Blocks until the next re-try while off.
    retry_in: u32,
    cpu: Cpu,
    /// Whether this window compressed anything.
    compressed: bool,
    win_start: Instant,
    win_blocks: u32,
    win_raw: u64,
    win_blocked: f64,
}

impl Encoder {
    pub fn new(name: &str) -> Self {
        let enabled = enabled();
        let hinted_off = compressed_kind(name);
        Encoder {
            ctx: if enabled { zstd::bulk::Compressor::new(LEVEL).ok() } else { None },
            on: enabled && !hinted_off,
            misses: 0,
            retry_in: if hinted_off { 1 } else { RETRY_EVERY },
            cpu: Cpu::Compress,
            compressed: false,
            win_start: Instant::now(),
            win_blocks: 0,
            win_raw: 0,
            win_blocked: 0.0,
        }
    }

    /// Reports a block handed to the network: its original size and how long
    /// the hand-off waited because the network was busy.
    pub fn sent(&mut self, raw: u64, blocked: f64) {
        self.win_blocks += 1;
        self.win_raw += raw;
        self.win_blocked += blocked;
        if self.win_blocks < WINDOW {
            return;
        }
        let elapsed = self.win_start.elapsed().as_secs_f64().max(1e-6);
        let rate = self.win_raw as f64 / elapsed;
        let cpu_bound = self.win_blocked < elapsed * 0.1;
        self.cpu = match self.cpu {
            Cpu::Compress if cpu_bound && self.compressed => Cpu::TryRaw(rate),
            Cpu::Compress => Cpu::Compress,
            Cpu::TryRaw(compressed_rate) if rate > compressed_rate * 1.15 => Cpu::Raw(4),
            Cpu::TryRaw(_) => Cpu::Compress,
            Cpu::Raw(n) if n > 1 => Cpu::Raw(n - 1),
            Cpu::Raw(_) => Cpu::Compress,
        };
        crate::debug(&format!("compression window: {:.0} MB/s, cpu-bound {cpu_bound}, now {:?}", rate / 1e6, self.cpu));
        self.compressed = false;
        self.win_start = Instant::now();
        self.win_blocks = 0;
        self.win_raw = 0;
        self.win_blocked = 0.0;
    }

    /// Compresses `raw` if that's worthwhile right now; None = send raw.
    fn try_compress(&mut self, raw: &[u8]) -> Option<Vec<u8>> {
        if self.cpu != Cpu::Compress {
            return None;
        }
        let ctx = self.ctx.as_mut()?;
        let attempt = if self.on {
            true
        } else {
            self.retry_in = self.retry_in.saturating_sub(1);
            self.retry_in == 0
        };
        if !attempt || raw.len() < 512 {
            return None;
        }
        let out = ctx.compress(raw).ok();
        match out {
            Some(c) if (c.len() as f64) <= raw.len() as f64 * WORTH => {
                self.on = true;
                self.misses = 0;
                self.compressed = true;
                Some(c)
            }
            _ => {
                self.misses += 1;
                if !self.on || self.misses >= 2 {
                    self.on = false;
                    self.retry_in = RETRY_EVERY;
                }
                None
            }
        }
    }

    /// One whole block: (tag, payload).
    pub fn block(&mut self, raw: Bytes) -> (u8, Bytes) {
        match self.try_compress(&raw) {
            Some(c) => (ZSTD, Bytes::from(c)),
            None => (RAW, raw),
        }
    }

    /// A delta block from pieces; `block` holds all its original bytes.
    pub fn pieces(&mut self, block: &[u8], segs: &[Seg]) -> (u8, Bytes) {
        let mut body = BytesMut::with_capacity(block.len() / 2 + 64);
        for s in segs {
            match *s {
                Seg::Lit { start, end } => {
                    let lit = &block[start..end];
                    match self.try_compress(lit) {
                        Some(c) => {
                            body.put_u8(SEG_ZSTD);
                            body.put_u32_le(lit.len() as u32);
                            body.put_u32_le(c.len() as u32);
                            body.put_slice(&c);
                        }
                        None => {
                            body.put_u8(SEG_RAW);
                            body.put_u32_le(lit.len() as u32);
                            body.put_slice(lit);
                        }
                    }
                }
                Seg::Ref { hash, off, len } => {
                    body.put_u8(SEG_REF);
                    body.put_slice(&hash);
                    body.put_u32_le(off);
                    body.put_u32_le(len);
                }
            }
        }
        (PIECES, body.freeze())
    }
}

/// Part of a delta block: new bytes (a range of the block) or a reference.
#[derive(Clone, Copy, Debug)]
pub enum Seg {
    Lit { start: usize, end: usize },
    Ref { hash: [u8; 16], off: u32, len: u32 },
}

/// Where referenced chunks come from (the receiver's old version of a file).
pub trait Base {
    /// Bytes `off..off+len` of the chunk with this hash, appended to `out`.
    fn read(&self, hash: &[u8; 16], off: u32, len: u32, out: &mut Vec<u8>) -> Result<()>;
}

/// Turns blocks back into original bytes (one per stream, on the writer thread).
pub struct Decoder {
    ctx: zstd::bulk::Decompressor<'static>,
}

impl Decoder {
    pub fn new() -> Result<Self> {
        Ok(Decoder { ctx: zstd::bulk::Decompressor::new()? })
    }

    /// `body` is the payload; `len` the block's original length. Returns the original bytes and
    /// whether any of them came from the base.
    pub fn decode(&mut self, tag: u8, body: Vec<u8>, len: usize, base: Option<&dyn Base>) -> Result<(Vec<u8>, bool)> {
        match tag {
            RAW => Ok((body, false)),
            ZSTD => {
                let out = self.ctx.decompress(&body, len).map_err(|e| anyhow!("bad compressed block: {e}"))?;
                Ok((out, false))
            }
            PIECES => {
                let mut out = Vec::with_capacity(len);
                let mut used_base = false;
                let mut p = &body[..];
                let take = |p: &mut &[u8], n: usize| -> Result<Vec<u8>> {
                    if p.len() < n {
                        bail!("truncated block");
                    }
                    let (a, b) = p.split_at(n);
                    *p = b;
                    Ok(a.to_vec())
                };
                let u32_of = |v: Vec<u8>| u32::from_le_bytes(v.try_into().unwrap()) as usize;
                while !p.is_empty() {
                    let kind = take(&mut p, 1)?[0];
                    match kind {
                        SEG_RAW => {
                            let n = u32_of(take(&mut p, 4)?);
                            if out.len() + n > len {
                                bail!("block too long");
                            }
                            out.extend_from_slice(&take(&mut p, n)?);
                        }
                        SEG_ZSTD => {
                            let n = u32_of(take(&mut p, 4)?);
                            let c = u32_of(take(&mut p, 4)?);
                            if out.len() + n > len {
                                bail!("block too long");
                            }
                            let data = take(&mut p, c)?;
                            let raw = self.ctx.decompress(&data, n).map_err(|e| anyhow!("bad compressed piece: {e}"))?;
                            if raw.len() != n {
                                bail!("bad compressed piece");
                            }
                            out.extend_from_slice(&raw);
                        }
                        SEG_REF => {
                            let hash: [u8; 16] = take(&mut p, 16)?.try_into().unwrap();
                            let off = u32_of(take(&mut p, 4)?) as u32;
                            let n = u32_of(take(&mut p, 4)?);
                            if out.len() + n > len {
                                bail!("block too long");
                            }
                            let base = base.ok_or_else(|| anyhow!("reference without a base"))?;
                            base.read(&hash, off, n as u32, &mut out)?;
                            used_base = true;
                        }
                        _ => bail!("unknown piece"),
                    }
                }
                Ok((out, used_base))
            }
            _ => bail!("unknown block type {tag}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(n: usize) -> Vec<u8> {
        let words = ["neo ", "sends ", "files ", "quickly ", "and ", "safely, ", "block ", "by ", "block. "];
        let mut out = Vec::with_capacity(n);
        let mut i = 7usize;
        while out.len() < n {
            i = i.wrapping_mul(2654435761).wrapping_add(1);
            out.extend_from_slice(words[i % words.len()].as_bytes());
        }
        out.truncate(n);
        out
    }

    fn noise(n: usize) -> Vec<u8> {
        let mut x = 88172645463325252u64;
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
    fn compressible_shrinks_random_goes_raw_and_both_round_trip() {
        let mut dec = Decoder::new().unwrap();
        let mut enc = Encoder::new("log.txt");
        let t = text(1 << 20);
        let (tag, body) = enc.block(Bytes::from(t.clone()));
        assert_eq!(tag, ZSTD);
        assert!(body.len() < t.len() / 3, "{} bytes", body.len());
        let (back, _) = dec.decode(tag, body.to_vec(), t.len(), None).unwrap();
        assert_eq!(back, t);

        let mut enc = Encoder::new("data.bin");
        let r = noise(1 << 20);
        let tags: Vec<u8> = (0..70).map(|_| enc.block(Bytes::from(r.clone())).0).collect();
        assert!(tags.iter().all(|t| *t == RAW));
        // Two real tries, then only a re-try every 64 blocks.
        assert_eq!(enc.misses, 3, "tried blocks 1, 2 and 66");
    }

    #[test]
    fn a_cpu_bound_reader_tries_raw_and_keeps_the_faster() {
        let mut enc = Encoder::new("log.txt");
        let t = Bytes::from(text(1 << 20));
        let window = |enc: &mut Encoder, blocked: f64| {
            for _ in 0..WINDOW {
                enc.block(t.clone());
                enc.sent(1 << 20, blocked);
            }
        };
        // Held back by the network: compression is free, keep it.
        window(&mut enc, 1.0);
        assert_eq!(enc.cpu, Cpu::Compress);
        // Never held back: try raw. Raw went much faster: stay raw a while.
        window(&mut enc, 0.0);
        assert!(matches!(enc.cpu, Cpu::TryRaw(_)));
        enc.cpu = Cpu::TryRaw(1.0); // compressed managed 1 B/s
        window(&mut enc, 0.0);
        assert_eq!(enc.cpu, Cpu::Raw(4));
        assert_eq!(enc.block(t.clone()).0, RAW);
        for _ in 0..4 {
            window(&mut enc, 0.0);
        }
        assert_eq!(enc.cpu, Cpu::Compress, "and tries compression again later");
        assert_eq!(enc.block(t.clone()).0, ZSTD);
    }

    #[test]
    fn already_compressed_names_only_sample() {
        let mut enc = Encoder::new("movie.MP4");
        assert!(!enc.on);
        // Its first block is still tested; text inside a "zip" does get compressed.
        let (tag, _) = enc.block(Bytes::from(text(1 << 20)));
        assert_eq!(tag, ZSTD);
        assert!(enc.on);
    }

    struct Fixed(Vec<u8>);
    impl Base for Fixed {
        fn read(&self, _: &[u8; 16], off: u32, len: u32, out: &mut Vec<u8>) -> Result<()> {
            out.extend_from_slice(&self.0[off as usize..(off + len) as usize]);
            Ok(())
        }
    }

    #[test]
    fn pieces_mix_new_bytes_and_references() {
        let old = text(300_000);
        let mut block = b"NEW HEADER ".to_vec();
        block.extend_from_slice(&old[1000..201_000]);
        let tail = noise(5000);
        block.extend_from_slice(&tail);
        let segs = [
            Seg::Lit { start: 0, end: 11 },
            Seg::Ref { hash: [1; 16], off: 1000, len: 200_000 },
            Seg::Lit { start: 200_011, end: block.len() },
        ];
        let mut enc = Encoder::new("x.db");
        let (tag, body) = enc.pieces(&block, &segs);
        assert_eq!(tag, PIECES);
        assert!(body.len() < 6000);
        let mut dec = Decoder::new().unwrap();
        let (back, used) = dec.decode(tag, body.to_vec(), block.len(), Some(&Fixed(old))).unwrap();
        assert!(used);
        assert_eq!(back, block);
        // A hostile length can't make it allocate or write past the block.
        assert!(dec.decode(tag, body.to_vec(), 100, Some(&Fixed(text(300_000)))).is_err());
    }
}
