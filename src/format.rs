//! On-disk layout.
//!
//! ```text
//! header   MAGIC(8) flags(1) [salt(16) m_kib(4) t(4) p(4)]   -- KDF params only when encrypted
//! blocks   stored block bytes, back to back (nonce||ciphertext||tag when encrypted)
//! index    zstd(Index::encode()), sealed when encrypted
//! footer   index_offset(8) index_len(8) FOOTER_MAGIC(8)
//! ```
//!
//! Files are split into chunks; unique chunks are packed into solid blocks in the
//! order they were first seen, so a chunk's offset inside its block is implied by
//! the lengths of the chunks before it and is not stored. A block with a non-zero
//! `prefix` was compressed with the previous block's raw tail as dictionary;
//! runs of such blocks form chains that decode in order.

use anyhow::{bail, Context, Result};

pub const MAGIC: &[u8; 8] = b"DALZIP\0\x01";
pub const FOOTER_MAGIC: &[u8; 8] = b"DZINDEX\x01";
pub const FOOTER_LEN: u64 = 24;
pub const FLAG_ENCRYPTED: u8 = 1;
const INDEX_VERSION: u64 = 2;

pub mod codec_id {
    pub const STORE: u8 = 0;
    pub const ZSTD: u8 = 1;
    /// dict_size(4) + raw LZMA2 stream
    pub const LZMA2: u8 = 2;
    pub const PPMD: u8 = 3;
}

pub mod filter_id {
    pub const NONE: u8 = 0;
    pub const E8E9: u8 = 1;
}

#[derive(Clone, Debug)]
pub struct BlockInfo {
    pub offset: u64,
    pub stored_len: u64,
    pub raw_len: u64,
    pub codec: u8,
    pub filter: u8,
    /// Length of the raw tail of the preceding blocks used as preset dictionary;
    /// non-zero means this block can only be decoded after those blocks.
    pub prefix: u32,
    pub hash: [u8; 32],
}

#[derive(Clone, Copy, Debug)]
pub struct ChunkRef {
    pub block: u32,
    pub offset: u32,
    pub len: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
}

#[derive(Clone, Debug)]
pub struct Entry {
    /// '/'-separated relative path.
    pub path: String,
    pub kind: Kind,
    pub size: u64,
    pub mtime: i64,
    pub mtime_ns: u32,
    pub readonly: bool,
    pub hash: [u8; 32],
    pub chunks: Vec<u32>,
}

#[derive(Default)]
pub struct Index {
    pub blocks: Vec<BlockInfo>,
    pub chunks: Vec<ChunkRef>,
    pub entries: Vec<Entry>,
}

impl Index {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = W::default();
        w.var(INDEX_VERSION);

        w.var(self.blocks.len() as u64);
        for b in &self.blocks {
            w.var(b.offset);
            w.var(b.stored_len);
            w.var(b.raw_len);
            w.u8(b.codec);
            w.u8(b.filter);
            w.var(b.prefix as u64);
            w.bytes(&b.hash);
        }

        w.var(self.chunks.len() as u64);
        let mut prev_block = 0u32;
        for c in &self.chunks {
            w.var((c.block - prev_block) as u64);
            w.var(c.len as u64);
            prev_block = c.block;
        }

