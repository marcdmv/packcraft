//! PackCraft desktop app: a file-manager style archiver window.
//!
//! Browse folders, open .pcraft archives like folders, add files, extract,
//! test and inspect. All archive work runs on a background thread.

#![cfg_attr(windows, windows_subsystem = "windows")]

mod icons;

use eframe::egui::{
    self, Align, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, Id, Key, Layout, Modal, RichText, Sense, Vec2,
};
use egui_extras::{Column, TableBuilder};
use icons::Icon;
use packcraft::archive::{self, ListItem, Options, Route};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const ACCENT: Color32 = Color32::from_rgb(0xE0, 0x7A, 0x2E);
const ACCENT_RGB: [u8; 3] = [0xE0, 0x7A, 0x2E];
const EXT: &str = "pcraft";

fn main() -> eframe::Result {
    let start = std::env::args_os().nth(1).map(PathBuf::from);
    let icon = egui::IconData { rgba: icons::window_icon(64, ACCENT_RGB), width: 64, height: 64 };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("PackCraft")
            .with_inner_size([960.0, 620.0])
            .with_min_inner_size([560.0, 360.0])
            .with_drag_and_drop(true)
            .with_icon(Arc::new(icon)),
        ..Default::default()
    };
    eframe::run_native(
        "PackCraft",
        options,
        Box::new(move |cc| {
            install_font(&cc.egui_ctx);
            cc.egui_ctx.all_styles_mut(|s| {
                s.visuals.selection.bg_fill = ACCENT.linear_multiply(0.55);
                s.visuals.hyperlink_color = ACCENT;
                s.spacing.item_spacing = Vec2::new(8.0, 5.0);
            });
            Ok(Box::new(App::new(start)))
        }),
    )
}

fn install_font(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::empty();
    fonts.font_data.insert("inter".into(), Arc::new(FontData::from_static(include_bytes!("../../assets/fonts/Inter.ttf"))));
    for fam in [FontFamily::Proportional, FontFamily::Monospace] {
        fonts.families.entry(fam).or_default().push("inter".into());
    }
    ctx.set_fonts(fonts);
}

// ---------------------------------------------------------------- model

enum Location {
    Folder(PathBuf),
    Archive { file: PathBuf, items: Vec<ListItem>, prefix: String },
}

#[derive(Clone)]
struct Row {
    name: String,
    is_dir: bool,
    is_archive: bool,
    size: u64,
    files: usize,
    kind: String,
    modified: Option<SystemTime>,
}

#[derive(Clone, Copy, PartialEq)]
enum SortBy {
    Name,
    Size,
    Kind,
    Modified,
}

enum Dialog {
    None,
    Add { inputs: Vec<PathBuf>, archive: String, fast: bool, verify: bool },
    Extract { archive: PathBuf, only: Option<Vec<String>>, dest: String, open_after: bool },
    Message { title: String, body: String, error: bool },
    About,
}

struct Job {
    label: String,
    started: Instant,
    rx: mpsc::Receiver<JobDone>,
}

enum JobDone {
    Created { archive: PathBuf, report: Result<archive::Report, String> },
    Extracted { dest: PathBuf, result: Result<(usize, u64), String>, open_after: bool },
    Tested { archive: PathBuf, result: Result<(usize, u64), String> },
    Viewed { result: Result<PathBuf, String> },
}

enum Action {
    Navigate(PathBuf),
    OpenRow(usize),
    Up,
    Add,
    Extract,
    Test,
    View,
    Info,
    AddPaths(Vec<PathBuf>),
}

struct App {
    loc: Location,
    address: String,
    rows: Vec<Row>,
    selected: BTreeSet<usize>,
    anchor: Option<usize>,
    sort: (SortBy, bool),
    show_hidden: bool,
    threads: usize,
    dialog: Dialog,
    job: Option<Job>,
    status: String,
    /// The egui context, refreshed every frame, for commands that start a job.
    ctx_hint: Option<egui::Context>,
}

