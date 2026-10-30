//! Archive creation: scan -> chunk + dedup (main thread) -> compress blocks
//! (worker pool) -> write in order (writer thread) -> index + footer.

use crate::codec::{self, BlockKind, Profile};
use crate::crypto::{block_aad, Crypto, KdfParams, INDEX_AAD};
use crate::format::*;
use crate::mem;
use crate::progress::{self, Progress};
use crate::util::{human, write_header};
use anyhow::{bail, Context, Result};
use fastcdc::v2020::StreamCDC;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const CDC_MIN: u32 = 16 << 10;
const CDC_AVG: u32 = 64 << 10;
const CDC_MAX: u32 = 256 << 10;
const PLAIN_PIECE: usize = 1 << 20;

pub struct CreateOpts {
    pub level: u8,
    pub threads: Option<usize>,
    pub password: Option<String>,
    pub dedup: bool,
    pub block_mb: Option<usize>,
    pub progress: Option<Arc<Progress>>,
}

pub struct CreateReport {
    pub files: usize,
    pub dirs: usize,
    pub raw: u64,
    pub size: u64,
    pub secs: f64,
    pub dedup_saved: u64,
    pub threads: usize,
    pub capped: bool,
    pub encrypted: bool,
    pub profile: String,
    pub blocks: String,
    pub warnings: Vec<String>,
}

impl CreateReport {
    pub fn ratio(&self) -> f64 {
        if self.raw > 0 {
            self.size as f64 * 100.0 / self.raw as f64
        } else {
            0.0
        }
    }

    pub fn summary(&self) -> String {
        format!(
            "{} files, {} dirs | {} -> {} ({:.1}%) in {:.1}s ({}/s)\ndedup saved {} | {}, {} threads{}{}\n{}",
            self.files,
            self.dirs,
            human(self.raw),
            human(self.size),
            self.ratio(),
            self.secs,
            human((self.raw as f64 / self.secs.max(0.001)) as u64),
            human(self.dedup_saved),
            self.profile,
            self.threads,
            if self.capped { " (memory-capped)" } else { "" },
            if self.encrypted { " | encrypted (Argon2id + XChaCha20-Poly1305)" } else { "" },
            self.blocks
        )
    }
}

/// Sort/grouping class: similar data lands in the same solid blocks.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum Group {
    Text,
    Binary,
    Exec,
    Packed,
}

fn classify(ext: &str) -> Group {
    match ext {
        "exe" | "dll" | "sys" | "ocx" | "cpl" | "scr" | "drv" | "efi" | "so" | "dylib" | "o" | "obj" | "lib"
        | "a" | "pyd" | "node" | "rlib" => Group::Exec,
        "zip" | "7z" | "rar" | "gz" | "tgz" | "xz" | "txz" | "bz2" | "zst" | "lz4" | "br" | "cab" | "jpg" | "jpeg"
        | "png" | "gif" | "webp" | "avif" | "heic" | "jxl" | "mp3" | "m4a" | "aac" | "ogg" | "opus" | "flac"
        | "mp4" | "m4v" | "mkv" | "webm" | "mov" | "avi" | "wmv" | "docx" | "xlsx" | "pptx" | "jar" | "apk"
        | "msix" | "appx" | "nupkg" | "whl" | "woff" | "woff2" | "dz" => Group::Packed,
        "txt" | "md" | "rs" | "c" | "h" | "cc" | "cpp" | "hpp" | "cs" | "java" | "kt" | "go" | "py" | "js"
        | "mjs" | "cjs" | "ts" | "tsx" | "jsx" | "json" | "xml" | "html" | "htm" | "css" | "scss" | "svg" | "yml"
        | "yaml" | "toml" | "ini" | "cfg" | "csv" | "tsv" | "log" | "sql" | "sh" | "ps1" | "bat" | "cmd"
        | "gd" | "tscn" | "tres" | "lua" | "rb" | "php" | "map" | "lock" => Group::Text,
        _ => Group::Binary,
    }
}

