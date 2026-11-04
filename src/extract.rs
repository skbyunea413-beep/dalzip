//! Opening, listing, testing and extracting archives.
//!
//! Decoding works on chains (a block plus the following blocks primed with its
//! tail): chains decode in parallel, blocks inside a chain in order. Files whose
//! chains are all in memory are then written in parallel.

use crate::codec;
use crate::crypto::{block_aad, Crypto, INDEX_AAD};
use crate::format::*;
use crate::mem;
use crate::progress::Progress;
use crate::util::{fmt_time, human, read_header};
use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub struct Archive {
    file: Mutex<File>,
    crypto: Option<Crypto>,
    pub index: Index,
    pub size: u64,
}

impl Archive {
    pub fn open(path: &Path, password: Option<&str>) -> Result<Archive> {
        let mut f = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
        let size = f.metadata()?.len();
        let (kdf, header_len) = read_header(&mut f)?;
        if size < header_len + FOOTER_LEN {
            bail!("archive is truncated");
        }
        f.seek(SeekFrom::End(-(FOOTER_LEN as i64)))?;
        let mut foot = [0u8; FOOTER_LEN as usize];
        f.read_exact(&mut foot)?;
        if &foot[16..] != FOOTER_MAGIC {
            bail!("archive footer missing (incomplete or damaged file)");
        }
        let idx_off = u64::from_le_bytes(foot[..8].try_into().unwrap());
        let idx_len = u64::from_le_bytes(foot[8..16].try_into().unwrap());
        if idx_off < header_len || idx_off.checked_add(idx_len).map_or(true, |e| e > size - FOOTER_LEN) {
            bail!("archive index location is invalid");
        }
        let mut idx = vec![0u8; idx_len as usize];
        f.seek(SeekFrom::Start(idx_off))?;
        f.read_exact(&mut idx)?;

        let crypto = match kdf {
            None => None,
            Some(k) => {
                let pw = password.context("archive is encrypted; pass the password with -p")?;
                let c = Crypto::derive(pw, &k)?;
                idx = c.open(INDEX_AAD, &idx).map_err(|_| anyhow::anyhow!("wrong password (or damaged index)"))?;
                Some(c)
            }
        };
        let raw = zstd::stream::decode_all(&idx[..]).context("index is damaged")?;
        let index = Index::decode(&raw)?;
        for b in &index.blocks {
            if b.offset < header_len || b.offset.checked_add(b.stored_len).map_or(true, |e| e > idx_off) {
                bail!("block table points outside the data area");
            }
        }
        Ok(Archive { file: Mutex::new(f), crypto, index, size })
    }

    pub fn encrypted(&self) -> bool {
        self.crypto.is_some()
    }

    /// Decodes one block; `tail` must be the last `prefix` raw bytes of the
    /// preceding blocks in its chain.
    fn read_block(&self, id: u32, tail: &[u8]) -> Result<Vec<u8>> {
        let b = &self.index.blocks[id as usize];
        let mut stored = vec![0u8; b.stored_len as usize];
        {
            let mut f = self.file.lock().unwrap();
            f.seek(SeekFrom::Start(b.offset))?;
            f.read_exact(&mut stored)?;
        }
        if let Some(c) = &self.crypto {
            stored = c.open(&block_aad(id), &stored).with_context(|| format!("block {id}"))?;
        }
        if tail.len() != b.prefix as usize {
            bail!("internal error: block {id} decoded without its chain history");
        }
        let raw = codec::decode(b.codec, b.filter, stored, b.raw_len as usize, tail).with_context(|| format!("block {id}"))?;
        if blake3::hash(&raw).as_bytes() != &b.hash {
            bail!("block {id} checksum mismatch");
        }
        Ok(raw)
    }

    /// Decodes the chain that starts at `start`, returning every block in it.
    fn read_chain(&self, start: u32, end: u32) -> Result<Vec<(u32, Arc<Vec<u8>>)>> {
        let mut out: Vec<(u32, Arc<Vec<u8>>)> = Vec::with_capacity((end - start) as usize);
        for id in start..end {
            let need = self.index.blocks[id as usize].prefix as usize;
            let history: Vec<&[u8]> = out.iter().map(|(_, v)| &v[..]).collect();
            let tail = codec::history_tail(&history, need);
            let raw = self.read_block(id, &tail)?;
            out.push((id, Arc::new(raw)));
        }
        Ok(out)
    }
}