impl App {
    fn new(start: Option<PathBuf>) -> Self {
        let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from);
        let folder = home.or_else(|| std::env::current_dir().ok()).unwrap_or_else(|| PathBuf::from("/"));
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(8);
        let mut app = App {
            loc: Location::Folder(folder.clone()),
            address: String::new(),
            rows: Vec::new(),
            selected: BTreeSet::new(),
            anchor: None,
            sort: (SortBy::Name, true),
            show_hidden: false,
            threads,
            dialog: Dialog::None,
            job: None,
            status: String::new(),
            ctx_hint: None,
        };
        app.navigate(start.unwrap_or(folder));
        app
    }

    // ------------------------------------------------------------ navigation

    fn navigate(&mut self, path: PathBuf) {
        let path = if path.is_relative() { self.current_folder().join(path) } else { path };
        if path.is_dir() {
            self.loc = Location::Folder(path);
        } else if path.is_file() && is_archive(&path) {
            match archive::list(&path) {
                Ok(items) => self.loc = Location::Archive { file: path, items, prefix: String::new() },
                Err(e) => return self.message("Cannot open archive", e, true),
            }
        } else {
            return self.message("Not found", format!("{} is not a folder or a .{EXT} archive.", path.display()), true);
        }
        self.reload();
    }

    fn reload(&mut self) {
        self.selected.clear();
        self.anchor = None;
        self.rows = match &self.loc {
            Location::Folder(dir) => {
                self.address = dir.display().to_string();
                folder_rows(dir, self.show_hidden).unwrap_or_else(|e| {
                    self.status = format!("{}: {e}", dir.display());
                    Vec::new()
                })
            }
            Location::Archive { file, items, prefix } => {
                self.address = format!("{}{}{}", file.display(), std::path::MAIN_SEPARATOR, prefix.replace('/', std::path::MAIN_SEPARATOR_STR));
                archive_rows(items, prefix)
            }
        };
        self.sort_rows();
    }

    fn sort_rows(&mut self) {
        let (by, asc) = self.sort;
        self.rows.sort_by(|a, b| {
            let ord = match by {
                SortBy::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                SortBy::Size => a.size.cmp(&b.size),
                SortBy::Kind => a.kind.cmp(&b.kind),
                SortBy::Modified => a.modified.cmp(&b.modified),
            };
            b.is_dir.cmp(&a.is_dir).then(if asc { ord } else { ord.reverse() })
        });
        self.selected.clear();
    }

    fn current_folder(&self) -> PathBuf {
        match &self.loc {
            Location::Folder(d) => d.clone(),
            Location::Archive { file, .. } => file.parent().map(Path::to_path_buf).unwrap_or_default(),
        }
    }

    fn up(&mut self) {
        match &mut self.loc {
            Location::Folder(d) => {
                if let Some(p) = d.parent() {
                    let p = p.to_path_buf();
                    self.navigate(p);
                }
            }
            Location::Archive { file, prefix, .. } => {
                if prefix.is_empty() {
                    let dir = file.parent().map(Path::to_path_buf).unwrap_or_default();
                    self.navigate(dir);
                } else {
                    let trimmed = prefix.trim_end_matches('/');
                    *prefix = trimmed.rfind('/').map(|i| trimmed[..=i].to_string()).unwrap_or_default();
                    self.reload();
                }
            }
        }
    }

    fn open_row(&mut self, i: usize) {
        let Some(row) = self.rows.get(i).cloned() else { return };
        match &mut self.loc {
            Location::Folder(d) => {
                let p = d.join(&row.name);
                if row.is_dir || row.is_archive {
                    self.navigate(p);
                } else {
                    open_with_system(&p);
                }
            }
            Location::Archive { prefix, .. } => {
                if row.is_dir {
                    prefix.push_str(&row.name);
                    prefix.push('/');
                    self.reload();
                } else {
                    self.view_selected();
                }
            }
        }
    }

    // ------------------------------------------------------------ selection helpers

    /// Full paths (folder mode) of the selected rows.
    fn selected_paths(&self) -> Vec<PathBuf> {
        let dir = self.current_folder();
        self.selected.iter().filter_map(|&i| self.rows.get(i)).map(|r| dir.join(&r.name)).collect()
    }

    /// Archive entry paths covered by the selection (folders expand to their contents).
    fn selected_entries(&self) -> Option<Vec<String>> {
        let Location::Archive { items, prefix, .. } = &self.loc else { return None };
        if self.selected.is_empty() {
            return None;
        }
        let mut out = Vec::new();
        for &i in &self.selected {
            let r = &self.rows[i];
            let full = format!("{prefix}{}", r.name);
            if r.is_dir {
                let pre = format!("{full}/");
                out.extend(items.iter().filter(|it| it.path.starts_with(&pre)).map(|it| it.path.clone()));
            } else {
                out.push(full);
            }
        }
        Some(out)
    }

    fn message(&mut self, title: &str, body: impl Into<String>, error: bool) {
        self.dialog = Dialog::Message { title: title.into(), body: body.into(), error };
    }

    // ------------------------------------------------------------ commands

    fn add_dialog(&mut self, inputs: Vec<PathBuf>) {
        if inputs.is_empty() {
            return self.message("Add to archive", "Select the files or folders to add first.", false);
        }
        let parent = inputs[0].parent().map(Path::to_path_buf).unwrap_or_else(|| self.current_folder());
        let base = if inputs.len() == 1 {
            inputs[0].file_stem().map(|s| s.to_string_lossy().into_owned())
        } else {
            parent.file_name().map(|s| s.to_string_lossy().into_owned())
        }
        .unwrap_or_else(|| "archive".into());
        let archive = parent.join(format!("{base}.{EXT}")).display().to_string();
        self.dialog = Dialog::Add { inputs, archive, fast: false, verify: true };
    }

    fn extract_dialog(&mut self) {
        let (archive, only) = match &self.loc {
            Location::Archive { file, .. } => (file.clone(), self.selected_entries()),
            Location::Folder(_) => {
                let picked: Vec<PathBuf> = self.selected_paths().into_iter().filter(|p| is_archive(p)).collect();
                match picked.as_slice() {
                    [one] => (one.clone(), None),
                    [] => return self.message("Extract", format!("Select a .{EXT} archive, or open one first."), false),
                    _ => return self.message("Extract", "Select one archive at a time.", false),
                }
            }
        };
        let stem = archive.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "extracted".into());
        let dest = archive.parent().unwrap_or(Path::new(".")).join(stem).display().to_string();
        self.dialog = Dialog::Extract { archive, only, dest, open_after: true };
    }

    fn target_archive(&self) -> Option<PathBuf> {
        match &self.loc {
            Location::Archive { file, .. } => Some(file.clone()),
            Location::Folder(_) => self.selected_paths().into_iter().find(|p| is_archive(p)),
        }
    }

    fn test_archive(&mut self, ctx: &egui::Context) {
        let Some(archive) = self.target_archive() else {
            return self.message("Test", format!("Select a .{EXT} archive, or open one first."), false);
        };
        let threads = self.threads;
        let a2 = archive.clone();
        self.spawn(ctx, format!("Testing {}", file_name(&archive)), move || JobDone::Tested {
            result: archive::extract(&a2, None, threads),
            archive: a2,
        });
    }

    fn view_selected(&mut self) {
        let Location::Archive { file, .. } = &self.loc else {
            for p in self.selected_paths() {
                open_with_system(&p);
            }
            return;
        };
        let Some(entries) = self.selected_entries().filter(|e| e.len() == 1) else {
            return self.message("View", "Select one file inside the archive.", false);
        };
        let entry = entries[0].clone();
        let archive = file.clone();
        let threads = self.threads;
        let dir = std::env::temp_dir().join(format!("packcraft-view-{}", std::process::id()));
        let ctx = self.ctx_hint.clone();
        if let Some(ctx) = ctx {
            self.spawn(&ctx, format!("Opening {entry}"), move || {
                let keep = entry.clone();
                let result = archive::extract_filtered(&archive, Some(&dir), threads, &move |p| p == keep)
                    .map(|_| dir.join(entry.replace('/', std::path::MAIN_SEPARATOR_STR)));
                JobDone::Viewed { result }
            });
        }
    }

    fn info(&mut self) {
        match &self.loc {
            Location::Archive { file, items, .. } => {
                let packed = std::fs::metadata(file).map(|m| m.len()).unwrap_or(0);
                let raw: u64 = items.iter().map(|i| i.size).sum();
                let mut by_route: Vec<(Route, usize, u64)> = Vec::new();
                for it in items {
                    match by_route.iter_mut().find(|(r, _, _)| *r == it.route) {
                        Some(e) => {
                            e.1 += 1;
                            e.2 += it.size;
                        }
                        None => by_route.push((it.route, 1, it.size)),
                    }
                }
                let mut body = format!(
                    "{}\n\n{} files, {} unpacked\nArchive size {} ({:.1}% of original)\n\nMethods:\n",
                    file.display(),
                    items.len(),
                    human(raw),
                    human(packed),
                    if raw > 0 { 100.0 * packed as f64 / raw as f64 } else { 0.0 }
                );
                for (r, n, b) in by_route {
                    body.push_str(&format!("  {:<16} {n} files, {}\n", r.name(), human(b)));
                }
                self.message("Archive information", body, false);
            }
            Location::Folder(_) => {
                let paths = self.selected_paths();
                if paths.is_empty() {
                    return self.message("Information", "Select files or folders first.", false);
                }
                let (mut files, mut bytes) = (0usize, 0u64);
                for p in &paths {
                    tally(p, &mut files, &mut bytes);
                }
                self.message("Information", format!("{} selected items\n{files} files, {}", paths.len(), human(bytes)), false);
            }
        }
    }

    fn spawn(&mut self, ctx: &egui::Context, label: String, work: impl FnOnce() -> JobDone + Send + 'static) {
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(work());
            ctx.request_repaint();
        });
        self.job = Some(Job { label, started: Instant::now(), rx });
    }

    fn poll_job(&mut self) {
        let Some(job) = &self.job else { return };
        let Ok(done) = job.rx.try_recv() else { return };
        let secs = job.started.elapsed().as_secs_f64();
        self.job = None;
        match done {
            JobDone::Created { archive, report } => match report {
                Ok(r) => {
                    let mut body = format!(
                        "{}\n\n{} -> {} ({:.2}%) in {secs:.1} s\n",
                        archive.display(),
                        human(r.input_bytes),
                        human(r.archive_bytes),
                        100.0 * r.archive_bytes as f64 / r.input_bytes.max(1) as f64
                    );
                    for (route, st) in &r.stats {
                        body.push_str(&format!("\n  {:<16} {} files  {} -> {}", route.name(), st.files, human(st.raw), human(st.packed)));
                    }
                    if !r.skipped.is_empty() {
                        body.push_str(&format!("\n\nSkipped (not a file or link): {}", r.skipped.join(", ")));
                    }
                    self.status = format!("Created {} in {secs:.1} s", file_name(&archive));
                    self.reload();
                    self.message("Archive created", body, false);
                }
                Err(e) => self.message("Could not create the archive", e, true),
            },
            JobDone::Extracted { dest, result, open_after } => match result {
                Ok((n, bytes)) => {
                    self.status = format!("Extracted {n} files ({}) to {} in {secs:.1} s", human(bytes), dest.display());
                    if open_after {
                        open_with_system(&dest);
                    }
                }
                Err(e) => self.message("Extraction failed", e, true),
            },
            JobDone::Tested { archive, result } => match result {
                Ok((n, bytes)) => self.message(
                    "Test passed",
                    format!("{}\n\nAll {n} files ({}) decoded and every CRC matches ({secs:.1} s).", archive.display(), human(bytes)),
                    false,
                ),
                Err(e) => self.message("Test FAILED", format!("{}\n\n{e}", archive.display()), true),
            },
            JobDone::Viewed { result } => match result {
                Ok(p) => open_with_system(&p),
                Err(e) => self.message("Cannot open the file", e, true),
            },
        }
    }
}

