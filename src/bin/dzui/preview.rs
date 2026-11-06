//! Bandizip's small image preview under the folder tree: the selected file is
//! read out of the archive and decoded on a worker thread, then drawn scaled
//! to fit. Only the one selected file is read, never the whole archive.

use eframe::egui;
use anyhow::Result;
use dalzip::{read_zip_entry, Archive, IsoListing};
use egui::{Color32, RichText, Vec2};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Where the bytes of a listed item come from.
pub enum Source {
    Dz(Archive),
    Zip { path: PathBuf, password: Option<String> },
    Iso(IsoListing),
}

impl Source {
    /// `src` is the format's own index: .dz entry, ZIP central-directory index, or ISO entry.
    fn read(&self, src: usize) -> Result<Vec<u8>> {
        match self {
            Source::Dz(ar) => ar.read_file(src),
            Source::Zip { path, password } => read_zip_entry(path, src, password.as_deref()),
            Source::Iso(l) => l.read_entry(src),
        }
    }
}

const IMAGE_EXTS: [&str; 7] = ["png", "jpg", "jpeg", "gif", "bmp", "webp", "ico"];
/// Bigger files are not read just to draw a thumbnail.
const MAX_BYTES: u64 = 64 << 20;
/// Decoded images are shrunk to this before upload; the panel is never wider.
const THUMB: u32 = 640;

pub fn is_image(name: &str) -> bool {
    name.rsplit_once('.').is_some_and(|(_, e)| IMAGE_EXTS.iter().any(|x| e.eq_ignore_ascii_case(x)))
}

type Slot = Arc<Mutex<Option<Result<(egui::ColorImage, [u32; 2]), String>>>>;

enum State {
    Loading(Slot),
    Ready(egui::TextureHandle, [u32; 2]),
    Failed(String),
}

pub struct Preview {
    source: Arc<Source>,
    /// The list item being shown, and how far it got.
    current: Option<(usize, State)>,
}

fn decode(bytes: &[u8]) -> Result<(egui::ColorImage, [u32; 2])> {
    let img = image::load_from_memory(bytes).map_err(|_| anyhow::anyhow!("이미지로 읽을 수 없는 파일입니다"))?;
    let full = [img.width(), img.height()];
    let small = if img.width() > THUMB || img.height() > THUMB { img.thumbnail(THUMB, THUMB) } else { img };
    let rgba = small.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Ok((egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()), full))
}

impl Preview {
    pub fn new(source: Source) -> Preview {
        Preview { source: Arc::new(source), current: None }
    }

    /// Follows the selection: `item` is (list index, source index, name, size), or `None`.
    pub fn follow(&mut self, ctx: &egui::Context, item: Option<(usize, usize, &str, u64)>) {
        let item = item.filter(|(_, _, name, _)| is_image(name));
        if self.current.as_ref().map(|c| c.0) == item.map(|i| i.0) {
            return;
        }
        self.current = item.map(|(i, src, _, size)| {
            if size > MAX_BYTES {
                return (i, State::Failed(format!("{} MiB가 넘는 파일은 미리 보지 않습니다", MAX_BYTES >> 20)));
            }
            let slot: Slot = Arc::new(Mutex::new(None));
            let (s, source, ctx) = (slot.clone(), self.source.clone(), ctx.clone());
            std::thread::spawn(move || {
                let r = source.read(src).and_then(|b| decode(&b)).map_err(|e| {
                    // An encrypted ZIP lists without a password; reading needs one.
                    let e = format!("{e:#}");
                    if e.contains("password") { "암호가 걸린 파일입니다".to_string() } else { e }
                });
                *s.lock().unwrap() = Some(r);
                ctx.request_repaint();
            });
            (i, State::Loading(slot))
        });
    }

    pub fn active(&self) -> bool {
        self.current.is_some()
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        let Some((_, state)) = &mut self.current else { return };
        let done = match state {
            State::Loading(slot) => slot.lock().unwrap().take(),
            _ => None,
        };
        if let Some(r) = done {
            *state = match r {
                Ok((img, full)) => State::Ready(ui.ctx().load_texture("preview", img, egui::TextureOptions::LINEAR), full),
                Err(e) => State::Failed(e),
            };
        }
        let w = ui.available_width();
        let area = Vec2::new(w, (w * 0.75).clamp(80.0, 320.0));
        match state {
            State::Ready(tex, full) => {
                let t = tex.size_vec2();
                // Fit inside the area, never blown up past the decoded size.
                let scale = (area.x / t.x).min(area.y / t.y).min(1.0);
                let (rect, _) = ui.allocate_exact_size(area, egui::Sense::hover());
                let r = egui::Rect::from_center_size(rect.center(), t * scale);
                ui.painter().image(tex.id(), r, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
                ui.vertical_centered(|ui| ui.label(RichText::new(format!("{} × {}", full[0], full[1])).weak()));
            }
            State::Loading(_) => {
                ui.allocate_ui(area, |ui| {
                    ui.centered_and_justified(|ui| ui.spinner());
                });
            }
            State::Failed(e) => {
                ui.allocate_ui(area, |ui| {
                    ui.centered_and_justified(|ui| ui.label(RichText::new(format!("미리 볼 수 없습니다\n{e}")).weak()));
                });
            }
        }
    }
}
