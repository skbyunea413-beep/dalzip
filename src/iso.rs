//! Reading disc images (.iso): UDF, ISO 9660, Joliet and Rock Ridge.
//!
//! A single image can carry several of these describing the same files. UDF
//! wins because only it holds files over 4 GiB in one piece and full Unicode
//! names (Windows install media); then Rock Ridge, whose names are the real
//! ones on Linux media; then Joliet; then plain ISO 9660.
//!
//! Disc images carry no checksums, so unlike .dz and ZIP nothing here can prove
//! the data is intact: a test only shows every byte can be read.

use crate::extract::{destination_for, safe_path, ExtractReport, Target};
use crate::progress::Progress;
use crate::util::{local_offset_secs, unix_from_civil};
use anyhow::{bail, Context, Result};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

const SECTOR: u64 = 2048;
/// Directories deeper than this, or larger than `MAX_DIR`, mean a damaged or hostile image.
const MAX_DEPTH: usize = 128;
const MAX_DIR: u64 = 256 << 20;
const MAX_ENTRIES: usize = 4_000_000;

/// Where one run of a file's bytes lives; `None` reads as zeros (a UDF hole).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Extent {
    pub offset: Option<u64>,
    pub len: u64,
}

pub struct IsoEntry {
    /// '/'-separated, no leading or trailing '/'.
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: i64,
    pub extents: Vec<Extent>,
}

pub struct IsoListing {
    pub path: PathBuf,
    pub entries: Vec<IsoEntry>,
    pub size: u64,
    /// Which file system was read: "UDF", "Rock Ridge", "Joliet" or "ISO 9660".
    pub filesystem: &'static str,
}

pub fn is_iso(p: &Path) -> bool {
    p.extension().is_some_and(|e| e.eq_ignore_ascii_case("iso"))
}

