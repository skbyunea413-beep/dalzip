//! Per-block codec choice: compressibility probe, optional E8/E9 filter, then
//! LZMA2 or zstd, or at the top levels every candidate with the smallest kept.
//!
//! Blocks are small enough to compress in parallel, but each one may be primed
//! with the raw tail of the preceding blocks in its chain (up to the dictionary
//! size) as a preset dictionary, so matches across boundaries are not lost. The prefix is raw data the
//! producer already has, so compression never waits on another block; only
//! decompression walks the chain in order.

use crate::filter;
use crate::format::{codec_id, filter_id};
use anyhow::{anyhow, bail, Context, Result};
use ppmd_rust::{Ppmd8Decoder, Ppmd8Encoder, RestoreMethod};
use std::borrow::Cow;
use std::io::{Read, Write};

const MB: usize = 1 << 20;
const ZSTD_MAX_WINDOW_LOG: u32 = 27;

/// What most of a block's bytes came from; set by the block builder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockKind {
    Text,
    Exec,
    Other,
}

#[derive(Clone, Copy, Debug)]
pub struct Profile {
    pub level: u8,
    pub block_size: usize,
    /// Skip blocks that a fast sample says are already compressed.
    pub probe: bool,
    pub zstd: Option<(i32, bool)>,
    /// (preset, extreme); the dictionary covers prefix + block.
    pub lzma: Option<(u32, bool)>,
    /// Most chain history a block is primed with. Context = prefix + block, but
    /// the codec has to index the prefix again for every block, so this trades
    /// ratio against time.
    pub prefix_cap: usize,
    /// (order, model memory)
    pub ppmd: Option<(u32, u32)>,
    /// Compress with every configured codec and keep the smallest.
    pub try_all: bool,
    /// Max blocks in one prefix chain (1 = independent blocks). Bounds how much
    /// must be decoded to reach a file in the middle of an archive.
    pub chain: u32,
}

impl Profile {
    pub fn for_level(level: u8, block_mb: Option<usize>) -> Profile {
        let p = |block, prefix, zstd, lzma, ppmd, try_all| Profile {
            level,
            block_size: block * MB,
            probe: level >= 2,
            zstd,
            lzma,
            prefix_cap: prefix * MB,
            ppmd,
            try_all,
            chain: 1,
        };
        // Block / prefix sizes picked by sweeping them on a mixed 118 MiB corpus
        // against Bandizip 7z -l:5 and -l:9 (see README).
        let mut prof = match level {
            0 => p(64, 0, None, None, None, false),
            1 => p(4, 4, Some((3, false)), None, None, false),
            2 => p(8, 8, Some((9, true)), None, None, false),
            3 => p(8, 16, Some((19, true)), None, None, false),
            4 => p(8, 4, None, Some((6, false)), None, false),
            5 => p(8, 8, None, Some((6, false)), None, false),
            6 => p(16, 32, None, Some((9, true)), None, false),
            7 => p(32, 32, None, Some((9, true)), Some((10, 256 << 20)), true),
            8 => p(64, 64, None, Some((9, true)), Some((12, 512 << 20)), true),
            _ => p(64, 192, Some((19, true)), Some((9, true)), Some((16, 1024 << 20)), true),
        };
        if let Some(mb) = block_mb {
            prof.block_size = mb.clamp(1, 1024) * MB;
        }
        if level > 0 {
            // Keep a chain to ~256 MiB of raw data.
            prof.chain = ((256 * MB) / prof.block_size).max(1) as u32;
        }
        prof
    }

    fn stored_only(&self) -> bool {
        self.zstd.is_none() && self.lzma.is_none() && self.ppmd.is_none()
    }

    fn lzma_prefix(&self, prev: usize) -> usize {
        prev.min(self.prefix_cap)
    }

    fn zstd_prefix(&self, prev: usize, len: usize) -> usize {
        prev.min(self.prefix_cap).min((1usize << ZSTD_MAX_WINDOW_LOG).saturating_sub(len))
    }

    /// How much chain history a block of `len` bytes can use, given `history` available.
    pub fn max_prefix(&self, history: usize, len: usize) -> usize {
        let l = if self.lzma.is_some() { self.lzma_prefix(history) } else { 0 };
        let z = if self.zstd.is_some() { self.zstd_prefix(history, len) } else { 0 };
        l.max(z)
    }

