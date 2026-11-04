//! The dalzip window: start screen, archive browser, compress / extract
//! dialogs, password prompt and a progress view. Layout follows Bandizip's
//! (big tiles on a blue start screen, toolbar + folder tree + file list,
//! "새로 압축" dialog with file list / name / folder / options).

use eframe::egui;
use crate::icons::{self, Icon};
use crate::jobs::{self, After, Format, Job, Outcome, Request};
use crate::register;
use crate::settings::{self, Settings, MENU};
use dalzip::format::Kind;
use dalzip::util::{fmt_time_at, human};
use dalzip::{is_encrypted, is_iso, is_zip, list_iso, list_zip, Archive, IsoListing, Target, ZipListing};
use egui::{Align2, Color32, CornerRadius, FontId, RichText, Sense, Stroke, Vec2};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const ACCENT: Color32 = Color32::from_rgb(30, 136, 229);
const BLUE_TOP: Color32 = Color32::from_rgb(66, 150, 230);
const BLUE_BOTTOM: Color32 = Color32::from_rgb(18, 110, 210);

/// What the window was asked to do on the command line.
pub enum Launch {
    Start,
    Open(PathBuf),
    Compose(Vec<PathBuf>),
    Quick(Request),
    ExtractDialog(Vec<PathBuf>),
    Settings,
}

enum Screen {
    Start,
    Settings(SettingsUi),
    Browse(Box<Browser>),
    Compose(Compose),
    ExtractDlg(ExtractDlg),
    Password(PasswordPrompt),
    Running(Job),
    Finished { title: String, text: String, open: Option<PathBuf>, ok: bool, retry: Option<(Request, After)> },
}

pub struct App {
    screen: Screen,
    tz_offset: i64,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, launch: Launch, tz_offset: i64) -> App {
        let screen = match launch {
            Launch::Start => Screen::Start,
            Launch::Open(p) => open_archive(p, None),
            Launch::Compose(files) => Screen::Compose(Compose::new(files)),
            Launch::ExtractDialog(archives) => Screen::ExtractDlg(ExtractDlg::new(archives, None, None)),
            Launch::Quick(req) => start_req(req, &cc.egui_ctx, After::CLOSE),
            Launch::Settings => Screen::Settings(SettingsUi::new()),
        };
        App { screen, tz_offset }
    }
}

fn open_archive(path: PathBuf, password: Option<String>) -> Screen {
    if is_iso(&path) {
        return match list_iso(&path) {
            Ok(z) => Screen::Browse(Box::new(Browser::from_iso(z))),
            Err(e) => failed("열기 실패", &e),
        };
    }
    // ZIP names are never encrypted, so a ZIP opens without a password; extracting asks.
    if is_zip(&path) {
        return match list_zip(&path) {
            Ok(z) => Screen::Browse(Box::new(Browser::from_zip(z))),
            Err(e) => failed("열기 실패", &e),
        };
    }
    if password.is_none() && is_encrypted(&path).unwrap_or(false) {
        return Screen::Password(PasswordPrompt::new(Pending::Open(path)));
    }
    match Archive::open(&path, password.as_deref()) {
        Ok(ar) => Screen::Browse(Box::new(Browser::from_dz(path, &ar, password))),
        Err(e) => failed("열기 실패", &e),
    }
}

/// Whether working on this archive needs a password first.
fn needs_password(path: &Path) -> bool {
    if is_iso(path) {
        false
    } else if is_zip(path) {
        list_zip(path).is_ok_and(|z| z.encrypted)
    } else {
        is_encrypted(path).unwrap_or(false)
    }
}

/// Starts `req`, asking for a password first when an input archive needs one.
fn start_req(req: Request, ctx: &egui::Context, after: After) -> Screen {
    let needs_pw = match &req {
        Request::Extract { archives, password: None, .. } | Request::Test { archives, password: None } => {
            archives.iter().any(|a| needs_password(a))
        }
        _ => false,
    };
    if needs_pw {
        return Screen::Password(PasswordPrompt::new(Pending::Run(req, after)));
    }
    Screen::Running(jobs::spawn(req, ctx.clone(), after))
}

fn failed(title: &str, e: &anyhow::Error) -> Screen {
    Screen::Finished { title: title.to_string(), text: format!("{e:#}"), open: None, ok: false, retry: None }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
        let mut next: Option<Screen> = None;

        // Dropping files: archives open, anything else goes to a new archive (like Bandizip's start screen).
        if !dropped.is_empty() && matches!(self.screen, Screen::Start | Screen::Browse(_) | Screen::Compose(_)) {
            if let Screen::Compose(c) = &mut self.screen {
                c.add(dropped.clone());
            } else if dropped.len() == 1 && is_archive(&dropped[0]) {
                next = Some(open_archive(dropped[0].clone(), None));
            } else {
                next = Some(Screen::Compose(Compose::new(dropped)));
            }
        }

        let tz = self.tz_offset;
        match &mut self.screen {
            Screen::Start => {
                if let Some(a) = start_screen(ui) {
                    next = Some(match a {
                        StartAction::Open => match pick_archive() {
                            Some(p) => open_archive(p, None),
                            None => Screen::Start,
                        },
                        StartAction::New => Screen::Compose(Compose::new(Vec::new())),
                        StartAction::Settings => Screen::Settings(SettingsUi::new()),
                    });
                }
            }
            Screen::Settings(s) => {
                if s.ui(ui) {
                    next = Some(Screen::Start);
                }
            }
            Screen::Browse(b) => match b.ui(ui, tz) {
                Some(BrowseAction::Open) => {
                    if let Some(p) = pick_archive() {
                        next = Some(open_archive(p, None));
                    }
                }
                Some(BrowseAction::New) => next = Some(Screen::Compose(Compose::new(Vec::new()))),
                Some(BrowseAction::Run(req, after)) => next = Some(start_req(req, &ctx, after)),
                Some(BrowseAction::Extract) => next = Some(Screen::ExtractDlg(ExtractDlg::new(vec![b.path.clone()], b.password.clone(), Some(b.path.clone())))),
                None => {}
            },
            Screen::Compose(c) => {
                if let Some(req) = c.ui(ui) {
                    next = Some(start_req(req, &ctx, After::default()));
                }
            }
            Screen::ExtractDlg(d) => match d.ui(ui) {
                Some(DlgAction::Start(req, after)) => next = Some(start_req(req, &ctx, after)),
                // Back to the archive it came from, or gone if the shell opened the dialog.
                Some(DlgAction::Cancel) => match d.back.take() {
                    Some(p) => next = Some(open_archive(p, d.password.clone())),
                    None => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                },
                None => {}
            },
            Screen::Password(p) => match p.ui(ui) {
                Some(Some((pending, pw))) => {
                    next = Some(match pending {
                        Pending::Open(path) => open_archive(path, Some(pw)),
                        Pending::Run(mut req, after) => {
                            match &mut req {
                                Request::Extract { password, .. } | Request::Test { password, .. } => *password = Some(pw),
                                _ => {}
                            }
                            Screen::Running(jobs::spawn(req, ctx.clone(), after))
                        }
                    })
                }
                Some(None) => next = Some(Screen::Start),
                None => {}
            },
            Screen::Running(job) => {
                if let Some(s) = running_ui(ui, job) {
                    if let Screen::Finished { ok: true, open, .. } = &s {
                        if let (true, Some(dir)) = (job.after.open_folder, open) {
                            let _ = std::process::Command::new("explorer.exe").arg(dir).spawn();
                        }
                        if job.after.close {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    }
                    next = Some(s);
                } else {
                    ctx.request_repaint_after(std::time::Duration::from_millis(100));
                }
            }
            Screen::Finished { title, text, open, ok, retry } => match finished_ui(ui, title, text, open.as_deref(), *ok, retry.is_some()) {
                Some(FinishAction::Close) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                Some(FinishAction::Home) => next = Some(Screen::Start),
                Some(FinishAction::Overwrite) => {
                    if let Some((Request::Extract { archives, base, target, password, delete_archives, .. }, after)) = retry.take() {
                        let req = Request::Extract { archives, base, target, overwrite: true, password, delete_archives };
                        next = Some(Screen::Running(jobs::spawn(req, ctx.clone(), after)));
                    }
                }
                None => {}
            },
        }
        if let Some(s) = next {
            self.screen = s;
        }
    }
}