// ---------------------------------------------------------------- UI

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.ctx_hint = Some(ctx.clone());
        self.poll_job();
        let mut actions: Vec<Action> = Vec::new();

        // Files dropped from the OS: an archive opens, anything else goes to "Add".
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).collect());
        if !dropped.is_empty() && self.job.is_none() {
            if dropped.len() == 1 && is_archive(&dropped[0]) {
                actions.push(Action::Navigate(dropped[0].clone()));
            } else {
                actions.push(Action::AddPaths(dropped));
            }
        }

        let busy = self.job.is_some();
        let in_archive = matches!(self.loc, Location::Archive { .. });

        egui::Panel::top("menu").resizable(false).show(ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("Go to parent folder").clicked() {
                        actions.push(Action::Up);
                    }
                    ui.separator();
                    if ui.button("Quit").clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
                ui.menu_button("Commands", |ui| {
                    for (label, a) in [("Add to archive...", Action::Add), ("Extract to...", Action::Extract), ("Test archive", Action::Test), ("View file", Action::View), ("Information", Action::Info)] {
                        if ui.add_enabled(!busy, egui::Button::new(label)).clicked() {
                            actions.push(a);
                        }
                    }
                });
                ui.menu_button("Options", |ui| {
                    if ui.checkbox(&mut self.show_hidden, "Show hidden files").changed() {
                        self.reload();
                    }
                    ui.horizontal(|ui| {
                        ui.label("Threads");
                        ui.add(egui::DragValue::new(&mut self.threads).range(1..=64));
                    });
                    ui.separator();
                    egui::widgets::global_theme_preference_buttons(ui);
                });
                ui.menu_button("Help", |ui| {
                    if ui.button("About PackCraft").clicked() {
                        self.dialog = Dialog::About;
                    }
                });
            });
        });

        egui::Panel::top("toolbar").resizable(false).show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let can_add = !busy && !in_archive;
                if tool_button(ui, Icon::Add, "Add", can_add)
                    .on_hover_text(if in_archive { "Archives are read-only in this version: create a new one" } else { "Add the selection to a new archive" })
                    .clicked()
                {
                    actions.push(Action::Add);
                }
                if tool_button(ui, Icon::Extract, "Extract To", !busy).on_hover_text("Extract the archive or the selected entries").clicked() {
                    actions.push(Action::Extract);
                }
                if tool_button(ui, Icon::Test, "Test", !busy).on_hover_text("Decode everything and check every CRC").clicked() {
                    actions.push(Action::Test);
                }
                if tool_button(ui, Icon::View, "View", !busy).on_hover_text("Open with the default app").clicked() {
                    actions.push(Action::View);
                }
                if tool_button(ui, Icon::Info, "Info", true).clicked() {
                    actions.push(Action::Info);
                }
            });
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                let (rect, up) = ui.allocate_exact_size(Vec2::new(28.0, 24.0), Sense::click());
                if up.hovered() {
                    ui.painter().rect_filled(rect, CornerRadius::same(4), ui.visuals().widgets.hovered.weak_bg_fill);
                }
                icons::paint(ui.painter(), rect.shrink(3.0), Icon::Up, ui.visuals().text_color(), ACCENT);
                if up.on_hover_text("Up one level (Backspace)").clicked() {
                    actions.push(Action::Up);
                }
                let resp = ui.add_sized([ui.available_width(), 24.0], egui::TextEdit::singleline(&mut self.address));
                if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    actions.push(Action::Navigate(PathBuf::from(self.address.trim())));
                }
            });
            ui.add_space(4.0);
        });

        egui::Panel::bottom("status").show(ui, |ui| {
            ui.horizontal(|ui| {
                let sel_bytes: u64 = self.selected.iter().filter_map(|&i| self.rows.get(i)).map(|r| r.size).sum();
                let total: u64 = self.rows.iter().map(|r| r.size).sum();
                if self.selected.is_empty() {
                    ui.label(format!("{} items, {}", self.rows.len(), human(total)));
                } else {
                    ui.label(format!("Selected {} of {} items, {}", self.selected.len(), self.rows.len(), human(sel_bytes)));
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(RichText::new(&self.status).weak());
                });
            });
        });

        egui::CentralPanel::default().show(ui, |ui| {
            self.file_table(ui, &mut actions);
        });

        // Keyboard, only when no text field has focus.
        if !busy && ctx.memory(|m| m.focused().is_none()) && matches!(self.dialog, Dialog::None) {
            ctx.input(|i| {
                if i.key_pressed(Key::Backspace) {
                    actions.push(Action::Up);
                }
                if i.key_pressed(Key::Enter) {
                    if let Some(&s) = self.selected.iter().next() {
                        actions.push(Action::OpenRow(s));
                    }
                }
            });
            if ctx.input(|i| i.modifiers.command && i.key_pressed(Key::A)) {
                self.selected = (0..self.rows.len()).collect();
            }
        }

        self.dialogs(&ctx);
        self.busy_overlay(&ctx);

        for a in actions {
            match a {
                Action::Navigate(p) => self.navigate(p),
                Action::OpenRow(i) => self.open_row(i),
                Action::Up => self.up(),
                Action::Add => {
                    let s = self.selected_paths();
                    self.add_dialog(s)
                }
                Action::AddPaths(p) => self.add_dialog(p),
                Action::Extract => self.extract_dialog(),
                Action::Test => self.test_archive(&ctx),
                Action::View => self.view_selected(),
                Action::Info => self.info(),
            }
        }
    }
}