    /// Rough peak memory of one compression worker (queued input, input,
    /// previous block, filtered copies, output, codec state). Used to cap threads.
    pub fn mem_per_thread(&self) -> u64 {
        let b = self.block_size;
        let pre = self.max_prefix(usize::MAX, b);
        let z = self.zstd.map_or(0, |(l, _)| zstd_mem(l, pre + b));
        let x = self.lzma.map_or(0, |_| lzma_dict(pre + b) as u64 * 11 + 8 * MB as u64);
        let p = self.ppmd.map_or(0, |(_, m)| m as u64);
        // Tail copy and its filtered copy come on top of the blocks themselves.
        b as u64 * if self.try_all { 7 } else { 6 } + 2 * pre as u64 + z.max(x).max(p)
    }

    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some((l, long)) = self.zstd {
            parts.push(format!("zstd-{l}{}", if long { " long" } else { "" }));
        }
        if let Some((p, e)) = self.lzma {
            parts.push(format!("lzma2-{p}{}", if e { "e" } else { "" }));
        }
        if let Some((o, m)) = self.ppmd {
            parts.push(format!("ppmd-o{o} {}M", m >> 20));
        }
        let how = if self.stored_only() {
            "store".to_string()
        } else if self.try_all {
            format!("best of {}", parts.join(" / "))
        } else {
            parts.join(" / ")
        };
        let chain = if self.chain > 1 && self.prefix_cap > 0 {
            format!(", primed with {} MiB of history", self.prefix_cap / MB)
        } else {
            String::new()
        };
        format!("level {} ({how}, {} MiB blocks{chain})", self.level, self.block_size / MB)
    }
}

fn window_log(len: usize) -> u32 {
    let mut w = 10;
    while (1usize << w) < len && w < ZSTD_MAX_WINDOW_LOG {
        w += 1;
    }
    w
}

fn zstd_mem(level: i32, span: usize) -> u64 {
    let win = 1u64 << window_log(span);
    match level {
        ..=3 => 32 * MB as u64,
        4..=12 => win * 4,
        _ => win * 8,
    }
}

fn lzma_dict(len: usize) -> u32 {
    len.next_power_of_two().clamp(4096, 1 << 30) as u32
}

pub struct Encoded {
    pub codec: u8,
    pub filter: u8,
    /// Bytes of chain history (the raw tail of the preceding blocks) used as preset dictionary.
    pub prefix: u32,
    pub data: Vec<u8>,
}

/// The last `n` bytes of the concatenation of `blocks`.
pub fn history_tail(blocks: &[&[u8]], n: usize) -> Vec<u8> {
    let mut parts = Vec::new();
    let mut left = n;
    for b in blocks.iter().rev() {
        if left == 0 {
            break;
        }
        let take = left.min(b.len());
        parts.push(&b[b.len() - take..]);
        left -= take;
    }
    let mut out = Vec::with_capacity(n - left);
    for p in parts.iter().rev() {
        out.extend_from_slice(p);
    }
    out
}

/// The preset dictionary as the codec sees it: the raw tail of the previous
/// block, run through this block's filter. Encoder and decoder both build it
/// this way, so they agree byte for byte.
fn dictionary(tail: &[u8], filter: u8) -> Cow<'_, [u8]> {
    if filter == filter_id::E8E9 {
        Cow::Owned(filter::e8e9_encode(tail))
    } else {
        Cow::Borrowed(tail)
    }
}