        w.var(self.entries.len() as u64);
        for e in &self.entries {
            w.str(&e.path);
            w.u8(match e.kind {
                Kind::File => 0,
                Kind::Dir => 1,
            });
            w.ivar(e.mtime);
            w.var(e.mtime_ns as u64);
            w.u8(e.readonly as u8);
            if e.kind == Kind::File {
                w.var(e.size);
                w.bytes(&e.hash);
                w.var(e.chunks.len() as u64);
                let mut prev = -1i64;
                for &id in &e.chunks {
                    w.ivar(id as i64 - prev - 1);
                    prev = id as i64;
                }
            }
        }
        w.0
    }

    /// Decodes and validates every cross-reference, so later code can index freely.
    pub fn decode(buf: &[u8]) -> Result<Index> {
        let mut r = R { buf, pos: 0 };
        let ver = r.var()?;
        if ver != INDEX_VERSION {
            bail!("unsupported index version {ver}");
        }

        let n = r.count()?;
        let mut blocks: Vec<BlockInfo> = Vec::with_capacity(n);
        let mut before = 0u64;
        for i in 0..n {
            let b = BlockInfo {
                offset: r.var()?,
                stored_len: r.var()?,
                raw_len: r.var()?,
                codec: r.u8()?,
                filter: r.u8()?,
                prefix: r.var()? as u32,
                hash: r.hash()?,
            };
            if b.prefix as u64 > before {
                bail!("block {i} prefix reaches before the first block");
            }
            before += b.raw_len;
            blocks.push(b);
        }

        let n = r.count()?;
        let mut chunks = Vec::with_capacity(n);
        let mut cur: Option<u32> = None;
        let mut off = 0u64;
        for _ in 0..n {
            let block = cur.unwrap_or(0) as u64 + r.var()?;
            let len = r.var()?;
            if block >= blocks.len() as u64 {
                bail!("chunk references missing block {block}");
            }
            let block = block as u32;
            if cur != Some(block) {
                cur = Some(block);
                off = 0;
            }
            if off + len > blocks[block as usize].raw_len || len > u32::MAX as u64 {
                bail!("chunk overruns block {block}");
            }
            chunks.push(ChunkRef { block, offset: off as u32, len: len as u32 });
            off += len;
        }

        let n = r.count()?;
        let mut entries = Vec::with_capacity(n);
        for _ in 0..n {
            let path = r.str()?;
            let kind = match r.u8()? {
                0 => Kind::File,
                1 => Kind::Dir,
                k => bail!("unknown entry kind {k}"),
            };
            let mtime = r.ivar()?;
            let mtime_ns = r.var()? as u32;
            let readonly = r.u8()? != 0;
            let mut e = Entry { path, kind, size: 0, mtime, mtime_ns, readonly, hash: [0; 32], chunks: Vec::new() };
            if kind == Kind::File {
                e.size = r.var()?;
                e.hash = r.hash()?;
                let k = r.count()?;
                e.chunks.reserve(k);
                let mut prev = -1i64;
                let mut total = 0u64;
                for _ in 0..k {
                    let id = prev + 1 + r.ivar()?;
                    if id < 0 || id >= chunks.len() as i64 {
                        bail!("entry {} references missing chunk", e.path);
                    }
                    total += chunks[id as usize].len as u64;
                    e.chunks.push(id as u32);
                    prev = id;
                }
                if total != e.size {
                    bail!("entry {} size does not match its chunks", e.path);
                }
            }
            entries.push(e);
        }
        Ok(Index { blocks, chunks, entries })
    }
}

#[derive(Default)]
pub struct W(pub Vec<u8>);

impl W {
    pub fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    pub fn var(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.0.push(v as u8 | 0x80);
            v >>= 7;
        }
        self.0.push(v as u8);
    }
    pub fn ivar(&mut self, v: i64) {
        self.var(((v << 1) ^ (v >> 63)) as u64);
    }
    pub fn bytes(&mut self, b: &[u8]) {
        self.0.extend_from_slice(b);
    }
    pub fn str(&mut self, s: &str) {
        self.var(s.len() as u64);
        self.bytes(s.as_bytes());
    }
}

pub struct R<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> R<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.buf.len()).context("index truncated")?;
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn var(&mut self) -> Result<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = self.u8()?;
            v |= ((b & 0x7f) as u64) << shift;
            if b < 0x80 {
                return Ok(v);
            }
        }
        bail!("varint overflow")
    }
    fn ivar(&mut self) -> Result<i64> {
        let v = self.var()?;
        Ok((v >> 1) as i64 ^ -((v & 1) as i64))
    }
    /// A length prefix for a list; bounded by the remaining bytes so a corrupt
    /// count cannot trigger a huge allocation.
    fn count(&mut self) -> Result<usize> {
        let n = self.var()?;
        if n > (self.buf.len() - self.pos) as u64 {
            bail!("index count out of range");
        }
        Ok(n as usize)
    }
    fn hash(&mut self) -> Result<[u8; 32]> {
        Ok(self.take(32)?.try_into().unwrap())
    }
    fn str(&mut self) -> Result<String> {
        let n = self.count()?;
        Ok(String::from_utf8(self.take(n)?.to_vec()).context("path is not UTF-8")?)
    }
}