/// Light page background for the dialog-style screens.
fn page_frame() -> egui::Frame {
    egui::Frame::new().inner_margin(20.0).fill(Color32::from_gray(250))
}

/// Formats dalzip can open: its own .dz, ZIP and disc images.
fn is_archive(p: &Path) -> bool {
    p.extension().is_some_and(|e| e.eq_ignore_ascii_case("dz")) || is_zip(p) || is_iso(p)
}

fn pick_archive() -> Option<PathBuf> {
    rfd::FileDialog::new().add_filter("압축 파일 (*.dz, *.zip, *.iso)", &["dz", "zip", "iso"]).pick_file()
}

// ---------------------------------------------------------------- start screen

enum StartAction {
    Open,
    New,
    Settings,
}

fn start_screen(ui: &mut egui::Ui) -> Option<StartAction> {
    let rect = ui.max_rect();
    let painter = ui.painter();
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(rect.left_top(), BLUE_TOP);
    mesh.colored_vertex(rect.right_top(), BLUE_TOP);
    mesh.colored_vertex(rect.right_bottom(), BLUE_BOTTOM);
    mesh.colored_vertex(rect.left_bottom(), BLUE_BOTTOM);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    painter.add(egui::Shape::mesh(mesh));

    let band = egui::Rect::from_min_max(egui::pos2(rect.left(), rect.bottom() - 44.0), rect.right_bottom());
    painter.rect_filled(band, CornerRadius::ZERO, Color32::from_rgba_unmultiplied(10, 40, 90, 170));
    painter.text(
        band.center(),
        Align2::CENTER_CENTER,
        "중복 제거 + 체인 LZMA2 + XChaCha20 암호  ·  파일을 끌어다 놓아도 됩니다",
        FontId::proportional(15.0),
        Color32::from_gray(235),
    );

    let tile = Vec2::new(200.0, 170.0);
    let gap = 6.0;
    let c = rect.center() - Vec2::new(0.0, 22.0);
    let left = egui::Rect::from_min_size(egui::pos2(c.x - tile.x - gap / 2.0, c.y - tile.y / 2.0), tile);
    let right = egui::Rect::from_min_size(egui::pos2(c.x + gap / 2.0, c.y - tile.y / 2.0), tile);
    let mut action = None;
    if tile_button(ui, left, Icon::Open, "압축 파일 열기") {
        action = Some(StartAction::Open);
    }
    if tile_button(ui, right, Icon::New, "새로 압축하기") {
        action = Some(StartAction::New);
    }
    let t = egui::Rect::from_min_size(rect.left_top() + Vec2::new(28.0, 22.0), Vec2::new(300.0, 40.0));
    ui.painter().text(t.left_top(), Align2::LEFT_TOP, "dalzip", FontId::proportional(30.0), Color32::WHITE);
    let gear = egui::Rect::from_min_size(egui::pos2(rect.right() - 168.0, rect.top() + 22.0), Vec2::new(140.0, 34.0));
    let r = ui.allocate_rect(gear, Sense::click());
    let fill = if r.hovered() { Color32::from_white_alpha(60) } else { Color32::from_white_alpha(30) };
    ui.painter().rect_filled(gear, CornerRadius::same(4), fill);
    ui.painter().text(gear.center(), Align2::CENTER_CENTER, "탐색기 메뉴 설정", FontId::proportional(15.0), Color32::WHITE);
    if r.clicked() {
        action = Some(StartAction::Settings);
    }
    action
}

fn tile_button(ui: &mut egui::Ui, rect: egui::Rect, icon: Icon, label: &str) -> bool {
    let resp = ui.allocate_rect(rect, Sense::click());
    let fill = if resp.hovered() { Color32::from_rgb(232, 242, 253) } else { Color32::WHITE };
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(2), fill);
    let ir = egui::Rect::from_center_size(rect.center() - Vec2::new(0.0, 22.0), Vec2::splat(64.0));
    icons::draw(p, ir, icon, Color32::from_gray(90));
    p.text(rect.center() + Vec2::new(0.0, 45.0), Align2::CENTER_CENTER, label, FontId::proportional(18.0), Color32::from_gray(30));
    resp.clicked()
}

// ---------------------------------------------------------------- browser

/// One row of an archive listing, whatever the format.
struct Item {
    path: String,
    is_dir: bool,
    size: u64,
    mtime: i64,
}

#[derive(Default)]
struct Node {
    dirs: BTreeMap<String, Node>,
    files: Vec<usize>,
}

