//! ZIP output, for Bandizip's quick "파일명.zip으로 압축하기": deflate (zlib-rs),
//! UTF-8 names, extended timestamps, Zip64 when any size or offset needs it.
//!
//! Small files are read and deflated in parallel batches, then written in
//! order; files of `BIG` bytes or more stream through one deflater and get
//! their local header patched afterwards. Already-compressed types are stored.
//!
//! Levels: 0 store, 1-9 zlib-rs, 10 libdeflate-12, 11 Zopfli (see `deflate`).

use crate::create::{scan, Group, Source};
use crate::format::Kind;
use crate::progress::Progress;
use crate::util::{civil, local_offset_secs};
use anyhow::{bail, Context, Result};
use flate2::write::DeflateEncoder;
use flate2::Compression;
use rayon::prelude::*;
use std::fs::{self, File};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

const BIG: u64 = 32 << 20;
const U32_MAX: u64 = 0xFFFF_FFFF;
const UTF8: u16 = 1 << 11;
const STORE: u16 = 0;
const DEFLATE: u16 = 8;

pub struct ZipReport {
    pub files: usize,
    pub dirs: usize,
    pub raw: u64,
    pub size: u64,
    pub secs: f64,
    pub warnings: Vec<String>,
}

struct Central {
    name: String,
    method: u16,
    dos: (u16, u16),
    mtime: i64,
    crc: u32,
    comp: u64,
    size: u64,
    offset: u64,
    attrs: u32,
}

/// Counts bytes so offsets are known without asking the OS.
struct Out {
    w: BufWriter<File>,
    pos: u64,
}

impl Write for Out {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        let n = self.w.write(b)?;
        self.pos += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.w.flush()
    }
}

fn dos_time(mtime: i64, offset: i64) -> (u16, u16) {
    let (y, m, d, h, mi, s) = civil(mtime + offset);
    if y < 1980 {
        return (0, (1 << 5) | 1);
    }
    let y = y.min(2107);
    (((h << 11) | (mi << 5) | (s / 2)) as u16, (((y - 1980) << 9) | (m << 5) | d) as u16)
}

/// Extended timestamp extra field (0x5455): modification time, UTC.
fn ut_extra(mtime: i64) -> Vec<u8> {
    let t = mtime.clamp(0, i32::MAX as i64) as u32;
    let mut e = vec![0x55, 0x54, 5, 0, 1];
    e.extend_from_slice(&t.to_le_bytes());
    e
}

fn local_header(o: &mut Out, c: &Central, zip64: bool) -> Result<()> {
    let mut extra = Vec::new();
    if zip64 {
        extra.extend_from_slice(&[1, 0, 16, 0]);
        extra.extend_from_slice(&c.size.to_le_bytes());
        extra.extend_from_slice(&c.comp.to_le_bytes());
    }
    extra.extend(ut_extra(c.mtime));
    let mut h = Vec::with_capacity(30 + c.name.len() + extra.len());
    h.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
    h.extend_from_slice(&(if zip64 { 45u16 } else { 20 }).to_le_bytes());
    h.extend_from_slice(&UTF8.to_le_bytes());
    h.extend_from_slice(&c.method.to_le_bytes());
    h.extend_from_slice(&c.dos.0.to_le_bytes());
    h.extend_from_slice(&c.dos.1.to_le_bytes());
    h.extend_from_slice(&c.crc.to_le_bytes());
    let (cs, us) = if zip64 { (U32_MAX as u32, U32_MAX as u32) } else { (c.comp as u32, c.size as u32) };
    h.extend_from_slice(&cs.to_le_bytes());
    h.extend_from_slice(&us.to_le_bytes());
    h.extend_from_slice(&(c.name.len() as u16).to_le_bytes());
    h.extend_from_slice(&(extra.len() as u16).to_le_bytes());
    h.extend_from_slice(c.name.as_bytes());
    h.extend_from_slice(&extra);
    o.write_all(&h)?;
    Ok(())
}

