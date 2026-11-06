//! dzui — dalzip's window. With no arguments it shows the start screen; the
//! Explorer menu (dz_shell.dll) launches it with one of these commands, after
//! Bandizip's `bc`/`bx -target:` protocol:
//!
//! ```text
//! dzui <archive.dz> | open <archive>         browse an archive
//! dzui c  [--list F] [paths…]                 "새로 압축" dialog
//! dzui cq [--format zip] [--delete] [--rule] [--list F] [paths…]
//!                                             compress to <name>.dz|.zip right away
//! dzui ce [--format zip] [--list F] [paths…]  one archive per item
//! dzui x --target here|name|auto|dlg [--delete] [--list F] [archives…]
//! dzui t  [--list F] [archives…]              test
//! dzui settings                               Explorer menu settings
//! dzui newfolder <folder>                     make "새 폴더" there and start renaming it
//! dzui --register | --unregister              Explorer integration (HKCU)
//! ```
//!
//! `--delete` moves the sources (after a clean test) or the extracted
//! archives to the Recycle Bin; `--rule` names the archive with the
//! date rule from the settings (`${NAME} %Y-%m-%d %H-%M-%S`).
//!
//! `--list F` names a UTF-8 file with one path per line (the shell DLL writes
//! it to %TEMP% so long selections do not hit the command-line limit); it is
//! deleted after reading.

#![windows_subsystem = "windows"]

mod app;
mod icons;
mod jobs;
mod newfolder;
mod preview;
mod register;
mod settings;

use eframe::egui;
use anyhow::{bail, Result};
use app::{App, Launch};
use dalzip::Target;
use jobs::{Format, Request};
use std::path::PathBuf;

fn read_list(path: &str) -> Result<Vec<PathBuf>> {
    let p = PathBuf::from(path);
    let text = std::fs::read_to_string(&p)?;
    // Only clean up the hand-off files the shell DLL makes.
    if p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("dalzip-")) {
        let _ = std::fs::remove_file(&p);
    }
    Ok(text.lines().map(str::trim).filter(|l| !l.is_empty()).map(PathBuf::from).collect())
}

fn parse(args: &[String]) -> Result<Launch> {
    let Some(cmd) = args.first() else { return Ok(Launch::Start) };
    let mut paths = Vec::new();
    let mut target = Target::Auto;
    let mut dialog = false;
    let mut format = Format::Dz;
    let mut delete = false;
    let mut rule = false;
    let mut it = args[1..].iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--list" => paths.extend(read_list(it.next().map(String::as_str).unwrap_or_default())?),
            "--target" => match it.next().map(String::as_str) {
                Some("here") => target = Target::Here,
                Some("name") => target = Target::Name,
                Some("auto") => target = Target::Auto,
                Some("dlg") => dialog = true,
                other => bail!("unknown target {other:?}"),
            },
            "--format" => match it.next().map(String::as_str) {
                Some("zip") => format = Format::Zip,
                Some("dz") => format = Format::Dz,
                other => bail!("unknown format {other:?}"),
            },
            "--delete" => delete = true,
            "--rule" => rule = true,
            _ => paths.push(PathBuf::from(a)),
        }
    }
    Ok(match cmd.as_str() {
        "c" => Launch::Compose(paths),
        "cq" => {
            let archive = if rule {
                let (dir, stem) = jobs::base_name(&paths)?;
                let name = settings::expand_rule(&settings::Settings::load().rule, &stem);
                jobs::unique(&dir, &name, format.ext())?
            } else {
                jobs::archive_name_for(&paths, format.ext())?
            };
            let level = if format == Format::Zip { 6 } else { 5 };
            Launch::Quick(Request::Create { archive, inputs: paths, format, level, password: None, dedup: true, test: false, delete_sources: delete })
        }
        "ce" => Launch::Quick(Request::CreateEach { inputs: paths, format, level: if format == Format::Zip { 6 } else { 5 } }),
        "x" if dialog => Launch::ExtractDialog(paths),
        "x" => Launch::Quick(Request::Extract { archives: paths, base: None, target, overwrite: false, password: None, delete_archives: delete }),
        "t" => Launch::Quick(Request::Test { archives: paths, password: None }),
        "open" => Launch::Open(paths.into_iter().next().unwrap_or_default()),
        "settings" => Launch::Settings,
        _ => Launch::Open(PathBuf::from(cmd)),
    })
}