impl Node {
    fn build(items: &[Item]) -> Node {
        let mut root = Node::default();
        for (i, e) in items.iter().enumerate() {
            let parts: Vec<&str> = e.path.split('/').collect();
            let (dirs, last) = if e.is_dir { (&parts[..], None) } else { (&parts[..parts.len() - 1], Some(i)) };
            let mut n = &mut root;
            for d in dirs {
                n = n.dirs.entry((*d).to_string()).or_default();
            }
            if let Some(i) = last {
                n.files.push(i);
            }
        }
        root
    }

    fn at(&self, path: &[String]) -> &Node {
        let mut n = self;
        for p in path {
            match n.dirs.get(p) {
                Some(c) => n = c,
                None => break,
            }
        }
        n
    }
}

enum BrowseAction {
    Open,
    New,
    Run(Request, After),
    /// Open the extract dialog.
    Extract,
}

struct Browser {
    path: PathBuf,
    password: Option<String>,
    items: Vec<Item>,
    tree: Node,
    cwd: Vec<String>,
    selected: Option<String>,
    files: usize,
    dirs: usize,
    raw: u64,
    size: u64,
    encrypted: bool,
    /// .dz only: bytes left to compress after dedup.
    unique: Option<u64>,
    /// Disc images only: the file system that was read.
    image: Option<&'static str>,
    /// Tree folders the user expanded.
    expanded: std::collections::HashSet<String>,
}

impl Browser {
    fn from_dz(path: PathBuf, ar: &Archive, password: Option<String>) -> Browser {
        let idx = &ar.index;
        let items = idx.entries.iter().map(|e| Item { path: e.path.clone(), is_dir: e.kind == Kind::Dir, size: e.size, mtime: e.mtime }).collect();
        let unique = idx.blocks.iter().map(|b| b.raw_len).sum();
        Browser::new(path, items, ar.size, ar.encrypted(), Some(unique), password)
    }

    fn from_zip(z: ZipListing) -> Browser {
        let items = z.entries.iter().map(|e| Item { path: e.path.clone(), is_dir: e.is_dir, size: e.size, mtime: e.mtime }).collect();
        Browser::new(z.path, items, z.size, z.encrypted, None, None)
    }

    fn from_iso(z: IsoListing) -> Browser {
        let items = z.entries.iter().map(|e| Item { path: e.path.clone(), is_dir: e.is_dir, size: e.size, mtime: e.mtime }).collect();
        let mut b = Browser::new(z.path, items, z.size, false, None, None);
        b.image = Some(z.filesystem);
        b
    }

    fn new(path: PathBuf, items: Vec<Item>, size: u64, encrypted: bool, unique: Option<u64>, password: Option<String>) -> Browser {
        let tree = Node::build(&items);
        let files = items.iter().filter(|e| !e.is_dir).count();
        // Zips often list folders only implicitly, so count them from the tree.
        fn count(n: &Node) -> usize {
            n.dirs.values().map(|c| 1 + count(c)).sum()
        }
        let dirs = count(&tree);
        let raw = items.iter().map(|e| e.size).sum();
        // Like Bandizip's enterFolderWhenRootFolderIsOnlyOne: step into a lone root folder.
        let mut cwd = Vec::new();
        if tree.files.is_empty() && tree.dirs.len() == 1 {
            cwd.push(tree.dirs.keys().next().cloned().unwrap_or_default());
        }
        Browser { path, password, items, tree, cwd, selected: None, files, dirs, raw, size, encrypted, unique, image: None, expanded: Default::default() }
    }