impl App {
    fn file_table(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        let in_archive = matches!(self.loc, Location::Archive { .. });
        let headers: [(&str, SortBy); 4] = [
            ("Name", SortBy::Name),
            ("Size", SortBy::Size),
            (if in_archive { "Method" } else { "Type" }, SortBy::Kind),
            (if in_archive { "Files" } else { "Modified (UTC)" }, SortBy::Modified),
        ];
        let mut resort = None;
        let row_h = 22.0;
        let fg = ui.visuals().text_color();
        TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .sense(Sense::click())
            .cell_layout(Layout::left_to_right(Align::Center))
            .column(Column::remainder().at_least(220.0).clip(true))
            .column(Column::initial(96.0).at_least(60.0))
            .column(Column::initial(130.0).at_least(60.0))
            .column(Column::initial(150.0).at_least(60.0))
            .header(24.0, |mut header| {
                for (label, by) in headers {
                    header.col(|ui| {
                        let arrow = if self.sort.0 == by { if self.sort.1 { " ↑" } else { " ↓" } } else { "" };
                        if ui.add(egui::Label::new(RichText::new(format!("{label}{arrow}")).strong()).sense(Sense::click())).clicked() {
                            resort = Some(by);
                        }
                    });
                }
            })
            .body(|body| {
                body.rows(row_h, self.rows.len(), |mut row| {
                    let i = row.index();
                    row.set_selected(self.selected.contains(&i));
                    let r = &self.rows[i];
                    row.col(|ui| {
                        let (rect, _) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::hover());
                        let icon = if r.is_dir { Icon::Folder } else if r.is_archive { Icon::Archive } else { Icon::File };
                        icons::paint(ui.painter(), rect, icon, fg, ACCENT);
                        ui.label(&r.name);
                    });
                    row.col(|ui| {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.label(if r.is_dir && !in_archive { String::new() } else { human(r.size) });
                        });
                    });
                    row.col(|ui| {
                        ui.label(&r.kind);
                    });
                    row.col(|ui| {
                        if in_archive {
                            ui.label(if r.is_dir { r.files.to_string() } else { String::new() });
                        } else {
                            ui.label(r.modified.map(format_time).unwrap_or_default());
                        }
                    });
                    let resp = row.response();
                    if resp.double_clicked() {
                        actions.push(Action::OpenRow(i));
                    } else if resp.clicked() {
                        let m = resp.ctx.input(|inp| inp.modifiers);
                        if m.shift {
                            let a = self.anchor.unwrap_or(i);
                            self.selected = (a.min(i)..=a.max(i)).collect();
                        } else if m.command {
                            if !self.selected.remove(&i) {
                                self.selected.insert(i);
                            }
                            self.anchor = Some(i);
                        } else {
                            self.selected = BTreeSet::from([i]);
                            self.anchor = Some(i);
                        }
                    }
                });
            });
        if let Some(by) = resort {
            self.sort = if self.sort.0 == by { (by, !self.sort.1) } else { (by, true) };
            self.sort_rows();
        }
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        let mut close = false;
        let mut start: Option<Box<dyn FnOnce(&mut App)>> = None;
        match &mut self.dialog {
            Dialog::None => {}
            Dialog::Message { title, body, error } => {
                let resp = Modal::new(Id::unique("packcraft-message")).show(ctx, |ui| {
                    ui.set_max_width(520.0);
                    let t = RichText::new(title.as_str()).heading();
                    ui.label(if *error { t.color(ui.visuals().error_fg_color) } else { t });
                    ui.add_space(6.0);
                    egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                        ui.label(RichText::new(body.as_str()).monospace());
                    });
                    ui.add_space(8.0);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| ui.button("OK").clicked()).inner
                });
                close = resp.inner || resp.should_close();
            }
            Dialog::About => {
                let resp = Modal::new(Id::unique("packcraft-about")).show(ctx, |ui| {
                    ui.set_max_width(420.0);
                    ui.horizontal(|ui| {
                        let (rect, _) = ui.allocate_exact_size(Vec2::splat(40.0), Sense::hover());
                        icons::crate_mark(ui.painter(), rect, ACCENT, Color32::WHITE);
                        ui.vertical(|ui| {
                            ui.heading("PackCraft");
                            ui.label(format!("version {}", env!("CARGO_PKG_VERSION")));
                        });
                    });
                    ui.add_space(6.0);
                    ui.label("Lossless archiver for creative work. Pure Rust.");
                    ui.label("MIT OR Apache-2.0. Interface font: Inter (SIL OFL 1.1).");
                    ui.hyperlink(env!("CARGO_PKG_REPOSITORY"));
                    ui.add_space(8.0);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| ui.button("Close").clicked()).inner
                });
                close = resp.inner || resp.should_close();
            }
            Dialog::Add { inputs, archive, fast, verify } => {
                let resp = Modal::new(Id::unique("packcraft-add")).show(ctx, |ui| {
                    ui.set_width(520.0);
                    ui.heading("Add to archive");
                    ui.add_space(4.0);
                    ui.label(format!("{} item(s): {}", inputs.len(), inputs.iter().map(|p| file_name(p)).collect::<Vec<_>>().join(", ")));
                    ui.add_space(6.0);
                    ui.label("Archive name");
                    ui.add(egui::TextEdit::singleline(archive).desired_width(f32::INFINITY));
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.label("Compression");
                        ui.radio_value(fast, false, "Best (smallest)");
                        ui.radio_value(fast, true, "Fast (quick to extract)");
                    });
                    ui.checkbox(verify, "Test the archive after creating it");
                    ui.add_space(8.0);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let ok = ui.add(egui::Button::new(RichText::new("Create").strong()).fill(ACCENT.linear_multiply(0.8))).clicked();
                        let cancel = ui.button("Cancel").clicked();
                        (ok, cancel)
                    })
                    .inner
                });
                let (ok, cancel) = resp.inner;
                if ok {
                    let mut path = PathBuf::from(archive.trim());
                    if path.extension().is_none() {
                        path.set_extension(EXT);
                    }
                    let (inputs, fast, verify) = (inputs.clone(), *fast, *verify);
                    start = Some(Box::new(move |app: &mut App| {
                        if path.exists() {
                            return app.message("Archive exists", format!("{} already exists. Choose another name.", path.display()), true);
                        }
                        let opts = Options { threads: app.threads, fast, verify, ..Options::default() };
                        let p2 = path.clone();
                        let ctx = app.ctx_hint.clone().expect("context set every frame");
                        app.spawn(&ctx, format!("Creating {}", file_name(&path)), move || JobDone::Created {
                            report: archive::create(&p2, &inputs, &opts),
                            archive: p2,
                        });
                    }));
                }
                close = ok || cancel || resp.should_close();
            }
            Dialog::Extract { archive, only, dest, open_after } => {
                let resp = Modal::new(Id::unique("packcraft-extract")).show(ctx, |ui| {
                    ui.set_width(520.0);
                    ui.heading("Extract");
                    ui.add_space(4.0);
                    ui.label(match only {
                        Some(e) => format!("{} selected file(s) from {}", e.len(), file_name(archive)),
                        None => format!("All files from {}", file_name(archive)),
                    });
                    ui.add_space(6.0);
                    ui.label("Destination folder");
                    ui.add(egui::TextEdit::singleline(dest).desired_width(f32::INFINITY));
                    ui.horizontal(|ui| {
                        if ui.small_button("Next to the archive").clicked() {
                            if let Some(p) = archive.parent() {
                                *dest = p.display().to_string();
                            }
                        }
                        if ui.small_button("In a folder named after it").clicked() {
                            let stem = archive.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                            *dest = archive.parent().unwrap_or(Path::new(".")).join(stem).display().to_string();
                        }
                    });
                    ui.checkbox(open_after, "Open the folder when done");
                    ui.add_space(8.0);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let ok = ui.add(egui::Button::new(RichText::new("Extract").strong()).fill(ACCENT.linear_multiply(0.8))).clicked();
                        let cancel = ui.button("Cancel").clicked();
                        (ok, cancel)
                    })
                    .inner
                });
                let (ok, cancel) = resp.inner;
                if ok {
                    let (archive, only, dest, open_after) = (archive.clone(), only.clone(), PathBuf::from(dest.trim()), *open_after);
                    start = Some(Box::new(move |app: &mut App| {
                        let threads = app.threads;
                        let ctx = app.ctx_hint.clone().expect("context set every frame");
                        let d2 = dest.clone();
                        app.spawn(&ctx, format!("Extracting {}", file_name(&archive)), move || {
                            let result = match only {
                                Some(list) => {
                                    let set: BTreeSet<String> = list.into_iter().collect();
                                    archive::extract_filtered(&archive, Some(&d2), threads, &move |p| set.contains(p))
                                }
                                None => archive::extract(&archive, Some(&d2), threads),
                            };
                            JobDone::Extracted { dest: d2, result, open_after }
                        });
                    }));
                }
                close = ok || cancel || resp.should_close();
            }
        }
        if close {
            self.dialog = Dialog::None;
        }
        if let Some(f) = start {
            f(self);
        }
    }

    fn busy_overlay(&mut self, ctx: &egui::Context) {
        let Some(job) = &self.job else { return };
        Modal::new(Id::unique("packcraft-busy")).show(ctx, |ui| {
            ui.set_width(360.0);
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(RichText::new(&job.label).strong());
            });
            ui.label(format!("{:.0} s", job.started.elapsed().as_secs_f64()));
        });
        ctx.request_repaint_after(std::time::Duration::from_millis(250));
    }
}