/// Seconds to add to UTC for local time (no DST adjustment).
fn tz_offset() -> i64 {
    use windows_sys::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
    let mut tz: TIME_ZONE_INFORMATION = unsafe { std::mem::zeroed() };
    unsafe { GetTimeZoneInformation(&mut tz) };
    -(tz.Bias as i64) * 60
}

fn fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".into());
    if let Ok(bytes) = std::fs::read(format!("{windir}\\Fonts\\malgun.ttf")) {
        fonts.font_data.insert("malgun".into(), std::sync::Arc::new(egui::FontData::from_owned(bytes)));
        for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            if let Some(list) = fonts.families.get_mut(&fam) {
                let at = if fam == egui::FontFamily::Proportional { 0 } else { list.len() };
                list.insert(at, "malgun".into());
            }
        }
    }
    ctx.set_fonts(fonts);
    ctx.set_visuals(egui::Visuals::light());
    ctx.global_style_mut(|s| {
        s.spacing.button_padding = egui::vec2(10.0, 5.0);
        for (_, f) in s.text_styles.iter_mut() {
            f.size += 1.5;
        }
    });
}

fn message(text: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_OK};
    let w: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    let t: Vec<u16> = "dalzip".encode_utf16().chain(Some(0)).collect();
    unsafe { MessageBoxW(std::ptr::null_mut(), w.as_ptr(), t.as_ptr(), MB_OK) };
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if let Some(flag @ ("--register" | "--unregister")) = args.first().map(String::as_str) {
        let dir = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf())).unwrap_or_default();
        let r = if flag == "--register" { register::register(&dir) } else { register::unregister() };
        let line = match &r {
            Ok(s) => s.clone(),
            Err(e) => format!("error: {e:#}"),
        };
        // No console in a GUI binary: leave the outcome next to the exe for scripts, and tell the user.
        let _ = std::fs::write(dir.join("dzui-register.log"), &line);
        if !args.iter().any(|a| a == "--silent") {
            message(&line);
        }
        std::process::exit(if r.is_ok() { 0 } else { 1 });
    }

    // No window: create the folder, hand it to Explorer for renaming, done.
    if args.first().map(String::as_str) == Some("newfolder") {
        let dir = PathBuf::from(args.get(1).map(String::as_str).unwrap_or_default());
        match newfolder::create(&dir) {
            Ok(folder) => newfolder::select_for_rename(&dir, &folder),
            Err(e) => {
                message(&format!("{e:#}"));
                std::process::exit(1);
            }
        }
        return;
    }

    let launch = match parse(&args) {
        Ok(l) => l,
        Err(e) => {
            message(&format!("{e:#}"));
            std::process::exit(2);
        }
    };
    let small = matches!(launch, Launch::Quick(_));
    let size = if small { [520.0, 260.0] } else { [960.0, 660.0] };
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(size)
            .with_min_inner_size([420.0, 240.0])
            .with_drag_and_drop(true)
            .with_icon(egui::IconData { rgba: include_bytes!("../../../assets/dalzip-64.rgba").to_vec(), width: 64, height: 64 }),
        ..Default::default()
    };
    let tz = tz_offset();
    let _ = eframe::run_native(
        "dalzip",
        opts,
        Box::new(move |cc| {
            fonts(&cc.egui_ctx);
            Ok(Box::new(App::new(cc, launch, tz)))
        }),
    );
}