pub fn encode(raw: &[u8], prev: Option<&[u8]>, kind: BlockKind, p: &Profile) -> Result<Encoded> {
    let store = || Encoded { codec: codec_id::STORE, filter: filter_id::NONE, prefix: 0, data: raw.to_vec() };
    if p.stored_only() || raw.is_empty() {
        return Ok(store());
    }
    if p.probe && raw.len() >= 256 << 10 && !looks_compressible(raw) {
        return Ok(store());
    }
    let prev = prev.unwrap_or(&[]);

    let mut variants = vec![filter_id::NONE];
    if kind == BlockKind::Exec {
        // The filter is a guess from file extensions; at try_all levels check it paid off.
        variants.insert(0, filter_id::E8E9);
        if !p.try_all {
            variants.truncate(1);
        }
    }

    let mut best: Option<Encoded> = None;
    let debug = std::env::var_os("DZ_DEBUG").is_some();
    let mut consider = |e: Encoded| {
        if debug {
            eprintln!(
                "  [{kind:?} {} KiB +{} KiB prefix] codec {} filter {}: {} KiB",
                raw.len() >> 10,
                e.prefix >> 10,
                e.codec,
                e.filter,
                e.data.len() >> 10
            );
        }
        if best.as_ref().map_or(true, |b| e.data.len() < b.data.len()) {
            best = Some(e);
        }
    };

    for &filter in &variants {
        let input = dictionary(raw, filter);
        let lz_tail = &prev[prev.len() - p.lzma_prefix(prev.len())..];
        let z_tail = &prev[prev.len() - p.zstd_prefix(prev.len(), raw.len())..];
        let mk = |codec, tail: &[u8], data| Encoded { codec, filter, prefix: tail.len() as u32, data };

        let use_lzma = p.lzma.is_some();
        let use_zstd = p.zstd.is_some() && (p.try_all || p.lzma.is_none());
        if let (true, Some((preset, extreme))) = (use_lzma, p.lzma) {
            let dict = dictionary(lz_tail, filter);
            consider(mk(codec_id::LZMA2, lz_tail, lzma2_compress(&input, &dict, kind, preset, extreme)?));
            // Text-tuned literal coding also wins on some "binary" data (UTF-16, tables).
            if p.try_all && kind == BlockKind::Other {
                let d = lzma2_compress(&input, &dict, BlockKind::Text, preset, extreme)?;
                consider(mk(codec_id::LZMA2, lz_tail, d));
            }
        }
        if let (true, Some((level, long))) = (use_zstd, p.zstd) {
            let dict = dictionary(z_tail, filter);
            consider(mk(codec_id::ZSTD, z_tail, zstd_compress(&input, &dict, level, long)?));
        }
        if let (true, Some((order, mem))) = (p.try_all && kind != BlockKind::Exec, p.ppmd) {
            consider(mk(codec_id::PPMD, &[], ppmd_compress(&input, order, mem)?));
        }
    }
    let best = best.context("no codec configured")?;
    // Not worth a decode pass for under 0.5% savings.
    if best.data.len() as u64 * 1000 >= raw.len() as u64 * 995 {
        return Ok(store());
    }
    Ok(best)
}

/// `tail` is the last `prefix` raw bytes of chain history (empty if none).
pub fn decode(codec: u8, filter: u8, data: Vec<u8>, raw_len: usize, tail: &[u8]) -> Result<Vec<u8>> {
    let dict = dictionary(tail, filter);
    let mut out = match codec {
        codec_id::STORE => data,
        codec_id::ZSTD => {
            let mut d = zstd::zstd_safe::DCtx::create();
            let z = |c| anyhow!("zstd: {}", zstd::zstd_safe::get_error_name(c));
            d.set_parameter(zstd::zstd_safe::DParameter::WindowLogMax(ZSTD_MAX_WINDOW_LOG)).map_err(z)?;
            if !dict.is_empty() {
                d.ref_prefix(&dict).map_err(z)?;
            }
            let mut out = Vec::with_capacity(raw_len);
            d.decompress(&mut out, &data).map_err(z)?;
            out
        }
        codec_id::LZMA2 => lzma2_decompress(&data, &dict, raw_len)?,
        codec_id::PPMD => {
            if data.len() < 5 {
                bail!("ppmd header missing");
            }
            let order = data[0] as u32;
            let mem = u32::from_le_bytes(data[1..5].try_into().unwrap());
            if mem > 2048 << 20 {
                bail!("ppmd model size out of range");
            }
            let mut d = Ppmd8Decoder::new(&data[5..], order, mem, RestoreMethod::CutOff).map_err(|e| anyhow!("ppmd: {e:?}"))?;
            let mut out = vec![0u8; raw_len];
            d.read_exact(&mut out)?;
            out
        }
        c => bail!("unknown codec {c}"),
    };
    if out.len() != raw_len {
        bail!("block decoded to {} bytes, expected {raw_len}", out.len());
    }
    match filter {
        filter_id::NONE => {}
        filter_id::E8E9 => filter::e8e9_decode(&mut out),
        f => bail!("unknown filter {f}"),
    }
    Ok(out)
}