fn u16le(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u32le(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn u64le(b: &[u8], at: usize) -> u64 {
    u32le(b, at) as u64 | (u32le(b, at + 4) as u64) << 32
}

/// One path component made safe to create on Windows; `None` when nothing usable is left.
fn clean_name(name: &str) -> Option<String> {
    let s: String = name.chars().map(|c| if c < ' ' || "\\/:*?\"<>|".contains(c) { '_' } else { c }).collect();
    // Windows drops trailing dots and spaces itself, which would turn ".. " into "..".
    let s = s.trim_end_matches(['.', ' ']);
    (!s.is_empty()).then(|| s.to_string())
}

/// Reads from the image at absolute offsets.
struct Image<R> {
    r: R,
    size: u64,
}

impl<R: Read + Seek> Image<R> {
    fn read(&mut self, offset: u64, len: u64) -> Result<Vec<u8>> {
        if offset.checked_add(len).is_none_or(|e| e > self.size) {
            bail!("image is truncated or damaged (read past the end at {offset})");
        }
        let mut b = vec![0u8; len as usize];
        self.r.seek(SeekFrom::Start(offset))?;
        self.r.read_exact(&mut b)?;
        Ok(b)
    }

    fn read_extents(&mut self, extents: &[Extent]) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        for e in extents {
            match e.offset {
                Some(o) => out.extend(self.read(o, e.len)?),
                None => out.resize(out.len() + e.len as usize, 0),
            }
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------- ISO 9660

#[derive(Clone, Copy, PartialEq)]
enum Names {
    Plain,
    Joliet,
}

struct Tree {
    entries: Vec<IsoEntry>,
    rock_ridge: bool,
}

/// What a directory record's Rock Ridge fields say about it.
#[derive(Default)]
struct RockRidge {
    name: Option<Vec<u8>>,
    symlink: bool,
    /// Relocated directory: the record stands in for the directory at this block.
    child_link: Option<u32>,
    /// A directory moved here only to stay within ISO 9660's depth limit.
    relocated: bool,
}

fn rock_ridge<R: Read + Seek>(img: &mut Image<R>, block: u64, mut area: Vec<u8>) -> RockRidge {
    let mut rr = RockRidge::default();
    let mut hops = 0;
    let mut at = 0;
    while at + 4 <= area.len() {
        let (sig, len) = (&area[at..at + 2], area[at + 2] as usize);
        if len < 4 || at + len > area.len() {
            break;
        }
        match sig {
            b"NM" if len >= 5 => rr.name.get_or_insert_with(Vec::new).extend_from_slice(&area[at + 5..at + len]),
            b"SL" => rr.symlink = true,
            b"CL" if len >= 8 => rr.child_link = Some(u32le(&area, at + 4)),
            b"RE" => rr.relocated = true,
            b"ST" => break,
            // The fields go on in a continuation area elsewhere on the disc.
            b"CE" if len >= 28 && hops < 8 => {
                let (b, off, n) = (u32le(&area, at + 4) as u64, u32le(&area, at + 12) as u64, u32le(&area, at + 20) as u64);
                hops += 1;
                match img.read(b * block + off, n.min(SECTOR)) {
                    Ok(next) => {
                        area = next;
                        at = 0;
                        continue;
                    }
                    Err(_) => break,
                }
            }
            _ => {}
        }
        at += len;
    }
    rr
}

fn iso_time(d: &[u8]) -> i64 {
    if d[1] == 0 || d[2] == 0 {
        return 0;
    }
    let t = unix_from_civil(1900 + d[0] as i64, d[1] as i64, d[2] as i64, d[3] as i64, d[4] as i64, d[5] as i64);
    t - d[6] as i8 as i64 * 15 * 60
}

fn iso_name(raw: &[u8], names: Names) -> String {
    let s = match names {
        Names::Plain => String::from_utf8_lossy(raw).into_owned(),
        Names::Joliet => {
            let u: Vec<u16> = raw.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
            String::from_utf16_lossy(&u)
        }
    };
    // "NAME.TXT;1": the version number is not part of the name.
    match s.rfind(';') {
        Some(i) if s[i + 1..].chars().all(|c| c.is_ascii_digit()) => s[..i].to_string(),
        _ => s,
    }
}

struct IsoWalk<'a, R> {
    img: &'a mut Image<R>,
    block: u64,
    names: Names,
    seen: HashSet<u64>,
    out: Tree,
}

impl<R: Read + Seek> IsoWalk<'_, R> {
    fn dir(&mut self, lba: u64, len: u64, prefix: &str, depth: usize) -> Result<()> {
        if depth > MAX_DEPTH || len > MAX_DIR || !self.seen.insert(lba) {
            return Ok(());
        }
        let data = self.img.read(lba * self.block, len)?;
        // A file in several extents is several records in a row, all but the last flagged.
        let mut pending: Option<(String, i64, Vec<Extent>, u64)> = None;
        let mut at = 0;
        while at < data.len() {
            let rec = data[at] as usize;
            if rec == 0 {
                // Records never cross a sector; the rest of this one is padding.
                at = (at / self.block as usize + 1) * self.block as usize;
                continue;
            }
            if rec < 34 || at + rec > data.len() {
                break;
            }
            let r = &data[at..at + rec];
            at += rec;
            let name_len = r[32] as usize;
            if 33 + name_len > rec {
                continue;
            }
            let raw = &r[33..33 + name_len];
            let is_self = raw == [0] || raw == [1];
            let su_at = 33 + name_len + (1 - name_len % 2);
            let rr = if self.names == Names::Plain && su_at < rec {
                let area = r[su_at..].to_vec();
                rock_ridge(self.img, self.block, area)
            } else {
                RockRidge::default()
            };
            if is_self {
                continue;
            }
            self.out.rock_ridge |= rr.name.is_some();
            let flags = r[25];
            let (ex_lba, size) = (u32le(r, 2) as u64, u32le(r, 10) as u64);
            if rr.symlink || rr.relocated {
                continue;
            }
            let mut name_raw = rr.name.map(|n| String::from_utf8_lossy(&n).into_owned()).unwrap_or_else(|| iso_name(raw, self.names));
            if name_raw.is_empty() {
                name_raw = format!("_{ex_lba}");
            }
            let Some(name) = clean_name(&name_raw) else { continue };
            let path = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
            let mtime = iso_time(&r[18..25]);
            if self.out.entries.len() >= MAX_ENTRIES {
                bail!("too many entries in the image");
            }
            if let Some(target) = rr.child_link {
                // The directory itself lives elsewhere; its "." record says how long it is.
                let head = self.img.read(target as u64 * self.block, self.block)?;
                let dlen = u32le(&head, 10) as u64;
                self.out.entries.push(IsoEntry { path: path.clone(), is_dir: true, size: 0, mtime, extents: Vec::new() });
                self.dir(target as u64, dlen, &path, depth + 1)?;
            } else if flags & 0x02 != 0 {
                self.out.entries.push(IsoEntry { path: path.clone(), is_dir: true, size: 0, mtime, extents: Vec::new() });
                self.dir(ex_lba, size, &path, depth + 1)?;
            } else {
                if r[26] != 0 || r[27] != 0 {
                    bail!("interleaved files are not supported: {path}");
                }
                let ext = Extent { offset: Some(ex_lba * self.block), len: size };
                let (p, m, mut exts, mut total) = pending.take().unwrap_or((path, mtime, Vec::new(), 0));
                if size > 0 {
                    exts.push(ext);
                }
                total += size;
                if flags & 0x80 != 0 {
                    pending = Some((p, m, exts, total));
                } else {
                    self.out.entries.push(IsoEntry { path: p, is_dir: false, size: total, mtime: m, extents: exts });
                }
            }
        }
        // The chain's last record never came; keep what the image does describe.
        if let Some((path, mtime, extents, size)) = pending {
            self.out.entries.push(IsoEntry { path, is_dir: false, size, mtime, extents });
        }
        Ok(())
    }
}

fn read_iso9660<R: Read + Seek>(img: &mut Image<R>, vd: &[u8], names: Names) -> Result<Tree> {
    let block = (u16le(vd, 128) as u64).max(1);
    if !block.is_power_of_two() || block > 8192 {
        bail!("unusual logical block size {block}");
    }
    let root = &vd[156..190];
    let (lba, len) = (u32le(root, 2) as u64, u32le(root, 10) as u64);
    let mut w = IsoWalk { img, block, names, seen: HashSet::new(), out: Tree { entries: Vec::new(), rock_ridge: false } };
    w.dir(lba, len, "", 0)?;
    Ok(w.out)
}

// ---------------------------------------------------------------- UDF

/// A descriptor tag: right number, right place, right checksum.
fn tag_ok(b: &[u8], id: u16, location: u64) -> bool {
    if b.len() < 16 || u16le(b, 0) != id || u32le(b, 12) as u64 != location {
        return false;
    }
    let sum: u32 = b[..4].iter().chain(&b[5..16]).map(|&x| x as u32).sum();
    sum as u8 == b[4]
}

fn udf_string(raw: &[u8]) -> String {
    match raw.split_first() {
        Some((8, rest)) => rest.iter().map(|&c| c as char).collect(),
        Some((16, rest)) => {
            let u: Vec<u16> = rest.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
            String::from_utf16_lossy(&u)
        }
        _ => String::new(),
    }
}

fn udf_time(d: &[u8]) -> i64 {
    if d[4] == 0 || d[5] == 0 {
        return 0;
    }
    // Low 12 bits: minutes from UTC, signed; -2047 means not given.
    let raw = u16le(d, 0) & 0x0FFF;
    let tz = if raw & 0x800 != 0 { raw as i64 - 0x1000 } else { raw as i64 };
    let offset = if tz == -2047 { local_offset_secs() } else { tz * 60 };
    unix_from_civil(u16le(d, 2) as i64, d[4] as i64, d[5] as i64, d[6] as i64, d[7] as i64, d[8] as i64) - offset
}

enum PartMap {
    /// Blocks count from this absolute block on the disc.
    Physical(u64),
    /// Blocks count through the metadata file (UDF 2.5+, Blu-ray): runs of (block count, absolute block).
    Metadata(Vec<(u64, u64)>),
}

struct Udf<'a, R> {
    img: &'a mut Image<R>,
    block: u64,
    maps: Vec<PartMap>,
    seen: HashSet<u64>,
    entries: Vec<IsoEntry>,
}

/// What a File Entry says about a file or directory.
struct Node {
    file_type: u8,
    size: u64,
    mtime: i64,
    extents: Vec<Extent>,
}

impl<R: Read + Seek> Udf<'_, R> {
    /// Absolute block of block `lbn` of partition map `part`.
    fn abs(&self, part: u16, lbn: u64) -> Result<u64> {
        match self.maps.get(part as usize).context("UDF: unknown partition reference")? {
            PartMap::Physical(start) => Ok(start + lbn),
            PartMap::Metadata(runs) => {
                let mut left = lbn;
                for &(n, at) in runs {
                    if left < n {
                        return Ok(at + left);
                    }
                    left -= n;
                }
                bail!("UDF: block outside the metadata file")
            }
        }
    }

    /// Reads a run of allocation descriptors, following continuation blocks.
    fn allocations(&mut self, mut ads: Vec<u8>, long: bool, part: u16, size: u64) -> Result<Vec<Extent>> {
        let step = if long { 16 } else { 8 };
        let mut out = Vec::new();
        let (mut left, mut at, mut hops) = (size, 0, 0);
        while at + step <= ads.len() && left > 0 {
            let raw = u32le(&ads, at);
            let (kind, len) = (raw >> 30, (raw & 0x3FFF_FFFF) as u64);
            let lbn = u32le(&ads, at + 4) as u64;
            let p = if long { u16le(&ads, at + 8) } else { part };
            at += step;
            if len == 0 {
                break;
            }
            match kind {
                // The list goes on in another block.
                3 => {
                    hops += 1;
                    if hops > 4096 {
                        bail!("UDF: allocation descriptors loop");
                    }
                    let a = self.abs(p, lbn)?;
                    let b = self.img.read(a * self.block, len.min(self.block))?;
                    if b.len() < 24 || u16le(&b, 0) != 258 {
                        bail!("UDF: bad allocation extent");
                    }
                    let n = (u32le(&b, 20) as usize).min(b.len() - 24);
                    ads = b[24..24 + n].to_vec();
                    at = 0;
                }
                k => {
                    let take = len.min(left);
                    let offset = if k == 0 { Some(self.abs(p, lbn)? * self.block) } else { None };
                    out.push(Extent { offset, len: take });
                    left -= take;
                }
            }
        }
        if left > 0 {
            bail!("UDF: allocation descriptors stop {left} bytes short");
        }
        Ok(out)
    }

    /// Reads a File Entry or Extended File Entry at a long_ad.
    fn node(&mut self, part: u16, lbn: u64) -> Result<Node> {
        let abs = self.abs(part, lbn)?;
        let b = self.img.read(abs * self.block, self.block)?;
        let id = u16le(&b, 0);
        if (id != 261 && id != 266) || b.len() < 216 {
            bail!("UDF: no file entry at block {lbn}");
        }
        let file_type = b[27];
        let flags = u16le(&b, 34);
        let size = u64le(&b, 56);
        let (mtime_at, lea_at, base) = if id == 261 { (84, 168, 176) } else { (92, 208, 216) };
        let (lea, lad) = (u32le(&b, lea_at) as usize, u32le(&b, lea_at + 4) as usize);
        let start = base + lea;
        if start.checked_add(lad).is_none_or(|e| e > b.len()) {
            bail!("UDF: damaged file entry");
        }
        let extents = match flags & 7 {
            0 => self.allocations(b[start..start + lad].to_vec(), false, part, size)?,
            1 => self.allocations(b[start..start + lad].to_vec(), true, part, size)?,
            // Small files live inside the entry itself.
            3 => {
                if size > lad as u64 {
                    bail!("UDF: damaged inline file");
                }
                if size == 0 {
                    Vec::new()
                } else {
                    vec![Extent { offset: Some(abs * self.block + start as u64), len: size }]
                }
            }
            n => bail!("UDF: allocation type {n} is not supported"),
        };
        Ok(Node { file_type, size, mtime: udf_time(&b[mtime_at..mtime_at + 12]), extents })
    }

    fn dir(&mut self, node: &Node, prefix: &str, depth: usize) -> Result<()> {
        if depth > MAX_DEPTH || node.size > MAX_DIR {
            return Ok(());
        }
        let data = self.img.read_extents(&node.extents)?;
        let mut at = 0;
        while at + 38 <= data.len() {
            if u16le(&data, at) != 257 {
                break;
            }
            let (chars, name_len, iu) = (data[at + 18], data[at + 19] as usize, u16le(&data, at + 36) as usize);
            let total = (38 + iu + name_len + 3) & !3;
            if at + total > data.len() {
                break;
            }
            let (lbn, part) = (u32le(&data, at + 24) as u64, u16le(&data, at + 28));
            let raw = &data[at + 38 + iu..at + 38 + iu + name_len];
            let name = udf_string(raw);
            at += total;
            // Deleted entries and the link to the parent are not files.
            if chars & 0x0C != 0 {
                continue;
            }
            let Some(name) = clean_name(&name) else { continue };
            let path = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
            if self.entries.len() >= MAX_ENTRIES {
                bail!("too many entries in the image");
            }
            let n = self.node(part, lbn)?;
            match n.file_type {
                4 => {
                    let key = self.abs(part, lbn)?;
                    if self.seen.insert(key) {
                        self.entries.push(IsoEntry { path: path.clone(), is_dir: true, size: 0, mtime: n.mtime, extents: Vec::new() });
                        self.dir(&n, &path, depth + 1)?;
                    }
                }
                5 | 249 => self.entries.push(IsoEntry { path, is_dir: false, size: n.size, mtime: n.mtime, extents: n.extents }),
                // Symlinks and system streams, as ZIP symlinks are skipped.
                _ => {}
            }
        }
        Ok(())
    }
}