fn central_record(c: &Central) -> Vec<u8> {
    let big = [c.size >= U32_MAX, c.comp >= U32_MAX, c.offset >= U32_MAX];
    let mut z64 = Vec::new();
    for (v, b) in [c.size, c.comp, c.offset].into_iter().zip(big) {
        if b {
            z64.extend_from_slice(&v.to_le_bytes());
        }
    }
    let mut extra = Vec::new();
    if !z64.is_empty() {
        extra.extend_from_slice(&[1, 0]);
        extra.extend_from_slice(&(z64.len() as u16).to_le_bytes());
        extra.extend(z64);
    }
    extra.extend(ut_extra(c.mtime));
    let pick = |v: u64, b: bool| if b { U32_MAX as u32 } else { v as u32 };
    let mut h = Vec::with_capacity(46 + c.name.len() + extra.len());
    h.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
    h.extend_from_slice(&63u16.to_le_bytes()); // made by: spec 6.3, MS-DOS attributes
    h.extend_from_slice(&(if big.iter().any(|b| *b) { 45u16 } else { 20 }).to_le_bytes());
    h.extend_from_slice(&UTF8.to_le_bytes());
    h.extend_from_slice(&c.method.to_le_bytes());
    h.extend_from_slice(&c.dos.0.to_le_bytes());
    h.extend_from_slice(&c.dos.1.to_le_bytes());
    h.extend_from_slice(&c.crc.to_le_bytes());
    h.extend_from_slice(&pick(c.comp, big[1]).to_le_bytes());
    h.extend_from_slice(&pick(c.size, big[0]).to_le_bytes());
    h.extend_from_slice(&(c.name.len() as u16).to_le_bytes());
    h.extend_from_slice(&(extra.len() as u16).to_le_bytes());
    h.extend_from_slice(&0u16.to_le_bytes()); // comment
    h.extend_from_slice(&0u16.to_le_bytes()); // disk
    h.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
    h.extend_from_slice(&c.attrs.to_le_bytes());
    h.extend_from_slice(&pick(c.offset, big[2]).to_le_bytes());
    h.extend_from_slice(c.name.as_bytes());
    h.extend_from_slice(&extra);
    h
}

struct Packed {
    crc: u32,
    method: u16,
    size: u64,
    data: Vec<u8>,
}

/// Reads and compresses one small file in memory.
fn pack(s: &Source, level: u32, progress: Option<&Progress>) -> Result<Packed> {
    let raw = fs::read(&s.abs).with_context(|| s.abs.display().to_string())?;
    if let Some(p) = progress {
        p.check()?;
        p.add(raw.len() as u64);
    }
    let crc = crc32fast::hash(&raw);
    let size = raw.len() as u64;
    if s.group != Group::Packed && raw.len() > 64 && level > 0 {
        let data = deflate(&raw, level)?;
        if data.len() < raw.len() {
            return Ok(Packed { crc, method: DEFLATE, size, data });
        }
    }
    Ok(Packed { crc, method: STORE, size, data: raw })
}

/// Raw deflate of a whole buffer. 1-9: zlib-rs. 10: libdeflate level 12
/// (near-optimal parsing, still fast). 11: Zopfli, Bandizip's `-zopfli`
/// (smallest, very slow). All three are plain deflate any unzipper reads.
fn deflate(raw: &[u8], level: u32) -> Result<Vec<u8>> {
    match level {
        ..=9 => {
            let mut enc = DeflateEncoder::new(Vec::with_capacity(raw.len() / 2), Compression::new(level));
            enc.write_all(raw)?;
            Ok(enc.finish()?)
        }
        10 => {
            let lvl = libdeflater::CompressionLvl::new(12).map_err(|e| anyhow::anyhow!("{e:?}"))?;
            let mut c = libdeflater::Compressor::new(lvl);
            let mut out = vec![0u8; c.deflate_compress_bound(raw.len())];
            let n = c.deflate_compress(raw, &mut out).map_err(|e| anyhow::anyhow!("libdeflate: {e:?}"))?;
            out.truncate(n);
            Ok(out)
        }
        _ => {
            // Zopfli's own advice: 15 iterations for small inputs, fewer past a few MB.
            let iterations = if raw.len() > (1 << 20) { 5 } else { 15 };
            let opts = zopfli::Options { iteration_count: std::num::NonZeroU64::new(iterations).unwrap(), ..Default::default() };
            let mut out = Vec::with_capacity(raw.len() / 2);
            zopfli::compress(opts, zopfli::Format::Deflate, raw, &mut out)?;
            Ok(out)
        }
    }
}

fn attrs(s: &Source) -> u32 {
    (if s.kind == Kind::Dir { 0x10 } else { 0 }) | (if s.readonly { 0x01 } else { 0 })
}