    fn ui(&mut self, ui: &mut egui::Ui, tz: i64) -> Option<BrowseAction> {
        let mut action = None;
        egui::Panel::top("toolbar").frame(egui::Frame::new().fill(Color32::from_rgb(38, 132, 226)).inner_margin(egui::Margin::symmetric(14, 6))).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 14.0;
                // Bandizip's order: open, extract, new, then the rest.
                if tool(ui, Icon::Open, "열기") {
                    action = Some(BrowseAction::Open);
                }
                if tool(ui, Icon::Extract, "풀기") {
                    action = Some(BrowseAction::Extract);
                }
                if tool(ui, Icon::New, "새로 압축") {
                    action = Some(BrowseAction::New);
                }
                if tool(ui, Icon::Auto, "알아서 풀기") {
                    action = Some(BrowseAction::Run(self.extract_req(None, Target::Auto), Settings::load().after_extract()));
                }
                if tool(ui, Icon::Test, "테스트") {
                    let req = Request::Test { archives: vec![self.path.clone()], password: self.password.clone() };
                    action = Some(BrowseAction::Run(req, After::default()));
                }
            });
        });

        egui::Panel::bottom("status").frame(egui::Frame::new().fill(Color32::from_gray(242)).inner_margin(6.0)).show(ui, |ui| {
            // Bandizip shows space saved; so do we, plus what dedup left to compress.
            let saved = if self.raw > 0 { 100.0 - self.size as f64 * 100.0 / self.raw as f64 } else { 0.0 };
            let detail = match (self.image, self.unique) {
                (Some(fs), _) => format!("디스크 이미지, {fs})  ·  파일 합계 {}", human(self.raw)),
                (None, Some(u)) => format!("솔리드, {saved:.1}% 절약)  ·  원본 {}, 중복 제거 후 {}", human(self.raw), human(u)),
                (None, None) => format!("ZIP, {saved:.1}% 절약)  ·  원본 {}", human(self.raw)),
            };
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(format!(
                    "파일: {}, 폴더: {}, 압축 파일 크기: {} ({detail}{}",
                    self.files,
                    self.dirs,
                    human(self.size),
                    if self.encrypted { "  ·  암호화됨" } else { "" }
                ));
                if self.encrypted {
                    icons::inline(ui, Icon::Lock);
                }
            });
        });

        // The panel's width is the user's: rows cut long names with "…" instead of widening it.
        egui::Panel::left("tree").resizable(true).default_size(240.0).size_range(120.0..=640.0).show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                let name = self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                if list_row(ui, 0.0, None, Icon::Archive, &name, self.cwd.is_empty()).clicked() {
                    self.cwd.clear();
                    self.selected = None;
                }
                let mut goto = None;
                tree_rows(ui, &self.tree, &mut Vec::new(), &self.cwd, &mut self.expanded, &mut goto);
                if let Some(g) = goto {
                    self.cwd = g;
                    self.selected = None;
                }
            });
        });

        egui::CentralPanel::default().show(ui, |ui| {
            // Malgun Gothic draws '\' as '₩', so the breadcrumb uses its own separator.
            let name = self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let crumbs: Vec<&str> = std::iter::once(name.as_str()).chain(self.cwd.iter().map(String::as_str)).collect();
            ui.add(egui::Label::new(RichText::new(crumbs.join("  ›  ")).strong()).truncate());
            ui.separator();

            enum Row<'a> {
                Up,
                Dir(&'a str),
                File(usize),
            }
            let node = self.tree.at(&self.cwd);
            let mut rows: Vec<Row> = Vec::new();
            if !self.cwd.is_empty() {
                rows.push(Row::Up);
            }
            rows.extend(node.dirs.keys().map(|d| Row::Dir(d.as_str())));
            rows.extend(node.files.iter().map(|&i| Row::File(i)));
            let items = &self.items;
            let file_name = |i: usize| items[i].path.rsplit('/').next().unwrap_or(&items[i].path).to_string();

            let mut clicked: Option<String> = None;
            let mut enter: Option<String> = None;
            let mut up = false;
            let gray = Color32::from_gray(110);
            // Bandizip-like list: resizable columns, the name column takes the rest and clips, whole rows select.
            egui_extras::TableBuilder::new(ui)
                .striped(true)
                .resizable(true)
                .sense(Sense::click())
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(egui_extras::Column::remainder().at_least(140.0).clip(true))
                .column(egui_extras::Column::initial(90.0).at_least(60.0).clip(true))
                .column(egui_extras::Column::initial(70.0).at_least(44.0).clip(true))
                .column(egui_extras::Column::initial(140.0).at_least(90.0).clip(true))
                .header(26.0, |mut h| {
                    for t in ["이름", "크기", "종류", "수정한 날짜"] {
                        h.col(|ui| {
                            ui.strong(t);
                        });
                    }
                })
                .body(|body| {
                    body.rows(26.0, rows.len(), |mut row| {
                        let (key, icon, text) = match rows[row.index()] {
                            Row::Up => (String::new(), Icon::Folder, "..".to_string()),
                            Row::Dir(d) => (d.to_string(), Icon::Folder, d.to_string()),
                            Row::File(i) => (file_name(i), Icon::File, file_name(i)),
                        };
                        row.set_selected(!key.is_empty() && self.selected.as_deref() == Some(key.as_str()));
                        row.col(|ui| {
                            let (r, _) = ui.allocate_exact_size(Vec2::new(20.0, 18.0), Sense::hover());
                            icons::draw(ui.painter(), r.shrink(1.0), icon, gray);
                            ui.add(egui::Label::new(text).truncate().selectable(false));
                        });
                        match rows[row.index()] {
                            Row::File(i) => {
                                let e = &items[i];
                                row.col(|ui| {
                                    ui.label(human(e.size));
                                });
                                row.col(|ui| {
                                    ui.label(key.rsplit_once('.').map(|(_, x)| x.to_uppercase()).unwrap_or_default());
                                });
                                row.col(|ui| {
                                    ui.label(fmt_time_at(e.mtime, tz));
                                });
                            }
                            _ => {
                                row.col(|_| {});
                                row.col(|ui| {
                                    if !key.is_empty() {
                                        ui.label("폴더");
                                    }
                                });
                                row.col(|_| {});
                            }
                        }
                        let r = row.response();
                        if r.clicked() {
                            clicked = Some(key.clone());
                        }
                        if r.double_clicked() {
                            match rows[row.index()] {
                                Row::Up => up = true,
                                Row::Dir(d) => enter = Some(d.to_string()),
                                Row::File(_) => {}
                            }
                        }
                    });
                });
            if let Some(k) = clicked {
                self.selected = (!k.is_empty()).then_some(k);
            }
            if up {
                self.cwd.pop();
                self.selected = None;
            }
            if let Some(d) = enter {
                self.cwd.push(d);
                self.selected = None;
            }
        });
        action
    }

    fn extract_req(&self, base: Option<PathBuf>, target: Target) -> Request {
        Request::Extract {
            archives: vec![self.path.clone()],
            base,
            target,
            overwrite: false,
            password: self.password.clone(),
            delete_archives: false,
        }
    }
}

/// One full-width row: optional expander, icon, and text cut with "…" so a
/// long name never widens its panel. Selection and hover fill the whole row.
fn list_row(ui: &mut egui::Ui, indent: f32, expander: Option<bool>, icon: Icon, text: &str, selected: bool) -> egui::Response {
    let h = 26.0;
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width().max(40.0), h), Sense::click());
    let v = ui.visuals();
    let p = ui.painter();
    if selected {
        p.rect_filled(rect, CornerRadius::same(3), v.selection.bg_fill);
    } else if resp.hovered() {
        p.rect_filled(rect, CornerRadius::same(3), v.widgets.hovered.weak_bg_fill);
    }
    let mut x = rect.left() + 4.0 + indent;
    if let Some(open) = expander {
        let c = egui::pos2(x + 6.0, rect.center().y);
        let pts = if open {
            vec![c + Vec2::new(-4.0, -2.0), c + Vec2::new(4.0, -2.0), c + Vec2::new(0.0, 3.0)]
        } else {
            vec![c + Vec2::new(-2.0, -4.0), c + Vec2::new(3.0, 0.0), c + Vec2::new(-2.0, 4.0)]
        };
        p.add(egui::Shape::convex_polygon(pts, Color32::from_gray(90), Stroke::NONE));
    }
    x += 14.0;
    icons::draw(p, egui::Rect::from_center_size(egui::pos2(x + 8.0, rect.center().y), Vec2::splat(16.0)), icon, Color32::from_gray(110));
    x += 22.0;
    let galley = egui::WidgetText::from(text).into_galley(ui, Some(egui::TextWrapMode::Truncate), (rect.right() - x - 4.0).max(10.0), egui::TextStyle::Body);
    let color = if selected { v.selection.stroke.color } else { v.text_color() };
    ui.painter().galley(egui::pos2(x, rect.center().y - galley.size().y / 2.0), galley, color);
    resp.on_hover_text(text)
}