/// Turns a stored path into a safe relative path, refusing anything that could
/// escape the output folder.
pub(crate) fn safe_path(p: &str) -> Result<PathBuf> {
    let mut out = PathBuf::new();
    for part in p.split('/') {
        // Windows drops trailing dots and spaces, so ".. " would walk up like "..".
        if part.trim_end_matches(['.', ' ']).is_empty() || part.contains(['\\', ':', '\0']) {
            bail!("refusing unsafe path in archive: {p:?}");
        }
        out.push(part);
    }
    if out.as_os_str().is_empty() {
        bail!("empty path in archive");
    }
    Ok(out)
}

pub fn list(ar: &Archive) {
    let idx = &ar.index;
    let mut raw = 0u64;
    let mut files = 0;
    for e in &idx.entries {
        let when = fmt_time(e.mtime);
        match e.kind {
            Kind::Dir => println!("{:>12}  {when}  {}/", "<dir>", e.path),
            Kind::File => {
                raw += e.size;
                files += 1;
                println!("{:>12}  {when}  {}", e.size, e.path);
            }
        }
    }
    let unique: u64 = idx.blocks.iter().map(|b| b.raw_len).sum();
    println!(
        "\n{files} files, {} dirs | {} raw, {} unique after dedup, archive {} ({:.1}%){}",
        idx.entries.len() - files,
        human(raw),
        human(unique),
        human(ar.size),
        if raw > 0 { ar.size as f64 * 100.0 / raw as f64 } else { 0.0 },
        if ar.encrypted() { " | encrypted" } else { "" }
    );
}

/// Whether the archive needs a password, without deriving any key.
pub fn is_encrypted(path: &Path) -> Result<bool> {
    let mut f = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    Ok(read_header(&mut f)?.0.is_some())
}

struct Chains {
    /// Chain start for each block.
    start_of: Vec<u32>,
    /// (end, raw bytes) per chain, keyed by start.
    span: HashMap<u32, (u32, u64)>,
}

impl Chains {
    fn new(idx: &Index) -> Chains {
        let n = idx.blocks.len();
        // Earliest block whose bytes each block's prefix reaches into. A block
        // that used no prefix may still be history for a later one, so a chain
        // can only start at i when nothing at or after i reaches before i.
        let mut reach = vec![0usize; n];
        for i in 0..n {
            let (mut j, mut left) = (i, idx.blocks[i].prefix as u64);
            while left > 0 && j > 0 {
                j -= 1;
                left = left.saturating_sub(idx.blocks[j].raw_len);
            }
            reach[i] = j;
        }
        let mut barrier = vec![false; n];
        let mut low = usize::MAX;
        for i in (0..n).rev() {
            low = low.min(reach[i]);
            barrier[i] = low >= i;
        }
        let mut start_of = Vec::with_capacity(n);
        let mut span: HashMap<u32, (u32, u64)> = HashMap::new();
        for (i, b) in idx.blocks.iter().enumerate() {
            let s = if barrier[i] { i as u32 } else { start_of[i - 1] };
            start_of.push(s);
            let e = span.entry(s).or_insert((s, 0));
            e.0 = i as u32 + 1;
            e.1 += b.raw_len;
        }
        Chains { start_of, span }
    }

    /// Chains an entry needs, in first-use order.
    fn of(&self, idx: &Index, e: &Entry) -> Vec<u32> {
        let mut seen = Vec::new();
        for &c in &e.chunks {
            let s = self.start_of[idx.chunks[c as usize].block as usize];
            if !seen.contains(&s) {
                seen.push(s);
            }
        }
        seen
    }
}

/// Decoded blocks, evicted a whole chain at a time.
struct Cache {
    blocks: HashMap<u32, Arc<Vec<u8>>>,
    chains: VecDeque<u32>,
    bytes: u64,
}