pub(crate) struct Source {
    pub(crate) abs: PathBuf,
    pub(crate) name: String,
    pub(crate) kind: Kind,
    pub(crate) size: u64,
    pub(crate) mtime: i64,
    pub(crate) mtime_ns: u32,
    pub(crate) readonly: bool,
    pub(crate) group: Group,
    ext: String,
}

fn unix_time(t: std::io::Result<SystemTime>) -> (i64, u32) {
    match t {
        Ok(t) => match t.duration_since(UNIX_EPOCH) {
            Ok(d) => (d.as_secs() as i64, d.subsec_nanos()),
            Err(e) => {
                let d = e.duration();
                (-(d.as_secs() as i64) - (d.subsec_nanos() > 0) as i64, (1_000_000_000 - d.subsec_nanos()) % 1_000_000_000)
            }
        },
        Err(_) => (0, 0),
    }
}

fn source(abs: PathBuf, name: String, meta: &fs::Metadata) -> Source {
    let (mtime, mtime_ns) = unix_time(meta.modified());
    let ext = abs.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    Source {
        abs,
        name,
        kind: if meta.is_dir() { Kind::Dir } else { Kind::File },
        size: if meta.is_dir() { 0 } else { meta.len() },
        mtime,
        mtime_ns,
        readonly: meta.permissions().readonly(),
        group: classify(&ext),
        ext,
    }
}