fn read_udf<R: Read + Seek>(img: &mut Image<R>) -> Result<Vec<IsoEntry>> {
    // Anchor at sector 256, or failing that at the end of the disc.
    let last = img.size / SECTOR;
    let mut vds = None;
    for loc in [256, last.saturating_sub(257), last.saturating_sub(1)] {
        if loc < 256 {
            continue;
        }
        if let Ok(b) = img.read(loc * SECTOR, SECTOR) {
            if tag_ok(&b, 2, loc) {
                vds = Some((u32le(&b, 16) as u64, u32le(&b, 20) as u64));
                break;
            }
        }
    }
    let (vds_len, vds_at) = vds.context("no UDF anchor")?;

    let mut partitions: Vec<(u16, u64)> = Vec::new();
    let mut lvd: Option<Vec<u8>> = None;
    for i in 0..(vds_len / SECTOR).min(64) {
        let loc = vds_at + i;
        let b = img.read(loc * SECTOR, SECTOR)?;
        if !tag_ok(&b, u16le(&b, 0), loc) {
            continue;
        }
        match u16le(&b, 0) {
            5 => {
                let n = u16le(&b, 22);
                if !partitions.iter().any(|p| p.0 == n) {
                    partitions.push((n, u32le(&b, 188) as u64));
                }
            }
            6 => lvd = Some(b),
            8 => break,
            _ => {}
        }
    }
    let lvd = lvd.context("UDF: no logical volume")?;
    let block = u32le(&lvd, 212) as u64;
    if !block.is_power_of_two() || !(512..=8192).contains(&block) {
        bail!("UDF: unusual block size {block}");
    }
    let start_of = |n: u16| partitions.iter().find(|p| p.0 == n).map(|p| p.1).context("UDF: partition map names a missing partition");

    let (table_len, count) = (u32le(&lvd, 264) as usize, u32le(&lvd, 268));
    let table_end = (440 + table_len).min(lvd.len());
    let mut maps = Vec::new();
    let mut metadata_files: Vec<(usize, u16, u64)> = Vec::new();
    let mut at = 440;
    for _ in 0..count.min(64) {
        if at + 2 > table_end {
            break;
        }
        let (kind, len) = (lvd[at], lvd[at + 1] as usize);
        if len < 2 || at + len > table_end {
            break;
        }
        match kind {
            1 if len >= 6 => maps.push(PartMap::Physical(start_of(u16le(&lvd, at + 4))?)),
            2 if len >= 44 => {
                let ident = &lvd[at + 5..at + 28];
                let number = u16le(&lvd, at + 38);
                if ident.starts_with(b"*UDF Virtual Partition") {
                    bail!("UDF: virtual partitions (packet-written discs) are not supported");
                } else if ident.starts_with(b"*UDF Metadata Partition") {
                    metadata_files.push((maps.len(), number, u32le(&lvd, at + 40) as u64));
                    maps.push(PartMap::Metadata(Vec::new()));
                } else {
                    // A sparable partition reads as a plain one on an image.
                    maps.push(PartMap::Physical(start_of(number)?));
                }
            }
            _ => bail!("UDF: unknown partition map type {kind}"),
        }
        at += len;
    }
    let mut u = Udf { img, block, maps, seen: HashSet::new(), entries: Vec::new() };

    // The metadata partition is a file on a physical partition; resolve it first.
    for (slot, number, file_lbn) in metadata_files {
        // Borrow this slot as the physical partition just long enough to read the file.
        u.maps[slot] = PartMap::Physical(start_of(number)?);
        let n = u.node(slot as u16, file_lbn)?;
        let mut runs = Vec::new();
        for e in n.extents {
            runs.push((e.len / block, e.offset.context("UDF: hole in the metadata file")? / block));
        }
        u.maps[slot] = PartMap::Metadata(runs);
    }

    // File Set Descriptor -> root directory.
    let (fsd_lbn, fsd_part) = (u32le(&lvd, 252) as u64, u16le(&lvd, 256));
    let fsd_abs = u.abs(fsd_part, fsd_lbn)?;
    let fsd = u.img.read(fsd_abs * block, block)?;
    if u16le(&fsd, 0) != 256 {
        bail!("UDF: no file set descriptor");
    }
    let (root_lbn, root_part) = (u32le(&fsd, 404) as u64, u16le(&fsd, 408));
    let root = u.node(root_part, root_lbn)?;
    if root.file_type != 4 {
        bail!("UDF: root is not a directory");
    }
    let key = u.abs(root_part, root_lbn)?;
    u.seen.insert(key);
    u.dir(&root, "", 0)?;
    Ok(u.entries)
}