impl Cache {
    fn has(&self, chain: u32) -> bool {
        self.chains.contains(&chain)
    }

    fn load(&mut self, ar: &Archive, ch: &Chains, pool: &rayon::ThreadPool, want: &[u32]) -> Result<()> {
        let missing: Vec<u32> = want.iter().copied().filter(|c| !self.has(*c)).collect();
        let loaded = pool.install(|| {
            missing.par_iter().map(|&s| ar.read_chain(s, ch.span[&s].0).map(|v| (s, v))).collect::<Result<Vec<_>>>()
        })?;
        for (s, blocks) in loaded {
            for (id, v) in blocks {
                self.bytes += v.len() as u64;
                self.blocks.insert(id, v);
            }
            self.chains.push_back(s);
        }
        Ok(())
    }

    /// Drops least recently loaded chains until `bytes <= budget`, never touching `keep`.
    fn trim(&mut self, ch: &Chains, budget: u64, keep: &HashSet<u32>) {
        let mut i = 0;
        while self.bytes > budget && i < self.chains.len() {
            let s = self.chains[i];
            if keep.contains(&s) {
                i += 1;
                continue;
            }
            self.chains.remove(i);
            for id in s..ch.span[&s].0 {
                if let Some(v) = self.blocks.remove(&id) {
                    self.bytes -= v.len() as u64;
                }
            }
        }
    }
}

/// Where an archive's contents go, after Bandizip's shell verbs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Target {
    /// Straight into the folder.
    Here,
    /// Into `<archive name>\`.
    Name,
    /// Into the folder when everything sits under one top-level item, else `<archive name>\`.
    Auto,
}

impl Archive {
    /// Top-level names in the archive (first path component of every entry).
    pub fn roots(&self) -> Vec<&str> {
        let mut roots: Vec<&str> = self.index.entries.iter().map(|e| e.path.split('/').next().unwrap_or("")).collect();
        roots.sort_unstable();
        roots.dedup();
        roots
    }

    pub fn destination(&self, archive: &Path, base: &Path, target: Target) -> PathBuf {
        destination_for(self.roots().len(), archive, base, target)
    }
}

/// Where to extract, given how many top-level names the archive has (shared by .dz and .zip).
pub fn destination_for(roots: usize, archive: &Path, base: &Path, target: Target) -> PathBuf {
    match target {
        Target::Here => base.to_path_buf(),
        Target::Auto if roots == 1 => base.to_path_buf(),
        Target::Name | Target::Auto => {
            let stem = archive.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "archive".into());
            let mut dir = base.join(&stem);
            let mut n = 2;
            while dir.exists() {
                dir = base.join(format!("{stem} ({n})"));
                n += 1;
            }
            dir
        }
    }
}

pub struct ExtractReport {
    pub files: usize,
    pub bytes: u64,
    pub secs: f64,
}

/// Writes (or with `target == None`, only hashes) one file from cached blocks.
/// Returns false when the content hash does not match.
fn emit(
    idx: &Index,
    blocks: &HashMap<u32, Arc<Vec<u8>>>,
    e: &Entry,
    target: Option<&Path>,
    overwrite: bool,
    progress: Option<&Progress>,
) -> Result<bool> {
    let mut sink = match target {
        Some(t) => {
            if overwrite && t.exists() {
                let mut p = fs::metadata(t)?.permissions();
                if p.readonly() {
                    #[allow(clippy::permissions_set_readonly_false)]
                    p.set_readonly(false);
                    fs::set_permissions(t, p)?;
                }
            }
            if let Some(dir) = t.parent() {
                fs::create_dir_all(dir)?;
            }
            let f = File::create(t).with_context(|| format!("cannot create {}", t.display()))?;
            Some(BufWriter::with_capacity((e.size as usize).clamp(4096, 1 << 20), f))
        }
        None => None,
    };
    let mut hasher = blake3::Hasher::new();
    for &cid in &e.chunks {
        let c = idx.chunks[cid as usize];
        let block = &blocks[&c.block];
        let piece = &block[c.offset as usize..(c.offset + c.len) as usize];
        if let Some(p) = progress {
            p.check()?;
            p.add(piece.len() as u64);
        }
        hasher.update(piece);
        if let Some(w) = sink.as_mut() {
            w.write_all(piece)?;
        }
    }
    if let (Some(w), Some(t)) = (sink, target) {
        w.into_inner().map_err(|e| e.into_error())?;
        filetime::set_file_mtime(t, filetime::FileTime::from_unix_time(e.mtime, e.mtime_ns))?;
        if e.readonly {
            let mut p = fs::metadata(t)?.permissions();
            p.set_readonly(true);
            fs::set_permissions(t, p)?;
        }
    }
    Ok(hasher.finalize().as_bytes() == &e.hash)
}