pub(crate) fn scan(inputs: &[PathBuf], skip: &[PathBuf], warn: &mut Vec<String>) -> Result<Vec<Source>> {
    let mut out = Vec::new();
    for input in inputs {
        let meta = fs::symlink_metadata(input).with_context(|| format!("cannot read {}", input.display()))?;
        let canon = fs::canonicalize(input)?;
        // "." or a drive root has no name of its own: store its contents at the top level.
        let base = canon.file_name().map(|n| n.to_string_lossy().into_owned());
        if !meta.is_dir() {
            out.push(source(input.clone(), base.context("input has no file name")?, &meta));
            continue;
        }
        for e in walkdir::WalkDir::new(input).follow_links(false).sort_by_file_name() {
            let e = match e {
                Ok(e) => e,
                Err(err) => {
                    warn.push(format!("skipped: {err}"));
                    continue;
                }
            };
            if e.file_type().is_symlink() {
                warn.push(format!("skipped symlink: {}", e.path().display()));
                continue;
            }
            let rel: Vec<String> = e
                .path()
                .strip_prefix(input)
                .unwrap()
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            let mut parts: Vec<String> = base.iter().cloned().collect();
            parts.extend(rel);
            if parts.is_empty() {
                continue;
            }
            if e.file_type().is_file() && skip.iter().any(|s| e.file_name() == s.file_name().unwrap_or_default()) {
                if let Ok(c) = fs::canonicalize(e.path()) {
                    if skip.contains(&c) {
                        continue;
                    }
                }
            }
            match e.metadata() {
                Ok(m) => out.push(source(e.path().to_path_buf(), parts.join("/"), &m)),
                Err(err) => warn.push(format!("skipped {}: {err}", e.path().display())),
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    for s in &out {
        if !seen.insert(s.name.to_lowercase()) {
            bail!("two inputs map to the same archive path: {}", s.name);
        }
    }
    Ok(out)
}

struct Job {
    id: u32,
    data: Arc<Vec<u8>>,
    /// Preceding blocks of the same chain (oldest first), offered as preset dictionary.
    history: Vec<Arc<Vec<u8>>>,
    kind: BlockKind,
}

struct Done {
    id: u32,
    stored: Vec<u8>,
    raw_len: u64,
    codec: u8,
    filter: u8,
    prefix: u32,
    hash: [u8; 32],
}

struct BlockBuilder {
    buf: Vec<u8>,
    exec_bytes: usize,
    text_bytes: usize,
    id: u32,
    block_size: usize,
    /// Current chain: its kind, length in blocks, and recent blocks (enough to fill a dictionary).
    chain_kind: Option<BlockKind>,
    chain_len: u32,
    history: VecDeque<Arc<Vec<u8>>>,
    chain: u32,
    history_cap: usize,
    tx: SyncSender<Job>,
}

impl BlockBuilder {
    /// Appends a chunk, flushing first if it would not fit. Returns (block, offset).
    fn push(&mut self, data: &[u8], group: Group) -> Result<(u32, u32)> {
        if self.buf.len() + data.len() > self.block_size {
            self.flush()?;
        }
        let off = self.buf.len() as u32;
        self.buf.extend_from_slice(data);
        match group {
            Group::Exec => self.exec_bytes += data.len(),
            Group::Text => self.text_bytes += data.len(),
            _ => {}
        }
        Ok((self.id, off))
    }

    fn flush(&mut self) -> Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let data = Arc::new(std::mem::replace(&mut self.buf, Vec::with_capacity(self.block_size)));
        let kind = if self.exec_bytes * 2 > data.len() {
            BlockKind::Exec
        } else if self.text_bytes * 10 > data.len() * 9 {
            BlockKind::Text
        } else {
            BlockKind::Other
        };
        // A prefix from different content mostly costs time, and chains are capped
        // so reaching a late block never means decoding the whole archive.
        if self.chain_kind != Some(kind) || self.chain_len >= self.chain {
            self.chain_kind = Some(kind);
            self.chain_len = 0;
            self.history.clear();
        }
        let history: Vec<_> = self.history.iter().cloned().collect();
        self.chain_len += 1;
        self.history.push_back(data.clone());
        while self.history.len() > 1 && self.history.iter().skip(1).map(|b| b.len()).sum::<usize>() >= self.history_cap {
            self.history.pop_front();
        }
        self.tx.send(Job { id: self.id, data, history, kind }).context("compression pipeline stopped")?;
        self.id += 1;
        self.exec_bytes = 0;
        self.text_bytes = 0;
        Ok(())
    }
}

fn worker(rx: Arc<Mutex<Receiver<Job>>>, tx: Sender<Result<Done>>, profile: Profile, crypto: Option<Arc<Crypto>>) {
    loop {
        let job = match rx.lock().unwrap().recv() {
            Ok(j) => j,
            Err(_) => return,
        };
        let id = job.id;
        let res = (|| {
            let hash = *blake3::hash(&job.data).as_bytes();
            let raw_len = job.data.len() as u64;
            let history: Vec<&[u8]> = job.history.iter().map(|b| &b[..]).collect();
            let available = history.iter().map(|b| b.len()).sum();
            let tail = codec::history_tail(&history, profile.max_prefix(available, job.data.len()));
            drop(history);
            let enc = codec::encode(&job.data, Some(&tail), job.kind, &profile)?;
            drop(job);
            let stored = match &crypto {
                Some(c) => c.seal(&block_aad(id), &enc.data)?,
                None => enc.data,
            };
            Ok(Done { id, stored, raw_len, codec: enc.codec, filter: enc.filter, prefix: enc.prefix, hash })
        })();
        let failed = res.is_err();
        if tx.send(res).is_err() || failed {
            return;
        }
    }
}

fn writer(mut w: BufWriter<File>, mut pos: u64, rx: Receiver<Result<Done>>) -> Result<(BufWriter<File>, u64, Vec<BlockInfo>)> {
    let mut blocks = Vec::new();
    let mut pending = BTreeMap::new();
    for done in rx {
        let done = done?;
        pending.insert(done.id, done);
        while let Some(d) = pending.remove(&(blocks.len() as u32)) {
            w.write_all(&d.stored)?;
            blocks.push(BlockInfo {
                offset: pos,
                stored_len: d.stored.len() as u64,
                raw_len: d.raw_len,
                codec: d.codec,
                filter: d.filter,
                prefix: d.prefix,
                hash: d.hash,
            });
            pos += d.stored.len() as u64;
        }
    }
    if !pending.is_empty() {
        bail!("internal error: block {} never arrived", blocks.len());
    }
    Ok((w, pos, blocks))
}

#[derive(Default)]
struct Totals {
    raw: u64,
    dedup_saved: u64,
    files: usize,
    dirs: usize,
}

pub fn create(archive: &Path, inputs: &[PathBuf], o: &CreateOpts) -> Result<CreateReport> {
    let started = Instant::now();
    let profile = Profile::for_level(o.level, o.block_mb);

    let part = PathBuf::from(format!("{}.part", archive.display()));
    let parent = archive.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let parent = fs::canonicalize(parent).with_context(|| format!("output folder {} does not exist", parent.display()))?;
    let skip = [parent.join(archive.file_name().context("bad archive name")?), parent.join(part.file_name().unwrap())];

    let mut warnings = Vec::new();
    let mut sources = scan(inputs, &skip, &mut warnings)?;
    sources.sort_by(|a, b| (a.kind == Kind::File, a.group, &a.ext, &a.name).cmp(&(b.kind == Kind::File, b.group, &b.ext, &b.name)));
    if let Some(p) = &o.progress {
        p.set_total(sources.iter().map(|s| s.size).sum());
    }

    let (threads, capped) = mem::pick_threads(o.threads, profile.mem_per_thread());
    let kdf = o.password.as_ref().map(|_| KdfParams::fresh());
    let crypto = match (&o.password, &kdf) {
        (Some(pw), Some(k)) => Some(Arc::new(Crypto::derive(pw, k)?)),
        _ => None,
    };

    let header = write_header(kdf.as_ref());
    let mut out = BufWriter::with_capacity(4 << 20, File::create(&part).with_context(|| format!("cannot create {}", part.display()))?);
    out.write_all(&header)?;

    let result = (|| -> Result<(Totals, u64, String)> {
        let (job_tx, job_rx) = mpsc::sync_channel::<Job>(threads);
        let (done_tx, done_rx) = mpsc::channel::<Result<Done>>();
        let job_rx = Arc::new(Mutex::new(job_rx));
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                let (rx, tx, c) = (job_rx.clone(), done_tx.clone(), crypto.clone());
                thread::spawn(move || worker(rx, tx, profile, c))
            })
            .collect();
        drop(done_tx);
        let writer = thread::spawn(move || writer(out, header.len() as u64, done_rx));

        let mut builder = BlockBuilder {
            buf: Vec::with_capacity(profile.block_size),
            exec_bytes: 0,
            text_bytes: 0,
            id: 0,
            block_size: profile.block_size,
            chain_kind: None,
            chain_len: 0,
            history: VecDeque::new(),
            chain: profile.chain,
            history_cap: profile.max_prefix(usize::MAX, profile.block_size).max(profile.block_size),
            tx: job_tx,
        };
        let produced = produce(&sources, o, &mut builder, &mut warnings);
        let produced = produced.and_then(|p| builder.flush().map(|_| p));
        drop(builder);
        for w in workers {
            let _ = w.join();
        }
        let written = writer.join().map_err(|_| anyhow::anyhow!("writer thread panicked"))?;
        // A writer/worker failure is the root cause; report it before the producer's "pipeline stopped".
        let (mut out, pos, blocks) = written?;
        let (totals, chunks, entries) = produced?;

        let index = Index { blocks, chunks, entries };
        let mut idx = zstd::bulk::compress(&index.encode(), 19)?;
        if let Some(c) = &crypto {
            idx = c.seal(INDEX_AAD, &idx)?;
        }
        out.write_all(&idx)?;
        out.write_all(&pos.to_le_bytes())?;
        out.write_all(&(idx.len() as u64).to_le_bytes())?;
        out.write_all(FOOTER_MAGIC)?;
        out.into_inner().map_err(|e| e.into_error())?.sync_all()?;

        let mut by = [0usize; 4];
        let mut e8 = 0;
        for b in &index.blocks {
            by[b.codec as usize] += 1;
            e8 += (b.filter == filter_id::E8E9) as usize;
        }
        let blocks = format!(
            "blocks: {} zstd, {} lzma2, {} ppmd, {} stored, {} with E8/E9 | {} chunks",
            by[1], by[2], by[3], by[0], e8, index.chunks.len()
        );
        Ok((totals, pos + idx.len() as u64 + FOOTER_LEN, blocks))
    })();

    let (t, size, blocks) = match result {
        Ok(r) => r,
        Err(e) => {
            let _ = fs::remove_file(&part);
            return Err(e);
        }
    };
    fs::rename(&part, archive).with_context(|| format!("cannot move {} into place", part.display()))?;

    Ok(CreateReport {
        files: t.files,
        dirs: t.dirs,
        raw: t.raw,
        size,
        secs: started.elapsed().as_secs_f64(),
        dedup_saved: t.dedup_saved,
        threads,
        capped,
        encrypted: crypto.is_some(),
        profile: profile.describe(),
        blocks,
        warnings,
    })
}