// ---------------------------------------------------------------- listing

fn parse<R: Read + Seek>(r: R, size: u64) -> Result<(Vec<IsoEntry>, &'static str)> {
    let mut img = Image { r, size };
    // Each file system stands on its own: a damaged or unsupported one falls through to the next.
    let mut why = None;
    match read_udf(&mut img) {
        Ok(e) if !e.is_empty() => return Ok((e, "UDF")),
        Ok(_) => {}
        Err(e) => why = Some(e),
    }
    let (mut primary, mut joliet) = (None, None);
    for s in 16..80u64 {
        let Ok(vd) = img.read(s * SECTOR, SECTOR) else { break };
        if &vd[1..6] != b"CD001" {
            break;
        }
        match vd[0] {
            1 if primary.is_none() => primary = Some(vd),
            // Joliet marks itself with a UCS-2 escape sequence.
            2 if joliet.is_none() && vd[88] == 0x25 && vd[89] == 0x2F && matches!(vd[90], 0x40 | 0x43 | 0x45) => joliet = Some(vd),
            255 => break,
            _ => {}
        }
    }
    let plain = match &primary {
        Some(vd) => read_iso9660(&mut img, vd, Names::Plain).ok(),
        None => None,
    };
    if let Some(t) = &plain {
        if t.rock_ridge && !t.entries.is_empty() {
            return Ok((plain.unwrap().entries, "Rock Ridge"));
        }
    }
    if let Some(vd) = &joliet {
        if let Ok(t) = read_iso9660(&mut img, vd, Names::Joliet) {
            if !t.entries.is_empty() {
                return Ok((t.entries, "Joliet"));
            }
        }
    }
    if let Some(t) = plain {
        return Ok((t.entries, "ISO 9660"));
    }
    match why {
        Some(e) => Err(e.context("no readable file system in the image")),
        None => bail!("not a disc image: no UDF or ISO 9660 volume found"),
    }
}