/// Extracts (or with `out == None`, only verifies) every entry.
pub fn extract(
    ar: &Archive,
    out: Option<&Path>,
    overwrite: bool,
    threads: Option<usize>,
    progress: Option<&Progress>,
) -> Result<ExtractReport> {
    let started = Instant::now();
    let idx = &ar.index;
    let threads = threads.unwrap_or_else(mem::cpu_threads).max(1);
    let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build()?;

    let mut targets = Vec::with_capacity(idx.entries.len());
    for e in &idx.entries {
        let rel = safe_path(&e.path)?;
        targets.push(out.map(|o| o.join(rel)));
    }
    if let Some(root) = out {
        fs::create_dir_all(root)?;
        if !overwrite {
            let clash: Vec<_> = idx
                .entries
                .iter()
                .zip(&targets)
                .filter(|(e, t)| e.kind == Kind::File && t.as_ref().unwrap().exists())
                .map(|(e, _)| e.path.as_str())
                .take(5)
                .collect();
            if !clash.is_empty() {
                bail!("would overwrite existing files (use -y): {}", clash.join(", "));
            }
        }
        for (e, t) in idx.entries.iter().zip(&targets) {
            if e.kind == Kind::Dir {
                fs::create_dir_all(t.as_ref().unwrap())?;
            }
        }
    }

    let ch = Chains::new(idx);
    let biggest = ch.span.values().map(|s| s.1).max().unwrap_or(0);
    let budget = mem::available_memory().map_or(2 << 30, |a| a / 2).clamp(1 << 30, 8 << 30).max(biggest * 2);
    let mut cache = Cache { blocks: HashMap::new(), chains: VecDeque::new(), bytes: 0 };

    let files: Vec<(&Entry, Option<&Path>)> = idx
        .entries
        .iter()
        .zip(&targets)
        .filter(|(e, _)| e.kind == Kind::File)
        .map(|(e, t)| (e, t.as_deref()))
        .collect();
    let needs: Vec<Vec<u32>> = files.iter().map(|(e, _)| ch.of(idx, e)).collect();
    let (mut bytes, mut bad) = (0u64, Vec::new());
    if let Some(p) = progress {
        p.set_total(files.iter().map(|(e, _)| e.size).sum());
    }

    let mut i = 0;
    while i < files.len() {
        // Grow a batch of files whose chains fit in memory together.
        let mut want: Vec<u32> = Vec::new();
        let mut want_bytes = 0u64;
        let mut j = i;
        while j < files.len() && j - i < 8192 {
            let extra: Vec<u32> = needs[j].iter().copied().filter(|c| !want.contains(c)).collect();
            let extra_bytes: u64 = extra.iter().map(|c| ch.span[c].1).sum();
            if j > i && want_bytes + extra_bytes > budget {
                break;
            }
            want.extend(extra);
            want_bytes += extra_bytes;
            j += 1;
        }

        if let Some(p) = progress {
            p.check()?;
            p.set_current(&files[i].0.path);
        }
        if want_bytes <= budget {
            let keep: HashSet<u32> = want.iter().copied().collect();
            cache.trim(&ch, budget.saturating_sub(want_bytes), &keep);
            cache.load(ar, &ch, &pool, &want)?;
            let blocks = &cache.blocks;
            let results = pool.install(|| {
                files[i..j]
                    .par_iter()
                    .map(|(e, t)| emit(idx, blocks, e, *t, overwrite, progress).map(|ok| (ok, *e)))
                    .collect::<Result<Vec<_>>>()
            })?;
            for (ok, e) in results {
                bytes += e.size;
                if !ok {
                    bad.push(e.path.clone());
                }
            }
        } else {
            // One file larger than memory: stream it, loading a window of chains at a time.
            let (e, t) = files[i];
            let mut sink = t.map(|t| -> Result<_> { Ok(BufWriter::with_capacity(1 << 20, File::create(t)?)) }).transpose()?;
            let mut hasher = blake3::Hasher::new();
            let mut k = 0;
            while k < e.chunks.len() {
                let first = ch.start_of[idx.chunks[e.chunks[k] as usize].block as usize];
                if !cache.has(first) {
                    let pos = needs[i].iter().position(|&c| c == first).unwrap();
                    let window: Vec<u32> = needs[i][pos..].iter().copied().take(threads).collect();
                    let window_bytes: u64 = window.iter().map(|c| ch.span[c].1).sum();
                    cache.trim(&ch, budget.saturating_sub(window_bytes), &HashSet::from([first]));
                    cache.load(ar, &ch, &pool, &window)?;
                }
                // Take every following chunk that is already in memory.
                let mut end = k;
                while end < e.chunks.len() && cache.blocks.contains_key(&idx.chunks[e.chunks[end] as usize].block) {
                    end += 1;
                }
                for &cid in &e.chunks[k..end] {
                    let c = idx.chunks[cid as usize];
                    let piece = &cache.blocks[&c.block][c.offset as usize..(c.offset + c.len) as usize];
                    if let Some(p) = progress {
                        p.check()?;
                        p.add(piece.len() as u64);
                    }
                    hasher.update(piece);
                    if let Some(w) = sink.as_mut() {
                        w.write_all(piece)?;
                    }
                }
                k = end;
            }
            if let (Some(w), Some(t)) = (sink, t) {
                w.into_inner().map_err(|e| e.into_error())?;
                filetime::set_file_mtime(t, filetime::FileTime::from_unix_time(e.mtime, e.mtime_ns))?;
            }
            if hasher.finalize().as_bytes() != &e.hash {
                bad.push(e.path.clone());
            }
            bytes += e.size;
            j = i + 1;
        }
        i = j;
    }

    notify_extracted(out);
    if !bad.is_empty() {
        bail!("{} file(s) failed verification: {}", bad.len(), bad.join(", "));
    }
    Ok(ExtractReport { files: files.len(), bytes, secs: started.elapsed().as_secs_f64() })
}