fn tool_button(ui: &mut egui::Ui, icon: Icon, label: &str, enabled: bool) -> egui::Response {
    let size = Vec2::new(70.0, 54.0);
    let (rect, resp) = ui.allocate_exact_size(size, if enabled { Sense::click() } else { Sense::hover() });
    let v = ui.visuals();
    if enabled && resp.hovered() {
        ui.painter().rect_filled(rect, CornerRadius::same(6), v.widgets.hovered.weak_bg_fill);
    }
    let color = if enabled { v.text_color() } else { v.weak_text_color() };
    let accent = if enabled { ACCENT } else { v.weak_text_color() };
    let icon_rect = egui::Rect::from_center_size(rect.center_top() + Vec2::new(0.0, 19.0), Vec2::splat(26.0));
    icons::paint(ui.painter(), icon_rect, icon, color, accent);
    ui.painter().text(rect.center_bottom() - Vec2::new(0.0, 8.0), egui::Align2::CENTER_CENTER, label, egui::FontId::proportional(12.5), color);
    resp
}

// ---------------------------------------------------------------- helpers

fn is_archive(p: &Path) -> bool {
    p.extension().is_some_and(|e| e.eq_ignore_ascii_case(EXT))
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string())
}

fn folder_rows(dir: &Path, hidden: bool) -> std::io::Result<Vec<Row>> {
    let mut rows = Vec::new();
    for item in std::fs::read_dir(dir)? {
        let Ok(item) = item else { continue };
        let name = item.file_name().to_string_lossy().into_owned();
        if !hidden && name.starts_with('.') {
            continue;
        }
        let meta = std::fs::metadata(item.path()).or_else(|_| item.metadata());
        let Ok(meta) = meta else { continue };
        let is_dir = meta.is_dir();
        let path = item.path();
        let is_archive = !is_dir && is_archive(&path);
        let kind = if is_dir {
            "Folder".to_string()
        } else if is_archive {
            "PackCraft archive".to_string()
        } else {
            path.extension().map(|e| format!("{} file", e.to_string_lossy().to_uppercase())).unwrap_or_else(|| "File".into())
        };
        rows.push(Row { name, is_dir, is_archive, size: if is_dir { 0 } else { meta.len() }, files: 0, kind, modified: meta.modified().ok() });
    }
    Ok(rows)
}