pub fn list_iso(path: &Path) -> Result<IsoListing> {
    let f = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let size = f.metadata()?.len();
    let (entries, filesystem) = parse(f, size).with_context(|| format!("{} is not a readable disc image", path.display()))?;
    Ok(IsoListing { path: path.to_path_buf(), entries, size, filesystem })
}

impl IsoListing {
    /// One file's bytes (for previews).
    pub fn read_entry(&self, entry: usize) -> Result<Vec<u8>> {
        let e = self.entries.get(entry).context("no such entry")?;
        let mut img = Image { r: File::open(&self.path)?, size: self.size };
        img.read_extents(&e.extents)
    }

    pub fn roots(&self) -> usize {
        let mut r: Vec<&str> = self.entries.iter().map(|e| e.path.split('/').next().unwrap_or("")).collect();
        r.sort_unstable();
        r.dedup();
        r.len()
    }

    pub fn destination(&self, base: &Path, target: Target) -> PathBuf {
        destination_for(self.roots(), &self.path, base, target)
    }
}

// ---------------------------------------------------------------- extraction

/// Copies one file's extents to `w`, counting progress as it goes.
fn copy_extents(src: &mut File, size: u64, extents: &[Extent], w: &mut dyn Write, buf: &mut [u8], progress: Option<&Progress>) -> Result<()> {
    for e in extents {
        if let Some(o) = e.offset {
            if o.checked_add(e.len).is_none_or(|end| end > size) {
                bail!("image is truncated: data ends past the end of the file");
            }
            src.seek(SeekFrom::Start(o))?;
        }
        let mut left = e.len;
        while left > 0 {
            if let Some(p) = progress {
                p.check()?;
            }
            let n = left.min(buf.len() as u64) as usize;
            match e.offset {
                Some(_) => src.read_exact(&mut buf[..n])?,
                None => buf[..n].fill(0),
            }
            w.write_all(&buf[..n])?;
            if let Some(p) = progress {
                p.add(n as u64);
            }
            left -= n as u64;
        }
    }
    Ok(())
}