/// Streams a large file: header first, data through the deflater, then the
/// header's CRC and sizes patched in place.
fn stream(o: &mut Out, s: &Source, level: u32, offset_tz: i64, progress: Option<&Progress>) -> Result<Central> {
    let zip64 = s.size >= U32_MAX - (1 << 20);
    let method = if s.group == Group::Packed { STORE } else { DEFLATE };
    let mut c = Central {
        name: s.name.clone(),
        method,
        dos: dos_time(s.mtime, offset_tz),
        mtime: s.mtime,
        crc: 0,
        comp: 0,
        size: 0,
        offset: o.pos,
        attrs: attrs(s),
    };
    local_header(o, &c, zip64)?;
    let data_start = o.pos;
    let mut f = File::open(&s.abs).with_context(|| s.abs.display().to_string())?;
    let mut hasher = crc32fast::Hasher::new();
    if method == DEFLATE {
        // The 10/11 encoders need the whole file in memory; big files stream through zlib 9 instead.
        let mut enc = DeflateEncoder::new(&mut *o, Compression::new(level.min(9)));
        c.size = copy(&mut f, &mut enc, &mut hasher, progress)?;
        enc.finish()?;
    } else {
        c.size = copy(&mut f, &mut *o, &mut hasher, progress)?;
    }
    c.crc = hasher.finalize();
    c.comp = o.pos - data_start;
    if !zip64 && (c.size >= U32_MAX || c.comp >= U32_MAX) {
        bail!("{} grew past 4 GiB while being read", s.abs.display());
    }
    // Patch CRC and sizes in the local header (offset 14), plus the Zip64 extra when present.
    o.w.flush()?;
    let file = o.w.get_mut();
    file.seek(SeekFrom::Start(c.offset + 14))?;
    file.write_all(&c.crc.to_le_bytes())?;
    if zip64 {
        file.seek(SeekFrom::Start(c.offset + 30 + c.name.len() as u64 + 4))?;
        file.write_all(&c.size.to_le_bytes())?;
        file.write_all(&c.comp.to_le_bytes())?;
    } else {
        file.write_all(&(c.comp as u32).to_le_bytes())?;
        file.write_all(&(c.size as u32).to_le_bytes())?;
    }
    file.seek(SeekFrom::Start(o.pos))?;
    Ok(c)
}

fn copy(f: &mut File, sink: &mut dyn Write, hasher: &mut crc32fast::Hasher, progress: Option<&Progress>) -> Result<u64> {
    let mut buf = vec![0u8; 1 << 20];
    let mut total = 0;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            return Ok(total);
        }
        if let Some(p) = progress {
            p.check()?;
            p.add(n as u64);
        }
        hasher.update(&buf[..n]);
        sink.write_all(&buf[..n])?;
        total += n as u64;
    }
}