/// Folder tree rows; a folder's children show when it is expanded, is the
/// current folder, or holds it.
fn tree_rows(
    ui: &mut egui::Ui,
    node: &Node,
    path: &mut Vec<String>,
    cwd: &[String],
    expanded: &mut std::collections::HashSet<String>,
    goto: &mut Option<Vec<String>>,
) {
    for (name, child) in &node.dirs {
        path.push(name.clone());
        let key = path.join("/");
        let has_children = !child.dirs.is_empty();
        let open = has_children && (expanded.contains(&key) || cwd.starts_with(path));
        let indent = 14.0 * (path.len() - 1) as f32;
        let r = list_row(ui, indent, has_children.then_some(open), Icon::Folder, name, path.as_slice() == cwd);
        if r.clicked() {
            // A click on the arrow toggles; anywhere else opens the folder.
            let on_arrow = r.interact_pointer_pos().is_some_and(|p| p.x < r.rect.left() + 4.0 + indent + 16.0);
            if on_arrow && has_children {
                if open {
                    expanded.remove(&key);
                } else {
                    expanded.insert(key.clone());
                }
            } else {
                *goto = Some(path.clone());
            }
        }
        if open {
            tree_rows(ui, child, path, cwd, expanded, goto);
        }
        path.pop();
    }
}
/// A Bandizip-style toolbar button: white line icon over a label, on the blue bar.
fn tool(ui: &mut egui::Ui, icon: Icon, label: &str) -> bool {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(92.0, 96.0), Sense::click());
    let p = ui.painter();
    if resp.hovered() {
        p.rect_filled(rect, CornerRadius::same(3), Color32::from_white_alpha(40));
    }
    let ir = egui::Rect::from_center_size(rect.center_top() + Vec2::new(0.0, 36.0), Vec2::splat(48.0));
    icons::draw(p, ir, icon, Color32::WHITE);
    p.text(rect.center_bottom() - Vec2::new(0.0, 14.0), Align2::CENTER_CENTER, label, FontId::proportional(15.0), Color32::WHITE);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
}

// ---------------------------------------------------------------- compose

const LEVELS: [&str; 10] = [
    "저장 (압축 안 함)",
    "zstd 빠름 — zip보다 빠르고 작게",
    "zstd 보통",
    "zstd 최대 — 푸는 속도 가장 빠름",
    "LZMA2 빠름",
    "LZMA2 보통 (기본)",
    "LZMA2 높음",
    "블록마다 최적 코덱 (느림)",
    "블록마다 최적 코덱, 큰 블록",
    "최고 압축 (아주 느림)",
];

/// ZIP levels; 10 and 11 measured on a 118 MiB mixed corpus (see README).
const ZIP_LEVELS: [&str; 12] = [
    "저장 (압축 안 함)",
    "Deflate 1 — 가장 빠름",
    "Deflate 2",
    "Deflate 3",
    "Deflate 4",
    "Deflate 5",
    "Deflate 6 — 빠른 zip 기본",
    "Deflate 7",
    "Deflate 8",
    "Deflate 9",
    "최대 (libdeflate) — 7-Zip 최대보다 작고 빠름",
    "극한 (Zopfli) — 조금 더 작지만 아주 느림",
];

struct Compose {
    files: Vec<PathBuf>,
    selected: Option<usize>,
    name: String,
    folder: String,
    level: u8,
    use_password: bool,
    password: String,
    confirm: String,
    dedup: bool,
    test: bool,
    named: bool,
    format: Format,
}

impl Compose {
    fn new(files: Vec<PathBuf>) -> Compose {
        let mut c = Compose {
            files: Vec::new(),
            selected: None,
            name: String::new(),
            folder: String::new(),
            format: Format::Dz,
            level: 5,
            use_password: false,
            password: String::new(),
            confirm: String::new(),
            dedup: true,
            test: false,
            named: false,
        };
        c.add(files);
        c
    }

    fn add(&mut self, files: Vec<PathBuf>) {
        for f in files {
            if !self.files.contains(&f) {
                self.files.push(f);
            }
        }
        // Suggest a name until the user types one, like Bandizip does.
        if !self.named {
            if let Ok(p) = jobs::archive_name_for(&self.files, self.format.ext()) {
                self.name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                self.folder = p.parent().map(|d| d.display().to_string()).unwrap_or_default();
            }
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui) -> Option<Request> {
        let mut start = None;
        egui::CentralPanel::default().frame(page_frame()).show(ui, |ui| {
            ui.heading("새로 압축");
            ui.add_space(8.0);
            ui.label(RichText::new("압축할 파일 목록").strong());
            egui::Frame::new().stroke(Stroke::new(1.0, Color32::from_gray(210))).fill(Color32::WHITE).inner_margin(6.0).show(ui, |ui| {
                egui::ScrollArea::vertical().max_height(150.0).auto_shrink([false, true]).show(ui, |ui| {
                    if self.files.is_empty() {
                        ui.label(RichText::new("파일이나 폴더를 여기로 끌어다 놓거나 [추가]를 누르세요").weak());
                    }
                    for (i, f) in self.files.iter().enumerate() {
                        let icon = if f.is_dir() { Icon::Folder } else { Icon::File };
                        let r = ui.horizontal(|ui| {
                            icons::inline(ui, icon);
                            ui.selectable_label(self.selected == Some(i), f.display().to_string())
                        });
                        if r.inner.clicked() {
                            self.selected = Some(i);
                        }
                    }
                });
            });
            ui.horizontal(|ui| {
                if ui.button("파일 추가").clicked() {
                    if let Some(f) = rfd::FileDialog::new().pick_files() {
                        self.add(f);
                    }
                }
                if ui.button("폴더 추가").clicked() {
                    if let Some(f) = rfd::FileDialog::new().pick_folders() {
                        self.add(f);
                    }
                }
                if ui.add_enabled(self.selected.is_some(), egui::Button::new("제거")).clicked() {
                    if let Some(i) = self.selected.take() {
                        self.files.remove(i);
                    }
                }
            });
            ui.add_space(10.0);
            ui.label(RichText::new("압축 파일 설정").strong());
            egui::Grid::new("compose").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                ui.label("파일 이름");
                if ui.add(egui::TextEdit::singleline(&mut self.name).desired_width(420.0)).changed() {
                    self.named = true;
                }
                ui.end_row();
                ui.label("저장 폴더");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.folder).desired_width(340.0));
                    if ui.button("찾아보기...").clicked() {
                        if let Some(d) = rfd::FileDialog::new().pick_folder() {
                            self.folder = d.display().to_string();
                        }
                    }
                });
                ui.end_row();
                ui.label("압축 형식");
                ui.horizontal(|ui| {
                    let before = self.format;
                    ui.radio_value(&mut self.format, Format::Dz, "DZ (작게, dalzip 필요)");
                    ui.radio_value(&mut self.format, Format::Zip, "ZIP (어디서나 열림)");
                    if before != self.format {
                        self.level = if self.format == Format::Zip { 6 } else { 5 };
                        if let Some((stem, _)) = self.name.rsplit_once('.') {
                            self.name = format!("{stem}.{}", self.format.ext());
                        }
                    }
                });
                ui.end_row();
                ui.label("압축 레벨");
                ui.horizontal(|ui| {
                    let max = if self.format == Format::Zip { 11 } else { 9 };
                    self.level = self.level.min(max);
                    ui.add(egui::Slider::new(&mut self.level, 0..=max).show_value(true));
                    match self.format {
                        Format::Dz => ui.label(LEVELS[self.level as usize]),
                        Format::Zip => ui.label(ZIP_LEVELS[self.level as usize]),
                    };
                });
                ui.end_row();
                let dz = self.format == Format::Dz;
                ui.label("");
                ui.add_enabled(dz, egui::Checkbox::new(&mut self.dedup, "중복 제거 (같은 내용은 한 번만 저장)"));
                ui.end_row();
                ui.label("");
                ui.add_enabled(dz, egui::Checkbox::new(&mut self.test, "압축 후 테스트"));
                ui.end_row();
                ui.label("");
                ui.add_enabled(dz, egui::Checkbox::new(&mut self.use_password, "암호 설정 (파일 이름까지 암호화)"));
                ui.end_row();
                if !dz {
                    self.use_password = false;
                    self.test = false;
                }
                if self.use_password {
                    ui.label("암호");
                    ui.add(egui::TextEdit::singleline(&mut self.password).password(true).desired_width(260.0));
                    ui.end_row();
                    ui.label("암호 확인");
                    ui.add(egui::TextEdit::singleline(&mut self.confirm).password(true).desired_width(260.0));
                    ui.end_row();
                }
            });
            ui.add_space(14.0);
            let pw_ok = !self.use_password || (!self.password.is_empty() && self.password == self.confirm);
            let ready = !self.files.is_empty() && !self.name.trim().is_empty() && !self.folder.trim().is_empty() && pw_ok;
            ui.horizontal(|ui| {
                let b = egui::Button::new(RichText::new("압축 시작").size(16.0).color(Color32::WHITE)).fill(ACCENT).min_size(Vec2::new(140.0, 36.0));
                if ui.add_enabled(ready, b).clicked() {
                    let mut name = self.name.trim().to_string();
                    let ext = format!(".{}", self.format.ext());
                    if !name.to_ascii_lowercase().ends_with(&ext) {
                        name.push_str(&ext);
                    }
                    start = Some(Request::Create {
                        archive: Path::new(self.folder.trim()).join(name),
                        inputs: self.files.clone(),
                        format: self.format,
                        level: self.level,
                        password: self.use_password.then(|| self.password.clone()),
                        dedup: self.dedup,
                        test: self.test,
                        delete_sources: false,
                    });
                }
                if self.use_password && !pw_ok {
                    ui.label(RichText::new("암호가 비어 있거나 확인과 다릅니다").color(Color32::from_rgb(200, 60, 40)));
                }
            });
        });
        start
    }
}