/// zstd -1 over four 64 KiB samples; under 3% savings means already compressed.
fn looks_compressible(raw: &[u8]) -> bool {
    const SAMPLE: usize = 64 << 10;
    let (mut total, mut packed) = (0, 0);
    for i in 0..4 {
        let start = (raw.len() - SAMPLE) * i / 3;
        let s = &raw[start..start + SAMPLE];
        total += SAMPLE;
        packed += zstd::bulk::compress(s, 1).map(|v| v.len()).unwrap_or(SAMPLE);
    }
    packed * 100 < total * 97
}

fn zstd_compress(input: &[u8], dict: &[u8], level: i32, long: bool) -> Result<Vec<u8>> {
    use zstd::zstd_safe::{self, CParameter};
    let z = |c| anyhow!("zstd: {}", zstd_safe::get_error_name(c));
    let mut c = zstd_safe::CCtx::create();
    c.set_parameter(CParameter::CompressionLevel(level)).map_err(z)?;
    c.set_parameter(CParameter::WindowLog(window_log(dict.len() + input.len()))).map_err(z)?;
    c.set_parameter(CParameter::EnableLongDistanceMatching(long)).map_err(z)?;
    c.set_parameter(CParameter::ChecksumFlag(false)).map_err(z)?;
    if !dict.is_empty() {
        c.ref_prefix(dict).map_err(z)?;
    }
    let mut out = Vec::with_capacity(zstd_safe::compress_bound(input.len()));
    c.compress2(&mut out, input).map_err(z)?;
    Ok(out)
}

/// Raw LZMA2 stream (the .xz container cannot carry a preset dictionary),
/// prefixed with the dictionary size the decoder needs.
fn lzma2_compress(input: &[u8], dict: &[u8], kind: BlockKind, preset: u32, extreme: bool) -> Result<Vec<u8>> {
    use lzma_sys::*;
    unsafe {
        let mut o: lzma_options_lzma = std::mem::zeroed();
        if lzma_lzma_preset(&mut o, preset | if extreme { LZMA_PRESET_EXTREME } else { 0 }) != 0 {
            bail!("lzma2 preset {preset} rejected");
        }
        o.dict_size = lzma_dict(dict.len() + input.len());
        if kind == BlockKind::Text {
            // Byte-oriented text: more literal context, no position alignment.
            o.lc = 4;
            o.lp = 0;
            o.pb = 0;
        }
        if !dict.is_empty() {
            o.preset_dict = dict.as_ptr();
            o.preset_dict_size = dict.len() as u32;
        }
        let mut out = Vec::with_capacity(input.len() / 3 + 1024);
        out.extend_from_slice(&o.dict_size.to_le_bytes());
        let filters = [
            lzma_filter { id: LZMA_FILTER_LZMA2, options: &mut o as *mut _ as *mut _ },
            lzma_filter { id: LZMA_VLI_UNKNOWN, options: std::ptr::null_mut() },
        ];
        let mut s: lzma_stream = std::mem::zeroed();
        if lzma_raw_encoder(&mut s, filters.as_ptr()) != LZMA_OK {
            bail!("lzma2 encoder init failed");
        }
        let r = run_lzma(&mut s, input, &mut out, LZMA_FINISH, usize::MAX);
        lzma_end(&mut s);
        r?;
        Ok(out)
    }
}

fn lzma2_decompress(data: &[u8], dict: &[u8], raw_len: usize) -> Result<Vec<u8>> {
    use lzma_sys::*;
    if data.len() < 4 {
        bail!("lzma2 header missing");
    }
    let dict_size = u32::from_le_bytes(data[..4].try_into().unwrap());
    if dict_size > 1536 << 20 || (dict.len() as u64) > dict_size as u64 * 4 {
        bail!("lzma2 dictionary size out of range");
    }
    unsafe {
        let mut o: lzma_options_lzma = std::mem::zeroed();
        lzma_lzma_preset(&mut o, 6);
        o.dict_size = dict_size.max(4096);
        if !dict.is_empty() {
            o.preset_dict = dict.as_ptr();
            o.preset_dict_size = dict.len() as u32;
        }
        let filters = [
            lzma_filter { id: LZMA_FILTER_LZMA2, options: &mut o as *mut _ as *mut _ },
            lzma_filter { id: LZMA_VLI_UNKNOWN, options: std::ptr::null_mut() },
        ];
        let mut s: lzma_stream = std::mem::zeroed();
        if lzma_raw_decoder(&mut s, filters.as_ptr()) != LZMA_OK {
            bail!("lzma2 decoder init failed");
        }
        let mut out = Vec::with_capacity(raw_len);
        let r = run_lzma(&mut s, &data[4..], &mut out, LZMA_RUN, raw_len);
        lzma_end(&mut s);
        r?;
        Ok(out)
    }
}