fn archive_rows(items: &[ListItem], prefix: &str) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    let mut dirs: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for it in items {
        let Some(rest) = it.path.strip_prefix(prefix) else { continue };
        match rest.split_once('/') {
            Some((dir, _)) => {
                let k = *dirs.entry(dir.to_string()).or_insert_with(|| {
                    rows.push(Row { name: dir.to_string(), is_dir: true, is_archive: false, size: 0, files: 0, kind: "Folder".into(), modified: None });
                    rows.len() - 1
                });
                rows[k].size += it.size;
                rows[k].files += 1;
            }
            None => rows.push(Row {
                name: rest.to_string(),
                is_dir: false,
                is_archive: false,
                size: it.size,
                files: 1,
                kind: it.route.name().to_string(),
                modified: None,
            }),
        }
    }
    rows
}

fn tally(p: &Path, files: &mut usize, bytes: &mut u64) {
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => {
            if let Ok(rd) = std::fs::read_dir(p) {
                for e in rd.flatten() {
                    tally(&e.path(), files, bytes);
                }
            }
        }
        Ok(m) => {
            *files += 1;
            *bytes += m.len();
        }
        Err(_) => {}
    }
}

fn human(b: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1000.0 && u < U.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    if u == 0 { format!("{b} B") } else { format!("{v:.1} {}", U[u]) }
}

