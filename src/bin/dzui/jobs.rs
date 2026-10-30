//! Background work for the GUI: every operation runs on its own thread and
//! reports through a shared `Progress` plus a result slot the UI polls.

use eframe::egui;
use anyhow::{bail, Context, Result};
use dalzip::util::human;
use dalzip::{create, create_zip, extract, extract_zip, is_zip, list_zip, Archive, CreateOpts, Progress, Target};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub enum Outcome {
    Created { archives: Vec<PathBuf>, summary: String },
    Extracted { dests: Vec<PathBuf>, summary: String },
    Tested { summary: String },
}

pub struct Job {
    pub title: String,
    pub progress: Arc<Progress>,
    pub result: Arc<Mutex<Option<Result<Outcome>>>>,
    pub started: Instant,
    /// Close the window when this finishes without error (shell quick actions).
    pub auto_close: bool,
    /// Re-run the same job, e.g. with overwrite after a clash.
    pub retry: Option<Request>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    Dz,
    /// Plain deflate ZIP, for people and tools without dalzip.
    Zip,
}

impl Format {
    pub fn ext(self) -> &'static str {
        match self {
            Format::Dz => "dz",
            Format::Zip => "zip",
        }
    }
}

#[derive(Clone)]
pub enum Request {
    Create {
        archive: PathBuf,
        inputs: Vec<PathBuf>,
        format: Format,
        level: u8,
        password: Option<String>,
        dedup: bool,
        test: bool,
        /// Move the inputs to the Recycle Bin once the archive tests clean.
        delete_sources: bool,
    },
    /// One archive per input, next to it.
    CreateEach { inputs: Vec<PathBuf>, format: Format, level: u8 },
    Extract {
        archives: Vec<PathBuf>,
        base: Option<PathBuf>,
        target: Target,
        overwrite: bool,
        password: Option<String>,
        /// Move the archives to the Recycle Bin after every file checked out.
        delete_archives: bool,
    },
    Test { archives: Vec<PathBuf>, password: Option<String> },
}

impl Request {
    pub fn title(&self) -> &'static str {
        match self {
            Request::Create { .. } | Request::CreateEach { .. } => "압축하는 중",
            Request::Extract { .. } => "압축 푸는 중",
            Request::Test { .. } => "압축 파일 테스트 중",
        }
    }
}

pub fn spawn(req: Request, ctx: egui::Context, auto_close: bool) -> Job {
    let progress = Progress::new();
    let result = Arc::new(Mutex::new(None));
    let (p, slot, r) = (progress.clone(), result.clone(), req.clone());
    std::thread::spawn(move || {
        let out = run(&r, &p);
        *slot.lock().unwrap() = Some(out);
        ctx.request_repaint();
    });
    Job { title: req.title().to_string(), progress, result, started: Instant::now(), auto_close, retry: Some(req) }
}

/// Deflate level for a 0-9 dalzip level when writing ZIP.
fn zip_level(level: u8) -> u32 {
    (level as u32).min(9)
}

fn make(archive: &Path, inputs: &[PathBuf], format: Format, level: u8, password: Option<String>, dedup: bool, p: &Arc<Progress>) -> Result<String> {
    match format {
        Format::Dz => {
            let o = CreateOpts { level, threads: None, password, dedup, block_mb: None, progress: Some(p.clone()) };
            let r = create(archive, inputs, &o)?;
            let mut s = r.summary();
            for w in &r.warnings {
                s.push_str(&format!("\n경고: {w}"));
            }
            Ok(s)
        }
        Format::Zip => {
            let r = create_zip(archive, inputs, zip_level(level), Some(p))?;
            let pct = if r.raw > 0 { r.size as f64 * 100.0 / r.raw as f64 } else { 0.0 };
            let mut s = format!("ZIP {} files, {} dirs | {} -> {} ({pct:.1}%) in {:.1}s", r.files, r.dirs, human(r.raw), human(r.size), r.secs);
            for w in &r.warnings {
                s.push_str(&format!("\n경고: {w}"));
            }
            Ok(s)
        }
    }
}