/// Drives `lzma_code` until the stream ends, appending to `out` (never past `limit`).
unsafe fn run_lzma(s: &mut lzma_sys::lzma_stream, input: &[u8], out: &mut Vec<u8>, action: lzma_sys::lzma_action, limit: usize) -> Result<()> {
    use lzma_sys::*;
    s.next_in = input.as_ptr();
    s.avail_in = input.len();
    loop {
        if out.capacity() - out.len() < 64 << 10 {
            out.reserve(out.capacity().max(1 << 20));
        }
        let room = (out.capacity() - out.len()).min(limit.saturating_sub(out.len()).max(1));
        s.next_out = out.as_mut_ptr().add(out.len());
        s.avail_out = room;
        let ret = lzma_code(s, action);
        out.set_len(out.len() + (room - s.avail_out));
        match ret {
            LZMA_STREAM_END => return Ok(()),
            LZMA_OK if out.len() > limit => bail!("lzma2 output exceeds expected size"),
            LZMA_OK if s.avail_in == 0 && s.avail_out != 0 && action == LZMA_RUN => bail!("lzma2 stream truncated"),
            LZMA_OK => {}
            e => bail!("lzma2 error {e}"),
        }
    }
}

/// PPMd var.I with cut-off model restore. Stream starts with order(1) + mem(4).
fn ppmd_compress(input: &[u8], order: u32, mem: u32) -> Result<Vec<u8>> {
    // A model much larger than the input only costs allocation time.
    let mem = mem.min((input.len() as u64 * 12).clamp(1 << 20, u32::MAX as u64) as u32);
    let mut out = Vec::with_capacity(input.len() / 3 + 5);
    out.push(order as u8);
    out.extend_from_slice(&mem.to_le_bytes());
    let mut enc = Ppmd8Encoder::new(out, order, mem, RestoreMethod::CutOff).map_err(|e| anyhow!("ppmd: {e:?}"))?;
    enc.write_all(input)?;
    Ok(enc.finish(false)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(seed: u32) -> Vec<u8> {
        let mut v = Vec::new();
        let mut x = seed;
        for i in 0..300_000u32 {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            if i % 3 == 0 {
                v.extend_from_slice(b"function foo(bar) { return bar + 1; }\n");
            }
            v.push((x >> 16) as u8 & 0x3f);
            v.push(0xE8);
        }
        v
    }

    #[test]
    fn every_level_and_kind_round_trips_with_and_without_prefix() {
        let prev = sample(3);
        let raw = sample(7);
        for level in 0..=9 {
            let p = Profile::for_level(level, Some(4));
            for kind in [BlockKind::Text, BlockKind::Exec, BlockKind::Other] {
                for prev in [None, Some(&prev[..])] {
                    let e = encode(&raw, prev, kind, &p).unwrap();
                    let tail = prev.map_or(&[][..], |pv| &pv[pv.len() - e.prefix as usize..]);
                    let back = decode(e.codec, e.filter, e.data, raw.len(), tail).unwrap();
                    assert!(back == raw, "level {level} {kind:?} codec {} prefix {}", e.codec, e.prefix);
                }
            }
        }
    }

    #[test]
    fn prefix_helps_on_repeated_data() {
        let prev = sample(11);
        let p = Profile::for_level(5, Some(4));
        let alone = encode(&prev, None, BlockKind::Other, &p).unwrap();
        let primed = encode(&prev, Some(&prev), BlockKind::Other, &p).unwrap();
        assert!(primed.data.len() * 10 < alone.data.len(), "{} vs {}", primed.data.len(), alone.data.len());
    }
}