/// UTC "YYYY-MM-DD HH:MM" without a date library (days-from-civil inverse).
fn format_time(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", rem / 3600, rem % 3600 / 60)
}

fn open_with_system(p: &Path) {
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open").arg(p).spawn();
    #[cfg(target_os = "windows")]
    let r = std::process::Command::new("explorer").arg(p).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let r = std::process::Command::new("xdg-open").arg(p).spawn();
    if let Err(e) = r {
        eprintln!("cannot open {}: {e}", p.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates() {
        let at = |s: u64| format_time(UNIX_EPOCH + std::time::Duration::from_secs(s));
        assert_eq!(at(0), "1970-01-01 00:00");
        assert_eq!(at(951_782_400), "2000-02-29 00:00");
        assert_eq!(at(1_795_000_000), "2026-11-18 11:06");
    }

    #[test]
    fn archive_listing_groups_folders() {
        let it = |p: &str, s| ListItem { path: p.into(), size: s, route: Route::Cm };
        let items = vec![it("p/a.txt", 3), it("p/sub/b.txt", 4), it("p/sub/c/d.txt", 5), it("top.txt", 1)];
        let root = archive_rows(&items, "");
        assert_eq!(root.len(), 2);
        let p = archive_rows(&items, "p/");
        let sub = p.iter().find(|r| r.name == "sub").unwrap();
        assert!(sub.is_dir && sub.size == 9 && sub.files == 2);
        assert!(p.iter().any(|r| r.name == "a.txt" && !r.is_dir));
    }

    #[test]
    fn human_sizes() {
        assert_eq!(human(999), "999 B");
        assert_eq!(human(1_500_000), "1.5 MB");
    }
}