pub fn create_zip(archive: &Path, inputs: &[PathBuf], level: u32, progress: Option<&Progress>) -> Result<ZipReport> {
    let started = Instant::now();
    let part = PathBuf::from(format!("{}.part", archive.display()));
    let parent = archive.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let parent = fs::canonicalize(parent).with_context(|| format!("output folder {} does not exist", parent.display()))?;
    let skip = [parent.join(archive.file_name().context("bad archive name")?), parent.join(part.file_name().unwrap())];
    let mut warnings = Vec::new();
    let sources = scan(inputs, &skip, &mut warnings)?;
    if let Some(p) = progress {
        p.set_total(sources.iter().map(|s| s.size).sum());
    }

    let mut o = Out { w: BufWriter::with_capacity(4 << 20, File::create(&part).with_context(|| format!("cannot create {}", part.display()))?), pos: 0 };
    let result = (|| -> Result<(usize, usize, u64)> {
        let tz = local_offset_secs();
        let batch_bytes = (rayon::current_num_threads() as u64 * BIG).max(2 * BIG);
        let mut central = Vec::with_capacity(sources.len());
        let (mut files, mut dirs, mut raw) = (0, 0, 0);
        let mut i = 0;
        while i < sources.len() {
            if let Some(p) = progress {
                p.check()?;
                p.set_current(&sources[i].name);
            }
            if sources[i].kind == Kind::File && sources[i].size >= BIG {
                match stream(&mut o, &sources[i], level, tz, progress) {
                    Ok(c) => {
                        raw += c.size;
                        files += 1;
                        central.push(c);
                    }
                    Err(e) => return Err(e.context("a large file could not be read; the archive is incomplete")),
                }
                i += 1;
                continue;
            }
            // A batch of small files and folders, compressed in parallel.
            let mut j = i;
            let mut bytes = 0;
            while j < sources.len() && !(sources[j].kind == Kind::File && sources[j].size >= BIG) && bytes <= batch_bytes {
                bytes += sources[j].size;
                j += 1;
            }
            let packed: Vec<Option<Result<Packed>>> =
                sources[i..j].par_iter().map(|s| (s.kind == Kind::File).then(|| pack(s, level, progress))).collect();
            for (s, p) in sources[i..j].iter().zip(packed) {
                let dos = dos_time(s.mtime, tz);
                let c = match p {
                    None => {
                        dirs += 1;
                        let c = Central { name: format!("{}/", s.name), method: STORE, dos, mtime: s.mtime, crc: 0, comp: 0, size: 0, offset: o.pos, attrs: attrs(s) };
                        local_header(&mut o, &c, false)?;
                        c
                    }
                    Some(Err(e)) => {
                        if crate::progress::is_cancelled(&e) {
                            return Err(e);
                        }
                        warnings.push(format!("skipped {}: {e:#}", s.abs.display()));
                        continue;
                    }
                    Some(Ok(p)) => {
                        files += 1;
                        raw += p.size;
                        let c = Central {
                            name: s.name.clone(),
                            method: p.method,
                            dos,
                            mtime: s.mtime,
                            crc: p.crc,
                            comp: p.data.len() as u64,
                            size: p.size,
                            offset: o.pos,
                            attrs: attrs(s),
                        };
                        local_header(&mut o, &c, false)?;
                        o.write_all(&p.data)?;
                        c
                    }
                };
                central.push(c);
            }
            i = j;
        }

        let cd_start = o.pos;
        for c in &central {
            let r = central_record(c);
            o.write_all(&r)?;
        }
        let cd_size = o.pos - cd_start;
        let n = central.len() as u64;
        if n >= 0xFFFF || cd_start >= U32_MAX || cd_size >= U32_MAX {
            let eocd64 = o.pos;
            let mut r = Vec::with_capacity(56 + 20);
            r.extend_from_slice(&0x0606_4b50u32.to_le_bytes());
            r.extend_from_slice(&44u64.to_le_bytes());
            r.extend_from_slice(&63u16.to_le_bytes());
            r.extend_from_slice(&45u16.to_le_bytes());
            r.extend_from_slice(&0u32.to_le_bytes());
            r.extend_from_slice(&0u32.to_le_bytes());
            r.extend_from_slice(&n.to_le_bytes());
            r.extend_from_slice(&n.to_le_bytes());
            r.extend_from_slice(&cd_size.to_le_bytes());
            r.extend_from_slice(&cd_start.to_le_bytes());
            r.extend_from_slice(&0x0706_4b50u32.to_le_bytes());
            r.extend_from_slice(&0u32.to_le_bytes());
            r.extend_from_slice(&eocd64.to_le_bytes());
            r.extend_from_slice(&1u32.to_le_bytes());
            o.write_all(&r)?;
        }
        let mut r = Vec::with_capacity(22);
        r.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        r.extend_from_slice(&0u16.to_le_bytes());
        r.extend_from_slice(&0u16.to_le_bytes());
        r.extend_from_slice(&(n.min(0xFFFF) as u16).to_le_bytes());
        r.extend_from_slice(&(n.min(0xFFFF) as u16).to_le_bytes());
        r.extend_from_slice(&(cd_size.min(U32_MAX) as u32).to_le_bytes());
        r.extend_from_slice(&(cd_start.min(U32_MAX) as u32).to_le_bytes());
        r.extend_from_slice(&0u16.to_le_bytes());
        o.write_all(&r)?;
        o.w.flush()?;
        o.w.get_ref().sync_all()?;
        Ok((files, dirs, raw))
    })();
    let size = o.pos;
    drop(o);
    let (files, dirs, raw) = match result {
        Ok(r) => r,
        Err(e) => {
            let _ = fs::remove_file(&part);
            return Err(e);
        }
    };
    fs::rename(&part, archive).with_context(|| format!("cannot move {} into place", part.display()))?;
    Ok(ZipReport { files, dirs, raw, size, secs: started.elapsed().as_secs_f64(), warnings })
}