/// Lets Explorer re-sort the output folder and the folder it sits in (a new
/// "<name>\" folder appears there and keeps changing while files land in it).
pub(crate) fn notify_extracted(out: Option<&Path>) {
    if let Some(dir) = out {
        crate::util::notify_dir_changed(dir);
        if let Some(parent) = dir.parent() {
            crate::util::notify_dir_changed(parent);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{safe_path, Chains};
    use crate::format::{BlockInfo, Index};

    #[test]
    fn chain_spans_blocks_that_skipped_their_prefix() {
        let block = |prefix| BlockInfo { offset: 0, stored_len: 0, raw_len: 10, codec: 0, filter: 0, prefix, hash: [0; 32] };
        // Block 2 used no prefix, but block 3 reaches back into block 1.
        let idx = Index { blocks: vec![block(0), block(5), block(0), block(15), block(0)], ..Default::default() };
        let ch = Chains::new(&idx);
        assert_eq!(ch.start_of, vec![0, 0, 0, 0, 4]);
        assert_eq!(ch.span[&0], (4, 40));
    }

    #[test]
    fn rejects_paths_that_escape() {
        for bad in ["../x", "a/../../x", "/abs", "C:/win", "a\\b", "a//b", "", "./a", ".. /x", "a/... /b", " "] {
            assert!(safe_path(bad).is_err(), "{bad:?} should be rejected");
        }
        assert!(safe_path("dir/sub/file.txt").is_ok());
    }
}