fn run(req: &Request, p: &Arc<Progress>) -> Result<Outcome> {
    match req {
        Request::Create { archive, inputs, format, level, password, dedup, test, delete_sources } => {
            if *delete_sources && *format != Format::Dz {
                bail!("원본 삭제는 테스트할 수 있는 .dz 압축에서만 합니다");
            }
            let mut summary = make(archive, inputs, *format, *level, password.clone(), *dedup, p)?;
            // Never delete originals on the strength of an untested archive.
            if *test || *delete_sources {
                p.reset();
                p.set_current("테스트");
                let ar = Archive::open(archive, password.as_deref())?;
                let t = extract(&ar, None, false, None, Some(p))?;
                summary.push_str(&format!("\n테스트 통과: {}개 파일 모두 체크섬 일치", t.files));
            }
            if *delete_sources {
                recycle(inputs)?;
                summary.push_str(&format!("\n원본 {}개를 휴지통으로 옮겼습니다", inputs.len()));
            }
            Ok(Outcome::Created { archives: vec![archive.clone()], summary })
        }
        Request::CreateEach { inputs, format, level } => {
            let mut archives = Vec::new();
            let mut lines = Vec::new();
            for input in inputs {
                p.reset();
                let archive = archive_name_for(std::slice::from_ref(input), format.ext())?;
                let s = make(&archive, std::slice::from_ref(input), *format, *level, None, true, p)?;
                lines.push(format!("{}: {}", archive.file_name().map(|n| n.to_string_lossy()).unwrap_or_default(), s.lines().next().unwrap_or("")));
                archives.push(archive);
            }
            Ok(Outcome::Created { archives, summary: lines.join("\n") })
        }
        Request::Extract { archives, base, target, overwrite, password, delete_archives } => {
            let mut dests = Vec::new();
            let (mut files, mut bytes) = (0, 0);
            for a in archives {
                p.reset();
                let base = match base {
                    Some(b) => b.clone(),
                    None => a.parent().map(Path::to_path_buf).unwrap_or_default(),
                };
                let (dest, r) = if is_zip(a) {
                    let z = list_zip(a)?;
                    let dest = z.destination(&base, *target);
                    let r = extract_zip(&z, Some(&dest), *overwrite, password.as_deref(), Some(p))?;
                    (dest, r)
                } else {
                    let ar = Archive::open(a, password.as_deref()).with_context(|| a.display().to_string())?;
                    let dest = ar.destination(a, &base, *target);
                    let r = extract(&ar, Some(&dest), *overwrite, None, Some(p))?;
                    (dest, r)
                };
                files += r.files;
                bytes += r.bytes;
                dests.push(dest);
            }
            let mut summary = format!("{files}개 파일 ({}) 풀기 완료, 모든 체크섬 일치", human(bytes));
            if *delete_archives {
                recycle(archives)?;
                summary.push_str(&format!("\n압축 파일 {}개를 휴지통으로 옮겼습니다", archives.len()));
            }
            Ok(Outcome::Extracted { dests, summary })
        }
        Request::Test { archives, password } => {
            let mut lines = Vec::new();
            for a in archives {
                p.reset();
                let r = if is_zip(a) {
                    extract_zip(&list_zip(a)?, None, false, password.as_deref(), Some(p))?
                } else {
                    let ar = Archive::open(a, password.as_deref()).with_context(|| a.display().to_string())?;
                    extract(&ar, None, false, None, Some(p))?
                };
                lines.push(format!(
                    "{}: {}개 파일 ({}) 정상",
                    a.file_name().map(|n| n.to_string_lossy()).unwrap_or_default(),
                    r.files,
                    human(r.bytes)
                ));
            }
            Ok(Outcome::Tested { summary: lines.join("\n") })
        }
    }
}

/// Sends files and folders to the Recycle Bin (Bandizip deletes outright;
/// here a mistaken "…하고 삭제" can still be undone).
pub fn recycle(paths: &[PathBuf]) -> Result<()> {
    use windows_sys::Win32::UI::Shell::{SHFileOperationW, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, FO_DELETE, SHFILEOPSTRUCTW};
    if paths.is_empty() {
        return Ok(());
    }
    // Double-NUL-terminated list of NUL-separated absolute paths.
    let mut from: Vec<u16> = Vec::new();
    for p in paths {
        let abs = std::path::absolute(p)?;
        from.extend(abs.as_os_str().to_string_lossy().encode_utf16());
        from.push(0);
    }
    from.push(0);
    let mut op: SHFILEOPSTRUCTW = unsafe { std::mem::zeroed() };
    op.wFunc = FO_DELETE as u32;
    op.pFrom = from.as_ptr();
    op.fFlags = (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI) as u16;
    let rc = unsafe { SHFileOperationW(&mut op) };
    if rc != 0 || op.fAnyOperationsAborted != 0 {
        bail!("휴지통으로 옮기지 못했습니다 (code {rc:#x})");
    }
    Ok(())
}

/// The stem Bandizip uses for "${name}으로 압축하기": one item -> its own name,
/// several -> the folder they sit in.
pub fn base_name(inputs: &[PathBuf]) -> Result<(PathBuf, String)> {
    let first = inputs.first().context("nothing selected")?;
    let parent = first.parent().map(Path::to_path_buf).unwrap_or_default();
    let stem = if inputs.len() == 1 {
        if first.is_dir() {
            first.file_name()
        } else {
            first.file_stem()
        }
        .map(|s| s.to_string_lossy().into_owned())
    } else {
        parent.file_name().map(|s| s.to_string_lossy().into_owned())
    };
    Ok((parent, stem.filter(|s| !s.is_empty()).unwrap_or_else(|| "archive".into())))
}

/// `<dir>\<name>.<ext>`, never overwriting an existing archive.
pub fn archive_name_for(inputs: &[PathBuf], ext: &str) -> Result<PathBuf> {
    let (dir, stem) = base_name(inputs)?;
    unique(&dir, &stem, ext)
}

pub fn unique(dir: &Path, stem: &str, ext: &str) -> Result<PathBuf> {
    for n in 1..10_000 {
        let name = if n == 1 { format!("{stem}.{ext}") } else { format!("{stem} ({n}).{ext}") };
        let p = dir.join(name);
        if !p.exists() {
            return Ok(p);
        }
    }
    bail!("no free archive name in {}", dir.display())
}