/// Extracts (or with `out == None`, only reads) every entry, in the order the data
/// sits on the disc so a hard disk is not sent back and forth.
pub fn extract_iso(listing: &IsoListing, out: Option<&Path>, overwrite: bool, progress: Option<&Progress>) -> Result<ExtractReport> {
    let started = Instant::now();
    let mut targets = Vec::with_capacity(listing.entries.len());
    for e in &listing.entries {
        let rel = safe_path(&e.path)?;
        targets.push(out.map(|o| o.join(rel)));
    }
    if let Some(root) = out {
        fs::create_dir_all(root)?;
        if !overwrite {
            let clash: Vec<_> = listing
                .entries
                .iter()
                .zip(&targets)
                .filter(|(e, t)| !e.is_dir && t.as_ref().unwrap().exists())
                .map(|(e, _)| e.path.as_str())
                .take(5)
                .collect();
            if !clash.is_empty() {
                bail!("would overwrite existing files (use -y): {}", clash.join(", "));
            }
        }
        for (e, t) in listing.entries.iter().zip(&targets) {
            if e.is_dir {
                fs::create_dir_all(t.as_ref().unwrap())?;
            }
        }
    }
    let mut files: Vec<(&IsoEntry, Option<&Path>)> = listing.entries.iter().zip(&targets).filter(|(e, _)| !e.is_dir).map(|(e, t)| (e, t.as_deref())).collect();
    files.sort_by_key(|(e, _)| e.extents.first().and_then(|x| x.offset).unwrap_or(0));
    if let Some(p) = progress {
        p.set_total(files.iter().map(|(e, _)| e.size).sum());
    }
    let mut src = File::open(&listing.path).with_context(|| format!("cannot open {}", listing.path.display()))?;
    let mut buf = vec![0u8; 1 << 20];
    let mut bytes = 0;
    for (e, target) in &files {
        if let Some(p) = progress {
            p.check()?;
            p.set_current(&e.path);
        }
        let name = || e.path.clone();
        match target {
            Some(t) => {
                if let Some(dir) = t.parent() {
                    fs::create_dir_all(dir)?;
                }
                if overwrite && t.exists() {
                    let mut perm = fs::metadata(t)?.permissions();
                    #[allow(clippy::permissions_set_readonly_false)]
                    perm.set_readonly(false);
                    fs::set_permissions(t, perm)?;
                }
                let mut w = File::create(t).with_context(|| format!("cannot create {}", t.display()))?;
                copy_extents(&mut src, listing.size, &e.extents, &mut w, &mut buf, progress).with_context(name)?;
                drop(w);
                if e.mtime > 0 {
                    filetime::set_file_mtime(t, filetime::FileTime::from_unix_time(e.mtime, 0))?;
                }
            }
            None => copy_extents(&mut src, listing.size, &e.extents, &mut io::sink(), &mut buf, progress).with_context(name)?,
        }
        bytes += e.size;
    }
    crate::extract::notify_extracted(out);
    Ok(ExtractReport { files: files.len(), bytes, secs: started.elapsed().as_secs_f64() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn record(name: &[u8], lba: u32, len: u32, flags: u8, su: &[u8]) -> Vec<u8> {
        let mut r = vec![0u8; 33];
        r.extend_from_slice(name);
        if name.len() % 2 == 0 {
            r.push(0);
        }
        r.extend_from_slice(su);
        if r.len() % 2 == 1 {
            r.push(0);
        }
        r[0] = r.len() as u8;
        r[2..6].copy_from_slice(&lba.to_le_bytes());
        r[10..14].copy_from_slice(&len.to_le_bytes());
        // 2020-01-02 03:04:05 UTC
        r[18..25].copy_from_slice(&[120, 1, 2, 3, 4, 5, 0]);
        r[25] = flags;
        r[32] = name.len() as u8;
        r
    }

    fn nm(name: &str) -> Vec<u8> {
        let mut v = vec![b'N', b'M', (5 + name.len()) as u8, 1, 0];
        v.extend_from_slice(name.as_bytes());
        v
    }

    fn put(img: &mut Vec<u8>, sector: usize, data: &[u8]) {
        let at = sector * SECTOR as usize;
        if img.len() < at + data.len() {
            img.resize(at + data.len(), 0);
        }
        img[at..at + data.len()].copy_from_slice(data);
    }

    /// A minimal image: primary descriptor at 16, terminator at 17, root at 18.
    fn image(root: &[u8]) -> Vec<u8> {
        let mut img = vec![0u8; 40 * SECTOR as usize];
        let mut vd = vec![0u8; SECTOR as usize];
        vd[0] = 1;
        vd[1..6].copy_from_slice(b"CD001");
        vd[128..130].copy_from_slice(&2048u16.to_le_bytes());
        vd[156..190].copy_from_slice(&record(&[0], 18, root.len() as u32, 2, &[])[..34]);
        put(&mut img, 16, &vd);
        let mut end = vec![0u8; SECTOR as usize];
        end[0] = 255;
        end[1..6].copy_from_slice(b"CD001");
        put(&mut img, 17, &end);
        put(&mut img, 18, root);
        img
    }

    fn parse_image(img: Vec<u8>) -> Result<(Vec<IsoEntry>, &'static str)> {
        let size = img.len() as u64;
        parse(Cursor::new(img), size)
    }

    #[test]
    fn plain_names_lose_their_version_and_times_are_utc() {
        let mut root = record(&[0], 18, 2048, 2, &[]);
        root.extend(record(&[1], 18, 2048, 2, &[]));
        root.extend(record(b"README.TXT;1", 30, 5, 0, &[]));
        let (e, fs) = parse_image(image(&root)).unwrap();
        assert_eq!(fs, "ISO 9660");
        assert_eq!(e.len(), 1);
        assert_eq!((e[0].path.as_str(), e[0].size), ("README.TXT", 5));
        assert_eq!(e[0].extents, vec![Extent { offset: Some(30 * SECTOR), len: 5 }]);
        assert_eq!(e[0].mtime, unix_from_civil(2020, 1, 2, 3, 4, 5));
    }

    #[test]
    fn rock_ridge_names_win_and_symlinks_are_skipped() {
        let mut root = record(&[0], 18, 2048, 2, &[]);
        root.extend(record(&[1], 18, 2048, 2, &[]));
        root.extend(record(b"LONGNA~1.TXT;1", 30, 5, 0, &nm("a long name.txt")));
        root.extend(record(b"LINK.;1", 0, 0, 0, &[b'S', b'L', 5, 1, 0]));
        let (e, fs) = parse_image(image(&root)).unwrap();
        assert_eq!(fs, "Rock Ridge");
        assert_eq!(e.iter().map(|x| x.path.as_str()).collect::<Vec<_>>(), ["a long name.txt"]);
    }

    #[test]
    fn multi_extent_files_join_up() {
        let mut root = record(b"BIG.BIN;1", 30, 2048, 0x80, &[]);
        root.extend(record(b"BIG.BIN;1", 31, 100, 0, &[]));
        let (e, _) = parse_image(image(&root)).unwrap();
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].size, 2148);
        assert_eq!(e[0].extents.len(), 2);
    }

    #[test]
    fn relocated_directories_come_back_where_they_belong() {
        // Rock Ridge moves deep folders to rr_moved and leaves a CL pointer behind.
        let mut cl = nm("deep");
        cl.extend([b'C', b'L', 12, 1]);
        cl.extend(21u32.to_le_bytes());
        cl.extend(21u32.to_be_bytes());
        let mut root = record(&[0], 18, 2048, 2, &[]);
        root.extend(record(&[1], 18, 2048, 2, &[]));
        root.extend(record(b"DEEP", 0, 0, 0, &cl));
        root.extend(record(b"RR_MOVED", 20, 2048, 2, &nm("rr_moved")));
        let mut img = image(&root);
        let mut moved = record(&[0], 20, 2048, 2, &[]);
        moved.extend(record(&[1], 18, 2048, 2, &[]));
        let mut re = nm("deep");
        re.extend([b'R', b'E', 4, 1]);
        moved.extend(record(b"DEEP", 21, 2048, 2, &re));
        put(&mut img, 20, &moved);
        let mut deep = record(&[0], 21, 2048, 2, &[]);
        deep.extend(record(&[1], 20, 2048, 2, &[]));
        deep.extend(record(b"F.TXT;1", 30, 5, 0, &nm("f.txt")));
        put(&mut img, 21, &deep);
        let (e, fs) = parse_image(img).unwrap();
        assert_eq!(fs, "Rock Ridge");
        let names: Vec<_> = e.iter().map(|x| x.path.as_str()).collect();
        assert_eq!(names, ["deep", "deep/f.txt", "rr_moved"]);
    }

    #[test]
    fn a_directory_that_contains_itself_ends() {
        let root = record(b"LOOP", 18, 2048, 2, &[]);
        let (e, _) = parse_image(image(&root)).unwrap();
        assert_eq!(e.iter().map(|x| x.path.as_str()).collect::<Vec<_>>(), ["LOOP"]);
    }

    #[test]
    fn names_cannot_escape_or_upset_windows() {
        for hostile in ["../../evil", "..", ".. ", "...", "a/../../b", "C:\\x"] {
            let root = record(b"X.;1", 30, 1, 0, &nm(hostile));
            let (e, _) = parse_image(image(&root)).unwrap();
            for x in &e {
                assert!(safe_path(&x.path).is_ok(), "{hostile:?} became {:?}", x.path);
                assert!(!x.path.contains('/') && !x.path.contains(".. "), "{hostile:?} became {:?}", x.path);
            }
        }
        assert_eq!(clean_name("con?.txt. ").as_deref(), Some("con_.txt"));
    }

    #[test]
    fn a_file_past_the_end_is_an_error_not_garbage() {
        let root = record(b"F.;1", 1_000_000, 4096, 0, &[]);
        let (e, _) = parse_image(image(&root)).unwrap();
        let dir = std::env::temp_dir().join(format!("dz-iso-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.iso");
        fs::write(&p, image(&root)).unwrap();
        let size = fs::metadata(&p).unwrap().len();
        let mut f = File::open(&p).unwrap();
        let mut buf = vec![0u8; 4096];
        assert!(copy_extents(&mut f, size, &e[0].extents, &mut io::sink(), &mut buf, None).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn garbage_is_not_an_image() {
        assert!(parse_image(vec![0u8; 1 << 20]).is_err());
        assert!(parse_image(vec![0xAB; 100]).is_err());
    }
}