fn produce(
    sources: &[Source],
    o: &CreateOpts,
    builder: &mut BlockBuilder,
    warnings: &mut Vec<String>,
) -> Result<(Totals, Vec<ChunkRef>, Vec<Entry>)> {
    let mut t = Totals::default();
    let mut chunks: Vec<ChunkRef> = Vec::new();
    let mut entries = Vec::with_capacity(sources.len());
    let mut seen: HashMap<[u8; 32], u32> = HashMap::new();
    let mut group = None;
    let progress = o.progress.as_deref();

    for s in sources {
        if let Some(p) = progress {
            p.check()?;
            p.set_current(&s.name);
        }
        let mut e = Entry {
            path: s.name.clone(),
            kind: s.kind,
            size: 0,
            mtime: s.mtime,
            mtime_ns: s.mtime_ns,
            readonly: s.readonly,
            hash: [0; 32],
            chunks: Vec::new(),
        };
        if s.kind == Kind::Dir {
            t.dirs += 1;
            entries.push(e);
            continue;
        }
        // Keep blocks homogeneous so the probe and the E8/E9 decision fit the whole block.
        if group.is_some_and(|g| g != s.group) && builder.buf.len() >= 256 << 10 {
            builder.flush()?;
        }
        group = Some(s.group);

        let mut hasher = blake3::Hasher::new();
        let mut add = |data: &[u8], e: &mut Entry| -> Result<()> {
            if let Some(p) = progress {
                p.check()?;
                p.add(data.len() as u64);
            }
            hasher.update(data);
            e.size += data.len() as u64;
            let h = *blake3::hash(data).as_bytes();
            if o.dedup {
                if let Some(&id) = seen.get(&h) {
                    e.chunks.push(id);
                    t.dedup_saved += data.len() as u64;
                    return Ok(());
                }
            }
            let (block, offset) = builder.push(data, s.group)?;
            let id = chunks.len() as u32;
            chunks.push(ChunkRef { block, offset, len: data.len() as u32 });
            if o.dedup {
                seen.insert(h, id);
            }
            e.chunks.push(id);
            Ok(())
        };

        let read = (|| -> Result<()> {
            let f = File::open(&s.abs)?;
            let r = BufReader::with_capacity(1 << 20, f);
            if o.dedup {
                for c in StreamCDC::new(r, CDC_MIN, CDC_AVG, CDC_MAX) {
                    let c = c.map_err(|err| anyhow::anyhow!("{err:?}"))?;
                    add(&c.data, &mut e)?;
                }
            } else {
                let mut r = r;
                let mut piece = vec![0u8; PLAIN_PIECE];
                loop {
                    let n = read_full(&mut r, &mut piece)?;
                    if n == 0 {
                        break;
                    }
                    add(&piece[..n], &mut e)?;
                }
            }
            Ok(())
        })();
        if let Err(err) = read {
            // Cancelling or a dead pipeline is fatal; an unreadable file is only skipped.
            if progress::is_cancelled(&err) || err.to_string().contains("pipeline stopped") {
                return Err(err);
            }
            warnings.push(format!("skipped {}: {err:#}", s.abs.display()));
            continue;
        }
        e.hash = *hasher.finalize().as_bytes();
        t.raw += e.size;
        t.files += 1;
        entries.push(e);
    }
    Ok((t, chunks, entries))
}

fn read_full(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}