// ---------------------------------------------------------------- extract dialog

/// Bandizip's "압축 풀 폴더 선택", with Windows' own folder picker instead of a
/// built-in tree, plus shortcuts to common places.
struct ExtractDlg {
    archives: Vec<PathBuf>,
    folder: String,
    target: Target,
    overwrite: bool,
    delete: bool,
    close: bool,
    open: bool,
    remember: bool,
    password: Option<String>,
    /// The archive window to go back to on cancel; `None` = launched by the shell.
    back: Option<PathBuf>,
}

enum DlgAction {
    Start(Request, After),
    Cancel,
}

fn target_from(n: u32) -> Target {
    match n {
        1 => Target::Name,
        2 => Target::Here,
        _ => Target::Auto,
    }
}

fn target_to(t: Target) -> u32 {
    match t {
        Target::Auto => 0,
        Target::Name => 1,
        Target::Here => 2,
    }
}

/// A known shell folder (follows OneDrive redirection of Desktop/Documents).
fn known_folder(id: &windows_sys::core::GUID) -> Option<PathBuf> {
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::SHGetKnownFolderPath;
    let mut p: windows_sys::core::PWSTR = std::ptr::null_mut();
    if unsafe { SHGetKnownFolderPath(id, 0, std::ptr::null_mut(), &mut p) } != 0 || p.is_null() {
        return None;
    }
    let len = (0..).take_while(|&i| unsafe { *p.add(i) } != 0).count();
    let s = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, len) });
    unsafe { CoTaskMemFree(p as *const _) };
    Some(PathBuf::from(s))
}

