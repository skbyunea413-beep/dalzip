//! Reading ZIP archives (the zip crate does the formats: deflate, deflate64,
//! bzip2, LZMA, zstd, PPMd, xz, ZipCrypto and WinZip AES, Zip64).
//!
//! What this adds on top, as Bandizip does: names without the UTF-8 flag that
//! are not valid UTF-8 are read as CP949 (Korean Windows zips); `\` separators
//! are normalised; every path goes through the same escape check as .dz; and
//! entries extract in parallel, each worker with its own file handle. The
//! crate checks each entry's CRC-32 when it is read to the end.

use crate::extract::{destination_for, safe_path, ExtractReport, Target};
use crate::progress::Progress;
use crate::util::{local_offset_secs, unix_from_civil};
use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;
use zip::result::ZipError;
use zip::ZipArchive;

pub struct ZipEntry {
    pub index: usize,
    /// '/'-separated, no leading or trailing '/'.
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub compressed: u64,
    pub mtime: i64,
}

pub struct ZipListing {
    pub path: PathBuf,
    pub entries: Vec<ZipEntry>,
    pub size: u64,
    pub encrypted: bool,
}

pub fn is_zip(p: &Path) -> bool {
    p.extension().is_some_and(|e| e.eq_ignore_ascii_case("zip"))
}

fn decode_name(raw: &[u8]) -> String {
    match std::str::from_utf8(raw) {
        Ok(s) => s.to_string(),
        // Not UTF-8: Korean Windows tools write the ANSI code page, CP949.
        Err(_) => encoding_rs::EUC_KR.decode(raw).0.into_owned(),
    }
}

fn normalise(name: &str) -> String {
    name.replace('\\', "/").trim_matches('/').to_string()
}

fn open(path: &Path) -> Result<ZipArchive<BufReader<File>>> {
    let f = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    ZipArchive::new(BufReader::with_capacity(1 << 16, f)).with_context(|| format!("{} is not a readable ZIP", path.display()))
}

pub fn list_zip(path: &Path) -> Result<ZipListing> {
    let mut z = open(path)?;
    let offset = local_offset_secs();
    let mut entries = Vec::with_capacity(z.len());
    let mut encrypted = false;
    for i in 0..z.len() {
        let f = z.by_index_raw(i)?;
        let name = normalise(&decode_name(f.name_raw()));
        if name.is_empty() || f.is_symlink() {
            continue;
        }
        encrypted |= f.encrypted();
        // DOS times are local wall-clock time.
        let mtime = f.last_modified().map_or(0, |t| {
            unix_from_civil(t.year() as i64, t.month() as i64, t.day() as i64, t.hour() as i64, t.minute() as i64, t.second() as i64) - offset
        });
        entries.push(ZipEntry { index: i, path: name, is_dir: f.is_dir(), size: f.size(), compressed: f.compressed_size(), mtime });
    }
    let size = fs::metadata(path)?.len();
    Ok(ZipListing { path: path.to_path_buf(), entries, size, encrypted })
}

impl ZipListing {
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

/// Counts bytes into the progress bar and stops on cancel.
struct Counting<'a, R> {
    inner: R,
    progress: Option<&'a Progress>,
}

impl<R: Read> Read for Counting<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(p) = self.progress {
            if p.is_cancelled() {
                return Err(io::Error::other(crate::progress::Cancelled));
            }
        }
        let n = self.inner.read(buf)?;
        if let Some(p) = self.progress {
            p.add(n as u64);
        }
        Ok(n)
    }
}

fn zip_error(e: ZipError, name: &str) -> anyhow::Error {
    match e {
        ZipError::InvalidPassword => anyhow::anyhow!("wrong password (or damaged file): {name}"),
        ZipError::UnsupportedArchive(m) if m == ZipError::PASSWORD_REQUIRED => anyhow::anyhow!("password required: {name}"),
        e => anyhow::Error::new(e).context(name.to_string()),
    }
}

/// Extracts (or with `out == None`, only reads and CRC-checks) every entry.
pub fn extract_zip(
    listing: &ZipListing,
    out: Option<&Path>,
    overwrite: bool,
    password: Option<&str>,
    progress: Option<&Progress>,
) -> Result<ExtractReport> {
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
    let files: Vec<(&ZipEntry, Option<&Path>)> = listing.entries.iter().zip(&targets).filter(|(e, _)| !e.is_dir).map(|(e, t)| (e, t.as_deref())).collect();
    if let Some(p) = progress {
        p.set_total(files.iter().map(|(e, _)| e.size).sum());
    }

    // Each worker reads a contiguous run of entries through its own handle.
    let workers = rayon::current_num_threads().max(1);
    let per = files.len().div_ceil(workers * 4).max(1);
    let results: Vec<Result<u64>> = files
        .par_chunks(per)
        .map(|chunk| -> Result<u64> {
            let mut z = open(&listing.path)?;
            let mut bytes = 0;
            for (e, target) in chunk {
                if let Some(p) = progress {
                    p.check()?;
                    p.set_current(&e.path);
                }
                let f = match password {
                    Some(pw) => z.by_index_decrypt(e.index, pw.as_bytes()),
                    None => z.by_index(e.index),
                }
                .map_err(|err| zip_error(err, &e.path))?;
                let mut r = Counting { inner: f, progress };
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
                        let mut w = BufWriter::with_capacity(1 << 20, File::create(t).with_context(|| format!("cannot create {}", t.display()))?);
                        io::copy(&mut r, &mut w).map_err(|err| read_error(err, &e.path))?;
                        w.flush()?;
                        drop(w);
                        if e.mtime > 0 {
                            filetime::set_file_mtime(t, filetime::FileTime::from_unix_time(e.mtime, 0))?;
                        }
                    }
                    None => {
                        io::copy(&mut r, &mut io::sink()).map_err(|err| read_error(err, &e.path))?;
                    }
                }
                bytes += e.size;
            }
            Ok(bytes)
        })
        .collect();
    let mut bytes = 0;
    for r in results {
        bytes += r?;
    }
    Ok(ExtractReport { files: files.len(), bytes, secs: started.elapsed().as_secs_f64() })
}

/// A read error inside an entry: cancellation, a CRC mismatch, or bad data.
fn read_error(err: io::Error, name: &str) -> anyhow::Error {
    if err.get_ref().is_some_and(|e| e.is::<crate::progress::Cancelled>()) {
        return crate::progress::Cancelled.into();
    }
    anyhow::anyhow!("{name}: {err}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cp949_names_and_separators() {
        // "한글.txt" in CP949, as Korean Windows zip tools write it without the UTF-8 flag.
        let raw = [0xC7, 0xD1, 0xB1, 0xDB, b'.', b't', b'x', b't'];
        assert_eq!(decode_name(&raw), "한글.txt");
        assert_eq!(decode_name("한글.txt".as_bytes()), "한글.txt");
        assert_eq!(normalise("dir\\sub\\a.txt"), "dir/sub/a.txt");
        assert_eq!(normalise("/top/"), "top");
    }

    #[test]
    fn civil_round_trip() {
        for t in [0i64, 951_782_400, 1_793_400_000, 4_102_444_799] {
            let (y, m, d, h, mi, s) = crate::util::civil(t);
            assert_eq!(unix_from_civil(y, m, d, h, mi, s), t);
        }
    }
}
