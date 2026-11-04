use anyhow::Result;
use clap::{Parser, Subcommand};
use dalzip::util::human;
use dalzip::{create, extract, Archive, CreateOpts, Progress};
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// dalzip: solid archiver with content-defined dedup, per-block codec choice
/// (zstd / LZMA2 / PPMd / store), an x86 branch filter and
/// Argon2id + XChaCha20-Poly1305 encryption.
#[derive(Parser)]
#[command(name = "dz", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a new archive (overwrites an existing one)
    C {
        archive: PathBuf,
        #[arg(required = true)]
        inputs: Vec<PathBuf>,
        /// 0 store, 1-3 zstd (fastest unpack), 4-6 LZMA2, 7-9 best codec per block
        #[arg(short, long, default_value_t = 5, value_parser = clap::value_parser!(u8).range(0..=9))]
        level: u8,
        /// Worker threads (default: CPU count, reduced to fit memory)
        #[arg(short, long)]
        threads: Option<usize>,
        /// Encrypt contents and file names
        #[arg(short, long)]
        password: Option<String>,
        /// Disable chunk-level deduplication
        #[arg(long)]
        no_dedup: bool,
        /// Solid block size in MiB (default depends on level)
        #[arg(long)]
        block_mb: Option<usize>,
        #[arg(short, long)]
        quiet: bool,
    },
    /// Create a ZIP archive (deflate, parallel) for tools that only read ZIP
    Z {
        archive: PathBuf,
        #[arg(required = true)]
        inputs: Vec<PathBuf>,
        /// 0 store, 1-9 zlib, 10 libdeflate-12, 11 Zopfli (smallest, slowest)
        #[arg(short, long, default_value_t = 6, value_parser = clap::value_parser!(u32).range(0..=11))]
        level: u32,
    },
    /// Extract with full paths
    X {
        archive: PathBuf,
        /// Output folder
        #[arg(short, long, default_value = ".")]
        out: PathBuf,
        #[arg(short, long)]
        password: Option<String>,
        /// Overwrite existing files
        #[arg(short = 'y', long)]
        yes: bool,
        #[arg(short, long)]
        threads: Option<usize>,
        #[arg(short, long)]
        quiet: bool,
    },
    /// Test integrity (decode everything, check every hash)
    T {
        archive: PathBuf,
        #[arg(short, long)]
        password: Option<String>,
        #[arg(short, long)]
        threads: Option<usize>,
    },
    /// List contents
    L {
        archive: PathBuf,
        #[arg(short, long)]
        password: Option<String>,
    },
}

/// Prints a one-line status to stderr while `p` runs, when stderr is a terminal.
fn status_line(p: &Arc<Progress>, quiet: bool) -> Option<std::thread::JoinHandle<()>> {
    if quiet || !std::io::stderr().is_terminal() {
        return None;
    }
    let p = p.clone();
    Some(std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(250));
        if Arc::strong_count(&p) == 1 {
            eprint!("\r{:70}\r", "");
            return;
        }
        let (done, total, _) = p.snapshot();
        let pct = if total > 0 { done * 100 / total } else { 0 };
        eprint!("\r  {pct:>3}%  {} / {}   ", human(done), human(total));
    }))
}

fn run() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::C { archive, inputs, level, threads, password, no_dedup, block_mb, quiet } => {
            let p = Progress::new();
            let o = CreateOpts { level, threads, password, dedup: !no_dedup, block_mb, progress: Some(p.clone()) };
            let status = status_line(&p, quiet);
            let r = create(&archive, &inputs, &o);
            drop(o);
            drop(p);
            if let Some(s) = status {
                let _ = s.join();
            }
            let r = r?;
            if !quiet {
                for w in &r.warnings {
                    eprintln!("warning: {w}");
                }
                for line in r.summary().lines() {
                    eprintln!("dz: {line}");
                }
            }
            Ok(())
        }
        Cmd::Z { archive, inputs, level } => {
            let r = dalzip::create_zip(&archive, &inputs, level, None)?;
            for w in &r.warnings {
                eprintln!("warning: {w}");
            }
            eprintln!(
                "dz: zip {} files, {} dirs | {} -> {} ({:.1}%) in {:.1}s",
                r.files,
                r.dirs,
                human(r.raw),
                human(r.size),
                if r.raw > 0 { r.size as f64 * 100.0 / r.raw as f64 } else { 0.0 },
                r.secs
            );
            Ok(())
        }
        Cmd::X { archive, out, password, yes, threads, quiet } => {
            if dalzip::is_iso(&archive) {
                let r = dalzip::extract_iso(&dalzip::list_iso(&archive)?, Some(&out), yes, None)?;
                if !quiet {
                    eprintln!("dz: extracted {} files ({}) in {:.1}s; disc images carry no checksums", r.files, human(r.bytes), r.secs);
                }
                return Ok(());
            }
            let r = if dalzip::is_zip(&archive) {
                dalzip::extract_zip(&dalzip::list_zip(&archive)?, Some(&out), yes, password.as_deref(), None)?
            } else {
                let ar = Archive::open(&archive, password.as_deref())?;
                extract(&ar, Some(&out), yes, threads, None)?
            };
            if !quiet {
                eprintln!("dz: extracted {} files ({}) in {:.1}s, all checksums OK", r.files, human(r.bytes), r.secs);
            }
            Ok(())
        }
        Cmd::T { archive, password, threads } => {
            if dalzip::is_iso(&archive) {
                let r = dalzip::extract_iso(&dalzip::list_iso(&archive)?, None, false, None)?;
                eprintln!("dz: read {} files ({}) in {:.1}s; disc images carry no checksums, so only readability was tested", r.files, human(r.bytes), r.secs);
                return Ok(());
            }
            let r = if dalzip::is_zip(&archive) {
                dalzip::extract_zip(&dalzip::list_zip(&archive)?, None, false, password.as_deref(), None)?
            } else {
                let ar = Archive::open(&archive, password.as_deref())?;
                extract(&ar, None, false, threads, None)?
            };
            eprintln!("dz: tested {} files ({}) in {:.1}s, all checksums OK", r.files, human(r.bytes), r.secs);
            Ok(())
        }
        Cmd::L { archive, password } => {
            if dalzip::is_iso(&archive) {
                let z = dalzip::list_iso(&archive)?;
                for e in &z.entries {
                    let when = dalzip::util::fmt_time_at(e.mtime, dalzip::util::local_offset_secs());
                    let size = if e.is_dir { "<dir>".to_string() } else { e.size.to_string() };
                    println!("{size:>12}  {when}  {}{}", e.path, if e.is_dir { "/" } else { "" });
                }
                println!("\n{} entries | image {} | {}", z.entries.len(), human(z.size), z.filesystem);
                return Ok(());
            }
            if dalzip::is_zip(&archive) {
                let z = dalzip::list_zip(&archive)?;
                for e in &z.entries {
                    let when = dalzip::util::fmt_time_at(e.mtime, dalzip::util::local_offset_secs());
                    let size = if e.is_dir { "<dir>".to_string() } else { e.size.to_string() };
                    println!("{size:>12}  {when}  {}{}", e.path, if e.is_dir { "/" } else { "" });
                }
                println!("\n{} entries | archive {}{}", z.entries.len(), human(z.size), if z.encrypted { " | encrypted" } else { "" });
                return Ok(());
            }
            let ar = Archive::open(&archive, password.as_deref())?;
            dalzip::extract::list(&ar);
            Ok(())
        }
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}