impl ExtractDlg {
    fn new(archives: Vec<PathBuf>, password: Option<String>, back: Option<PathBuf>) -> ExtractDlg {
        let folder = archives.first().and_then(|a| a.parent()).map(|d| d.display().to_string()).unwrap_or_default();
        let s = Settings::load();
        let (target, delete) = if s.extract_remember { (target_from(s.extract_target), s.extract_delete) } else { (Target::Auto, false) };
        ExtractDlg {
            archives,
            folder,
            target,
            overwrite: false,
            delete,
            close: s.extract_close,
            open: s.extract_open,
            remember: s.extract_remember,
            password,
            back,
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui) -> Option<DlgAction> {
        let mut act = None;
        egui::Panel::bottom("extract_buttons").frame(egui::Frame::new().fill(Color32::from_gray(242)).inner_margin(12.0)).show(ui, |ui| {
            ui.horizontal(|ui| {
                let ok = egui::Button::new(RichText::new("압축 풀기").size(16.0).color(Color32::WHITE)).fill(ACCENT).min_size(Vec2::new(130.0, 34.0));
                if ui.add_enabled(!self.folder.trim().is_empty(), ok).clicked() {
                    if self.remember {
                        let mut s = Settings::load();
                        s.extract_target = target_to(self.target);
                        s.extract_delete = self.delete;
                        s.extract_close = self.close;
                        s.extract_open = self.open;
                        s.extract_remember = true;
                        let _ = s.save();
                    } else {
                        let mut s = Settings::load();
                        s.extract_remember = false;
                        let _ = s.save();
                    }
                    let req = Request::Extract {
                        archives: self.archives.clone(),
                        base: Some(PathBuf::from(self.folder.trim())),
                        target: self.target,
                        overwrite: self.overwrite,
                        password: self.password.clone(),
                        delete_archives: self.delete,
                    };
                    act = Some(DlgAction::Start(req, After { close: self.close, open_folder: self.open }));
                }
                if ui.add(egui::Button::new("취소").min_size(Vec2::new(90.0, 34.0))).clicked() {
                    act = Some(DlgAction::Cancel);
                }
            });
        });
        egui::CentralPanel::default().frame(page_frame()).show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                ui.heading("압축 풀기");
                ui.add_space(6.0);
                for a in &self.archives {
                    ui.horizontal(|ui| {
                        icons::inline(ui, Icon::Archive);
                        ui.add(egui::Label::new(a.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()).truncate());
                    });
                }
                ui.add_space(10.0);
                ui.label(RichText::new("대상 폴더").strong());
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.folder).desired_width((ui.available_width() - 110.0).max(200.0)));
                    if ui.button("찾아보기...").clicked() {
                        let start = PathBuf::from(self.folder.trim());
                        let mut dlg = rfd::FileDialog::new();
                        if start.is_dir() {
                            dlg = dlg.set_directory(&start);
                        }
                        if let Some(d) = dlg.pick_folder() {
                            self.folder = d.display().to_string();
                        }
                    }
                });
                // Bandizip's left column of places, as one row of shortcuts.
                ui.horizontal_wrapped(|ui| {
                    use windows_sys::Win32::UI::Shell::{FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads};
                    let mut places: Vec<(&str, Option<PathBuf>)> = vec![("압축 파일 위치", self.archives.first().and_then(|a| a.parent()).map(Path::to_path_buf))];
                    places.push(("바탕 화면", known_folder(&FOLDERID_Desktop)));
                    places.push(("문서", known_folder(&FOLDERID_Documents)));
                    places.push(("다운로드", known_folder(&FOLDERID_Downloads)));
                    for (label, path) in places {
                        if let Some(p) = path {
                            if ui.small_button(label).on_hover_text(p.display().to_string()).clicked() {
                                self.folder = p.display().to_string();
                            }
                        }
                    }
                });
                ui.add_space(10.0);
                ui.label(RichText::new("풀 위치").strong());
                ui.radio_value(&mut self.target, Target::Auto, "알아서 풀기 (최상위 항목이 하나면 그대로, 여러 개면 '압축 파일명' 폴더)");
                ui.radio_value(&mut self.target, Target::Name, "대상 폴더의 하위에 '압축 파일명'으로 폴더 생성 후 압축 풀기");
                ui.radio_value(&mut self.target, Target::Here, "대상 폴더에 바로 풀기");
                ui.add_space(8.0);
                ui.label(RichText::new("옵션").strong());
                ui.checkbox(&mut self.overwrite, "같은 이름의 파일이 있으면 덮어쓰기");
                ui.checkbox(&mut self.delete, "압축 풀기 성공 후 압축 파일 삭제 (휴지통으로, ISO는 검증할 수 없어 제외)");
                ui.checkbox(&mut self.close, "압축 풀기가 끝나면 dalzip 닫기");
                ui.checkbox(&mut self.open, "압축 풀기가 끝나면 풀린 폴더 열기");
                ui.checkbox(&mut self.remember, "설정 기억");
            });
        });
        act
    }
}

// ---------------------------------------------------------------- password

enum Pending {
    Open(PathBuf),
    Run(Request, After),
}

struct PasswordPrompt {
    pending: Option<Pending>,
    text: String,
}

impl PasswordPrompt {
    fn new(pending: Pending) -> PasswordPrompt {
        PasswordPrompt { pending: Some(pending), text: String::new() }
    }

    /// `Some(Some(..))` = entered, `Some(None)` = cancelled.
    fn ui(&mut self, ui: &mut egui::Ui) -> Option<Option<(Pending, String)>> {
        let mut out = None;
        egui::CentralPanel::default().frame(page_frame()).show(ui, |ui| {
            ui.horizontal(|ui| {
                icons::inline(ui, Icon::Lock);
                ui.heading("암호 입력");
            });
            ui.label("이 압축 파일은 암호화되어 있습니다. 파일 이름도 암호를 알아야 볼 수 있습니다.");
            ui.add_space(10.0);
            let r = ui.add(egui::TextEdit::singleline(&mut self.text).password(true).desired_width(300.0));
            r.request_focus();
            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if (ui.button("확인").clicked() || enter) && !self.text.is_empty() {
                    if let Some(p) = self.pending.take() {
                        out = Some(Some((p, std::mem::take(&mut self.text))));
                    }
                }
                if ui.button("취소").clicked() {
                    out = Some(None);
                }
            });
        });
        out
    }
}

// ---------------------------------------------------------------- progress / result

fn running_ui(ui: &mut egui::Ui, job: &Job) -> Option<Screen> {
    if let Some(res) = job.result.lock().unwrap().take() {
        return Some(match res {
            Ok(Outcome::Created { archives, summary }) => Screen::Finished {
                title: "압축 완료".into(),
                text: format!("{}\n\n{summary}", archives.iter().map(|a| a.display().to_string()).collect::<Vec<_>>().join("\n")),
                open: archives.first().and_then(|a| a.parent()).map(Path::to_path_buf),
                ok: true,
                retry: None,
            },
            Ok(Outcome::Extracted { dests, summary }) => Screen::Finished {
                title: "압축 풀기 완료".into(),
                text: format!("{}\n\n{summary}", dests.iter().map(|d| d.display().to_string()).collect::<Vec<_>>().join("\n")),
                open: dests.first().cloned(),
                ok: true,
                retry: None,
            },
            Ok(Outcome::Tested { summary }) => Screen::Finished { title: "테스트 완료".into(), text: summary, open: None, ok: true, retry: None },
            Err(e) if dalzip::progress::is_cancelled(&e) => {
                Screen::Finished { title: "취소됨".into(), text: "작업을 중지했습니다. 만들던 파일은 지웠습니다.".into(), open: None, ok: false, retry: None }
            }
            Err(e) => {
                let clash = format!("{e:#}").contains("would overwrite");
                Screen::Finished {
                    title: if clash { "이미 있는 파일".into() } else { "오류".into() },
                    text: format!("{e:#}"),
                    open: None,
                    ok: false,
                    retry: if clash { job.retry.clone().map(|r| (r, job.after)) } else { None },
                }
            }
        });
    }
    let (done, total, current) = job.progress.snapshot();
    let frac = if total > 0 { done as f32 / total as f32 } else { 0.0 };
    let secs = job.started.elapsed().as_secs_f64().max(0.001);
    let speed = done as f64 / secs;
    egui::CentralPanel::default().frame(page_frame()).show(ui, |ui| {
        ui.heading(&job.title);
        ui.add_space(8.0);
        ui.label(RichText::new(current).weak());
        ui.add_space(8.0);
        ui.add(egui::ProgressBar::new(frac).show_percentage().desired_height(22.0));
        ui.add_space(6.0);
        let eta = if speed > 0.0 && total > done { format!(", 남은 시간 약 {:.0}초", (total - done) as f64 / speed) } else { String::new() };
        ui.label(format!("{} / {}  ·  {}/s  ·  {:.0}초 경과{eta}", human(done), human(total), human(speed as u64), secs));
        ui.add_space(14.0);
        if ui.button("중지").clicked() {
            job.progress.cancel();
        }
    });
    None
}

enum FinishAction {
    Close,
    Home,
    Overwrite,
}

fn finished_ui(ui: &mut egui::Ui, title: &str, text: &str, open: Option<&Path>, ok: bool, can_overwrite: bool) -> Option<FinishAction> {
    let mut act = None;
    egui::CentralPanel::default().frame(page_frame()).show(ui, |ui| {
        let color = if ok { Color32::from_rgb(46, 125, 50) } else { Color32::from_rgb(198, 40, 40) };
        ui.heading(RichText::new(title).color(color));
        ui.add_space(10.0);
        egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
            ui.label(RichText::new(text).monospace());
        });
        ui.add_space(14.0);
        ui.horizontal(|ui| {
            if let Some(dir) = open {
                if ui.button("폴더 열기").clicked() {
                    let _ = std::process::Command::new("explorer.exe").arg(dir).spawn();
                }
            }
            if can_overwrite && ui.button("덮어쓰고 다시 풀기").clicked() {
                act = Some(FinishAction::Overwrite);
            }
            if ui.button("처음으로").clicked() {
                act = Some(FinishAction::Home);
            }
            if ui.button("닫기").clicked() {
                act = Some(FinishAction::Close);
            }
        });
    });
    act
}

// ---------------------------------------------------------------- settings

/// Bandizip's 설정 > 탐색기 메뉴 page: which items, flat or cascaded,
/// double-click action and the dated name rule. "적용" saves and re-registers.
pub struct SettingsUi {
    s: Settings,
    status: Option<(bool, String)>,
}

impl SettingsUi {
    pub fn new() -> SettingsUi {
        SettingsUi { s: Settings::load(), status: None }
    }

    /// Returns true to go back to the start screen.
    fn ui(&mut self, ui: &mut egui::Ui) -> bool {
        let back = self.buttons(ui);
        egui::CentralPanel::default().frame(page_frame()).show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                ui.heading("탐색기 메뉴 설정");
                ui.add_space(6.0);
                let groups = [
                    (settings::Group::Archives, "압축 파일(.dz)을 선택할 때"),
                    (settings::Group::Sources, "파일·폴더를 선택할 때 (압축하기)"),
                    (settings::Group::Folder, "폴더나 폴더 바닥을 선택할 때"),
                ];
                ui.columns(2, |cols| {
                    for (k, (g, title)) in groups.iter().enumerate() {
                        let ui = &mut cols[if k == 1 { 1 } else { 0 }];
                        ui.add_space(6.0);
                        ui.label(RichText::new(*title).strong());
                        for (i, (_, label, group, _)) in MENU.iter().enumerate() {
                            if group == g {
                                ui.checkbox(&mut self.s.enabled[i], *label);
                            }
                        }
                    }
                });
                ui.add_space(10.0);
                ui.label(RichText::new("파일명 규칙 (\"파일명에 규칙을 넣어 압축하기\")").strong());
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.s.rule).desired_width(360.0));
                    if ui.button("기본값").clicked() {
                        self.s.rule = settings::DEFAULT_RULE.into();
                    }
                });
                ui.label(RichText::new("${NAME}: 파일명  %Y 년 %m 월 %d 일 %H 시 %M 분 %S 초").weak());
                ui.label(format!("예: {}.dz", settings::expand_rule(&self.s.rule, "보고서")));
                ui.add_space(10.0);
                ui.label(RichText::new("압축 파일(.dz)을 더블 클릭할 때").strong());
                ui.radio_value(&mut self.s.double_click, 0, "dalzip으로 열기");
                ui.radio_value(&mut self.s.double_click, 1, "압축 파일이 있는 폴더에 알아서 풀기");
                ui.radio_value(&mut self.s.double_click, 2, "압축 파일명 폴더에 풀기");
                ui.add_space(10.0);
                ui.label(RichText::new("dalzip 창에서 압축 풀기가 끝나면").strong());
                ui.checkbox(&mut self.s.extract_close, "dalzip 닫기");
                ui.checkbox(&mut self.s.extract_open, "풀린 폴더 열기");
                ui.add_space(10.0);
                ui.label(RichText::new("기타").strong());
                ui.checkbox(&mut self.s.cascade, "탐색기 메뉴를 계단식(하위 메뉴 \"dalzip\")으로 보여주기");
                ui.label(RichText::new("Windows 11에서는 \"더 많은 옵션 표시\"(Shift+F10) 메뉴에 나옵니다.").weak());
            });
        });
        back
    }

    /// Buttons stay visible below the scrolling page.
    fn buttons(&mut self, ui: &mut egui::Ui) -> bool {
        let mut back = false;
        egui::Panel::bottom("settings_buttons").frame(egui::Frame::new().fill(Color32::from_gray(242)).inner_margin(12.0)).show(ui, |ui| {
            ui.horizontal(|ui| {
                let apply = egui::Button::new(RichText::new("적용").size(16.0).color(Color32::WHITE)).fill(ACCENT).min_size(Vec2::new(110.0, 34.0));
                if ui.add(apply).clicked() {
                    let dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)).unwrap_or_default();
                    self.status = Some(match self.s.save().and_then(|_| register::register(&dir)) {
                        Ok(m) => (true, format!("적용했습니다 — {m}")),
                        Err(e) => (false, format!("{e:#}")),
                    });
                }
                if ui.button("탐색기 메뉴 모두 제거").clicked() {
                    self.status = Some(match register::unregister() {
                        Ok(m) => (true, m),
                        Err(e) => (false, format!("{e:#}")),
                    });
                }
                if ui.button("뒤로").clicked() {
                    back = true;
                }
                if let Some((ok, msg)) = &self.status {
                    let color = if *ok { Color32::from_rgb(46, 125, 50) } else { Color32::from_rgb(198, 40, 40) };
                    ui.label(RichText::new(msg).color(color));
                }
            });
        });
        back
    }
}
