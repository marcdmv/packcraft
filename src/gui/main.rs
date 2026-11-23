//! PackCraft desktop app: a file-manager style archiver window.
//!
//! Browse folders, open archives (.pcraft, .zip, .7z, .rar and their parts)
//! like folders, add files, extract, test and inspect. All archive work runs on
//! a background thread.
//!
//! `packcraft-gui <archive-or-folder>` opens it; `packcraft-gui --add <path>...`
//! opens the Add dialog for those paths (used by the Finder and Explorer
//! actions); `--add-default` creates the archive straight away with the
//! remembered settings.

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod finder;
mod icons;

use eframe::egui::{
    self, Align, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, Id, Key, Layout, Modal, RichText, Sense, Vec2,
};
use egui_extras::{Column, TableBuilder};
use icons::Icon;
use packcraft::archive::Options;
use packcraft::formats::{self, CreateOptions, CreateReport, Format, Item};
use packcraft::volumes;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const ACCENT: Color32 = Color32::from_rgb(0xE0, 0x7A, 0x2E);
const ACCENT_RGB: [u8; 3] = [0xE0, 0x7A, 0x2E];

fn main() -> eframe::Result {
    // Scriptable install of the Finder Quick Actions, no window.
    #[cfg(target_os = "macos")]
    match std::env::args().nth(1).as_deref() {
        Some("--install-finder-actions") => match finder::install() {
            Ok(dir) => {
                println!("Installed {} Finder Quick Actions in {}", finder::ACTIONS.len(), dir.display());
                std::process::exit(0)
            }
            Err(e) => {
                eprintln!("packcraft-gui: {e}");
                std::process::exit(1)
            }
        },
        Some("--remove-finder-actions") => match finder::remove() {
            Ok(n) => {
                println!("Removed {n} Finder Quick Actions");
                std::process::exit(0)
            }
            Err(e) => {
                eprintln!("packcraft-gui: {e}");
                std::process::exit(1)
            }
        },
        _ => {}
    }
    let launch = Launch::from_args(std::env::args_os().skip(1).map(PathBuf::from).collect());
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
            Ok(Box::new(App::new(launch)))
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
    Archive { file: PathBuf, items: Vec<Item>, prefix: String },
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
    Add(Box<AddForm>),
    Password { archive: PathBuf, retry: Retry, input: String, wrong: bool },
    Extract { archive: PathBuf, only: Option<Vec<String>>, dest: String, open_after: bool },
    Message { title: String, body: String, error: bool },
    About,
}

/// What to run again once the user has typed a password.
#[derive(Clone)]
enum Retry {
    Open,
    Test,
    Extract { only: Option<Vec<String>>, dest: PathBuf, open_after: bool },
    View { entry: String },
}

/// Split presets: (label, bytes). 0 = no split.
const SPLITS: [(&str, u64); 7] = [
    ("No split", 0),
    ("10 MB", 10_000_000),
    ("100 MB", 100_000_000),
    ("700 MB (CD)", 700 << 20),
    ("4092 MB (FAT32)", 4092 << 20),
    ("4.7 GB (DVD)", 4_700_000_000),
    ("Custom", u64::MAX),
];

#[derive(Clone)]
struct AddForm {
    inputs: Vec<PathBuf>,
    archive: String,
    format: Format,
    fast: bool,
    level: u32,
    split: usize,
    custom_split: String,
    password: String,
    confirm: String,
    verify: bool,
}

/// The settings the Add dialog starts from (the last ones used).
#[derive(Clone)]
struct Defaults {
    format: Format,
    fast: bool,
    level: u32,
    split: usize,
    custom_split: String,
    verify: bool,
}

impl Default for Defaults {
    fn default() -> Self {
        Defaults { format: Format::Zip, fast: false, level: 9, split: 0, custom_split: "250 MB".into(), verify: true }
    }
}

impl Defaults {
    fn path() -> Option<PathBuf> {
        let base = if cfg!(windows) {
            std::env::var_os("APPDATA").map(PathBuf::from)
        } else if cfg!(target_os = "macos") {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"))
        } else {
            std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        };
        base.map(|b| b.join("PackCraft").join("settings.txt"))
    }

    /// `key=value` lines; anything unreadable falls back to the default.
    fn load() -> Self {
        let mut d = Defaults::default();
        let Some(text) = Self::path().and_then(|p| std::fs::read_to_string(p).ok()) else { return d };
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            match k.trim() {
                "format" => d.format = Format::WRITABLE.into_iter().find(|f| f.extension() == v.trim()).unwrap_or(d.format),
                "fast" => d.fast = v.trim() == "true",
                "level" => d.level = v.trim().parse().unwrap_or(d.level).min(9),
                "split" => d.split = v.trim().parse().unwrap_or(0).min(SPLITS.len() - 1),
                "custom_split" => d.custom_split = v.trim().to_string(),
                "verify" => d.verify = v.trim() == "true",
                _ => {}
            }
        }
        d
    }

    fn save(&self) {
        let Some(p) = Self::path() else { return };
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let text = format!(
            "format={}\nfast={}\nlevel={}\nsplit={}\ncustom_split={}\nverify={}\n",
            self.format.extension(), self.fast, self.level, self.split, self.custom_split, self.verify
        );
        let _ = std::fs::write(p, text);
    }
}

/// How the app was started.
enum Launch {
    Open(Option<PathBuf>),
    Add(Vec<PathBuf>),
    AddDefault(Vec<PathBuf>),
    /// Extract each archive into the folder it is in.
    ExtractHere(Vec<PathBuf>),
    /// Extract each archive into a new folder named after it.
    ExtractTo(Vec<PathBuf>),
}

impl Launch {
    fn from_args(args: Vec<PathBuf>) -> Self {
        match args.first().and_then(|a| a.to_str()) {
            Some("--add") => Launch::Add(args[1..].to_vec()),
            Some("--add-default") => Launch::AddDefault(args[1..].to_vec()),
            Some("--extract-here") => Launch::ExtractHere(args[1..].to_vec()),
            Some("--extract-to") => Launch::ExtractTo(args[1..].to_vec()),
            _ => Launch::Open(args.into_iter().next()),
        }
    }
}

struct Job {
    label: String,
    started: Instant,
    rx: mpsc::Receiver<JobDone>,
}

enum JobDone {
    Created { archive: PathBuf, report: Result<CreateReport, String>, quit_after: bool },
    Opened { archive: PathBuf, result: Result<Vec<Item>, String> },
    Extracted { archive: PathBuf, retry: Retry, dest: PathBuf, result: Result<(usize, u64), String>, open_after: bool },
    Tested { archive: PathBuf, result: Result<(usize, u64), String> },
    Viewed { archive: PathBuf, retry: Retry, result: Result<PathBuf, String> },
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
    /// Passwords typed this session, per archive.
    passwords: std::collections::HashMap<PathBuf, String>,
    defaults: Defaults,
    /// Work to start on the first frame (needs the context).
    pending: Option<Launch>,
    /// Archives still to extract (archive, destination), from a Finder action.
    queue: Vec<(PathBuf, PathBuf)>,
    /// Close the window once the queue is done without an error.
    quit_when_done: bool,
}

impl App {
    fn new(launch: Launch) -> Self {
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
            passwords: Default::default(),
            defaults: Defaults::load(),
            pending: None,
            queue: Vec::new(),
            quit_when_done: false,
        };
        match launch {
            Launch::Open(start) => app.navigate(start.unwrap_or(folder)),
            Launch::Add(inputs) | Launch::AddDefault(inputs) if inputs.is_empty() => app.navigate(folder),
            other @ (Launch::Add(_) | Launch::AddDefault(_)) => {
                let (Launch::Add(inputs) | Launch::AddDefault(inputs)) = &other else { unreachable!() };
                let dir = inputs[0].parent().map(Path::to_path_buf).unwrap_or(folder);
                app.navigate(dir);
                app.pending = Some(other);
            }
            Launch::ExtractHere(inputs) | Launch::ExtractTo(inputs) if inputs.is_empty() => app.navigate(folder),
            other @ (Launch::ExtractHere(_) | Launch::ExtractTo(_)) => {
                let (Launch::ExtractHere(inputs) | Launch::ExtractTo(inputs)) = &other else { unreachable!() };
                let dir = inputs[0].parent().map(Path::to_path_buf).unwrap_or(folder);
                app.navigate(dir);
                app.pending = Some(other);
            }
        }
        app
    }

    fn password_for(&self, archive: &Path) -> Option<String> {
        self.passwords.get(archive).cloned()
    }

    // ------------------------------------------------------------ navigation

    fn navigate(&mut self, path: PathBuf) {
        let path = if path.is_relative() { self.current_folder().join(path) } else { path };
        if path.is_dir() {
            self.loc = Location::Folder(path);
        } else if path.is_file() && formats::is_archive(&path) {
            let pw = self.password_for(&path);
            // Listing a big 7z or RAR set takes a moment: off the UI thread once it runs.
            if let Some(ctx) = self.ctx_hint.clone() {
                let label = format!("Opening {}", file_name(&path));
                return self.spawn(&ctx, label, move || JobDone::Opened { result: formats::list(&path, pw.as_deref()), archive: path });
            }
            match formats::list(&path, pw.as_deref()) {
                Ok(items) => self.loc = Location::Archive { file: path, items, prefix: String::new() },
                Err(e) if formats::needs_password(&e) => return self.ask_password(path, Retry::Open, e.starts_with(formats::WRONG_PASSWORD)),
                Err(e) => return self.message("Cannot open archive", e, true),
            }
        } else {
            return self.message("Not found", format!("{} is not a folder or an archive PackCraft can open.", path.display()), true);
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

    fn ask_password(&mut self, archive: PathBuf, retry: Retry, wrong: bool) {
        self.passwords.remove(&archive);
        self.dialog = Dialog::Password { archive, retry, input: String::new(), wrong };
    }

    fn run_retry(&mut self, archive: PathBuf, retry: Retry) {
        match retry {
            Retry::Open => self.navigate(archive),
            Retry::Test => self.start_test(archive),
            Retry::Extract { only, dest, open_after } => self.start_extract(archive, only, dest, open_after),
            Retry::View { entry } => self.start_view(archive, entry),
        }
    }

    fn ctx(&self) -> egui::Context {
        self.ctx_hint.clone().expect("context set every frame")
    }

    fn start_test(&mut self, archive: PathBuf) {
        let (threads, pw) = (self.threads, self.password_for(&archive));
        let a2 = archive.clone();
        let ctx = self.ctx();
        self.spawn(&ctx, format!("Testing {}", file_name(&archive)), move || JobDone::Tested {
            result: formats::extract(&a2, None, threads, pw.as_deref(), &|_| true),
            archive: a2,
        });
    }

    fn start_extract(&mut self, archive: PathBuf, only: Option<Vec<String>>, dest: PathBuf, open_after: bool) {
        let (threads, pw) = (self.threads, self.password_for(&archive));
        let retry = Retry::Extract { only: only.clone(), dest: dest.clone(), open_after };
        let ctx = self.ctx();
        self.spawn(&ctx, format!("Extracting {}", file_name(&archive)), move || {
            let result = match only {
                Some(list) => {
                    let set: BTreeSet<String> = list.into_iter().collect();
                    formats::extract(&archive, Some(&dest), threads, pw.as_deref(), &move |p| set.contains(p))
                }
                None => formats::extract(&archive, Some(&dest), threads, pw.as_deref(), &|_| true),
            };
            JobDone::Extracted { archive, retry, dest, result, open_after }
        });
    }

    fn start_view(&mut self, archive: PathBuf, entry: String) {
        let (threads, pw) = (self.threads, self.password_for(&archive));
        let dir = std::env::temp_dir().join(format!("packcraft-view-{}", std::process::id()));
        let retry = Retry::View { entry: entry.clone() };
        let ctx = self.ctx();
        self.spawn(&ctx, format!("Opening {entry}"), move || {
            let keep = entry.clone();
            let result = formats::extract(&archive, Some(&dir), threads, pw.as_deref(), &move |p| p == keep)
                .map(|_| dir.join(entry.replace('/', std::path::MAIN_SEPARATOR_STR)));
            JobDone::Viewed { archive, retry, result }
        });
    }

    fn start_create(&mut self, form: AddForm, quit_after: bool) {
        let mut path = PathBuf::from(form.archive.trim());
        if path.extension().is_none() {
            path.set_extension(form.format.extension());
        }
        let format = match Format::for_output(&path) {
            Some(f) => f,
            None => return self.message("Add to archive", "The archive name must end in .zip, .7z or .pcraft. (PackCraft can open RAR archives but not create them: the format is proprietary.)", true),
        };
        let volume = match SPLITS[form.split].1 {
            0 => None,
            u64::MAX => match volumes::parse_size(&form.custom_split) {
                Ok(v) => Some(v),
                Err(e) => return self.message("Split size", e, true),
            },
            v => Some(v),
        };
        let password = (!form.password.is_empty()).then(|| form.password.clone());
        if password.is_some() && form.password != form.confirm {
            return self.message("Password", "The two passwords are different.", true);
        }
        if path.exists() {
            return self.message("Archive exists", format!("{} already exists. Choose another name.", path.display()), true);
        }
        self.defaults = Defaults { format, fast: form.fast, level: form.level, split: form.split, custom_split: form.custom_split.clone(), verify: form.verify };
        self.defaults.save();
        let opts = CreateOptions {
            format,
            pcraft: Options { threads: self.threads, fast: form.fast, verify: form.verify, ..Options::default() },
            level: form.level,
            volume,
            password,
        };
        let (inputs, verify, threads) = (form.inputs, form.verify, self.threads);
        let p2 = path.clone();
        let ctx = self.ctx();
        self.spawn(&ctx, format!("Creating {}", file_name(&path)), move || {
            let mut report = formats::create(&p2, &inputs, &opts);
            // pcraft verifies inside create; zip and 7z are read back here.
            if let (Ok(r), true, false) = (&report, verify, format == Format::Pcraft) {
                if let Err(e) = formats::extract(&r.parts[0], None, threads, opts.password.as_deref(), &|_| true) {
                    report = Err(format!("the archive was written but failed its test: {e}"));
                }
            }
            JobDone::Created { archive: p2, report, quit_after }
        });
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
        let d = self.defaults.clone();
        let archive = parent.join(format!("{base}.{}", d.format.extension())).display().to_string();
        self.dialog = Dialog::Add(Box::new(AddForm {
            inputs,
            archive,
            format: d.format,
            fast: d.fast,
            level: d.level,
            split: d.split,
            custom_split: d.custom_split,
            password: String::new(),
            confirm: String::new(),
            verify: d.verify,
        }));
    }

    fn extract_dialog(&mut self) {
        let (archive, only) = match &self.loc {
            Location::Archive { file, .. } => (file.clone(), self.selected_entries()),
            Location::Folder(_) => {
                let picked: Vec<PathBuf> = self.selected_paths().into_iter().filter(|p| formats::is_archive(p)).collect();
                match picked.as_slice() {
                    [one] => (one.clone(), None),
                    [] => return self.message("Extract", "Select an archive, or open one first.", false),
                    _ => return self.message("Extract", "Select one archive at a time.", false),
                }
            }
        };
        let dest = archive.parent().unwrap_or(Path::new(".")).join(set_stem(&archive)).display().to_string();
        self.dialog = Dialog::Extract { archive, only, dest, open_after: true };
    }

    /// Finder "Extract here" / "Extract to folder": one job per archive set,
    /// in order, quitting when all succeed.
    fn queue_extracts(&mut self, inputs: Vec<PathBuf>, own_folder: bool) {
        let mut seen = BTreeSet::new();
        let mut skipped = Vec::new();
        for p in inputs {
            // Several parts of one set selected: extract the set once.
            let first = match formats::detect(&p) {
                Ok((_, set)) if formats::is_archive(&p) => set.parts[0].clone(),
                _ => {
                    skipped.push(file_name(&p));
                    continue;
                }
            };
            if !seen.insert(first.clone()) {
                continue;
            }
            let parent = first.parent().unwrap_or(Path::new(".")).to_path_buf();
            let dest = if own_folder { free_name(&parent.join(set_stem(&first))) } else { parent };
            self.queue.push((first, dest));
        }
        if !skipped.is_empty() {
            self.queue.clear();
            return self.message("Extract", format!("Not an archive PackCraft can open: {}", skipped.join(", ")), true);
        }
        self.quit_when_done = true;
        self.next_queued();
    }

    fn next_queued(&mut self) {
        if self.queue.is_empty() {
            return;
        }
        let (archive, dest) = self.queue.remove(0);
        self.start_extract(archive, None, dest, false);
    }

    fn target_archive(&self) -> Option<PathBuf> {
        match &self.loc {
            Location::Archive { file, .. } => Some(file.clone()),
            Location::Folder(_) => self.selected_paths().into_iter().find(|p| formats::is_archive(p)),
        }
    }

    fn test_archive(&mut self) {
        let Some(archive) = self.target_archive() else {
            return self.message("Test", "Select an archive, or open one first.", false);
        };
        self.start_test(archive);
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
        let (entry, archive) = (entries[0].clone(), file.clone());
        self.start_view(archive, entry);
    }

    fn info(&mut self) {
        match &self.loc {
            Location::Archive { file, items, .. } => {
                let (format, set) = match formats::detect(file) {
                    Ok(v) => v,
                    Err(e) => return self.message("Archive information", e, true),
                };
                let packed = set.total_len().unwrap_or(0);
                let files: Vec<&Item> = items.iter().filter(|i| !i.is_dir).collect();
                let raw: u64 = files.iter().map(|i| i.size).sum();
                let mut by_route: Vec<(String, usize, u64)> = Vec::new();
                for it in &files {
                    match by_route.iter_mut().find(|(r, _, _)| *r == it.method) {
                        Some(e) => {
                            e.1 += 1;
                            e.2 += it.size;
                        }
                        None => by_route.push((it.method.clone(), 1, it.size)),
                    }
                }
                let parts = if set.parts.len() > 1 { format!(" in {} parts", set.parts.len()) } else { String::new() };
                let locked = if items.iter().any(|i| i.encrypted) { "\nEncrypted: yes" } else { "" };
                let mut body = format!(
                    "{}\n\n{} archive{parts}{locked}\n{} files, {} unpacked\nArchive size {} ({:.1}% of original)\n\nMethods:\n",
                    file.display(),
                    format.name(),
                    files.len(),
                    human(raw),
                    human(packed),
                    if raw > 0 { 100.0 * packed as f64 / raw as f64 } else { 0.0 }
                );
                for (r, n, b) in by_route {
                    body.push_str(&format!("  {:<16} {n} files, {}\n", if r.is_empty() { "-" } else { r.as_str() }, human(b)));
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
            JobDone::Created { archive, report, quit_after } => match report {
                Ok(r) => {
                    if quit_after {
                        open_with_system_reveal(&r.parts[r.parts.len() - 1]);
                        if let Some(ctx) = &self.ctx_hint {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                        return;
                    }
                    let mut body = format!(
                        "{}\n\n{} files: {} -> {} ({:.2}%) in {secs:.1} s\n",
                        archive.display(),
                        r.files,
                        human(r.input_bytes),
                        human(r.archive_bytes),
                        100.0 * r.archive_bytes as f64 / r.input_bytes.max(1) as f64
                    );
                    if r.parts.len() > 1 {
                        body.push_str(&format!("\n{} parts: {} ... {}\n", r.parts.len(), file_name(&r.parts[0]), file_name(&r.parts[r.parts.len() - 1])));
                    }
                    if let Some(p) = &r.pcraft {
                        for (route, st) in &p.stats {
                            body.push_str(&format!("\n  {:<16} {} files  {} -> {}", route.name(), st.files, human(st.raw), human(st.packed)));
                        }
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
            JobDone::Opened { archive, result: Err(e) } if formats::needs_password(&e) => {
                self.ask_password(archive, Retry::Open, e.starts_with(formats::WRONG_PASSWORD));
            }
            JobDone::Opened { archive, result } => match result {
                Ok(items) => {
                    self.loc = Location::Archive { file: archive, items, prefix: String::new() };
                    self.reload();
                }
                Err(e) => self.message("Cannot open archive", e, true),
            },
            JobDone::Extracted { archive, retry, result: Err(e), .. } | JobDone::Viewed { archive, retry, result: Err(e) } if formats::needs_password(&e) => {
                self.ask_password(archive, retry, e.starts_with(formats::WRONG_PASSWORD));
            }
            JobDone::Tested { archive, result: Err(e) } if formats::needs_password(&e) => {
                self.ask_password(archive, Retry::Test, e.starts_with(formats::WRONG_PASSWORD));
            }
            JobDone::Extracted { dest, result, open_after, .. } => match result {
                Ok((n, bytes)) => {
                    self.status = format!("Extracted {n} files ({}) to {} in {secs:.1} s", human(bytes), dest.display());
                    if open_after {
                        open_with_system(&dest);
                    }
                    if !self.queue.is_empty() {
                        self.next_queued();
                    } else if std::mem::take(&mut self.quit_when_done) {
                        if let Some(ctx) = &self.ctx_hint {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    }
                }
                Err(e) => {
                    // Stay open and say what failed; the rest of the queue waits.
                    self.quit_when_done = false;
                    let left = if self.queue.is_empty() { String::new() } else { format!("\n\n{} more archive(s) were not extracted.", self.queue.len()) };
                    self.queue.clear();
                    self.message("Extraction failed", format!("{e}{left}"), true)
                }
            },
            JobDone::Tested { archive, result } => match result {
                Ok((n, bytes)) => self.message(
                    "Test passed",
                    format!("{}\n\nAll {n} entries ({}) decoded and every checksum matches ({secs:.1} s).", archive.display(), human(bytes)),
                    false,
                ),
                Err(e) => self.message("Test FAILED", format!("{}\n\n{e}", archive.display()), true),
            },
            JobDone::Viewed { result, .. } => match result {
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
        match self.pending.take() {
            Some(Launch::Add(inputs)) => self.add_dialog(inputs),
            Some(Launch::AddDefault(inputs)) => {
                self.add_dialog(inputs);
                if let Dialog::Add(mut form) = std::mem::replace(&mut self.dialog, Dialog::None) {
                    // One click never fails on an existing name: "photos 2.zip".
                    form.archive = free_name(Path::new(&form.archive)).display().to_string();
                    self.start_create(*form, true);
                }
            }
            Some(Launch::ExtractHere(inputs)) => self.queue_extracts(inputs, false),
            Some(Launch::ExtractTo(inputs)) => self.queue_extracts(inputs, true),
            _ => {}
        }
        let mut actions: Vec<Action> = Vec::new();

        // Files dropped from the OS: an archive opens, anything else goes to "Add".
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).collect());
        if !dropped.is_empty() && self.job.is_none() {
            if dropped.len() == 1 && formats::is_archive(&dropped[0]) {
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
                    #[cfg(target_os = "macos")]
                    {
                        ui.separator();
                        let label = if finder::installed() { "Reinstall Finder actions" } else { "Install Finder actions" };
                        if ui.button(label).clicked() {
                            match finder::install() {
                                Ok(dir) => self.message(
                                    "Finder actions installed",
                                    format!(
                                        "Right-click files in Finder, then Quick Actions (or Services):\n\n  PackCraft - Compress: one click, with your saved settings\n  PackCraft - Compress with settings: opens the Add dialog\n  PackCraft - Extract here\n  PackCraft - Extract to folder\n\nInstalled in {}. If you move PackCraft, install them again. \
                                         RAR stays extract-only: the format is proprietary, so PackCraft creates zip, 7z or pcraft.",
                                        dir.display()
                                    ),
                                    false,
                                ),
                                Err(e) => self.message("Could not install the Finder actions", e, true),
                            }
                        }
                        if finder::installed() && ui.button("Remove Finder actions").clicked() {
                            match finder::remove() {
                                Ok(n) => self.message("Finder actions removed", format!("Removed {n} Quick Actions."), false),
                                Err(e) => self.message("Could not remove the Finder actions", e, true),
                            }
                        }
                    }
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
                Action::Test => self.test_archive(),
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
            Dialog::Add(form) => {
                let resp = Modal::new(Id::unique("packcraft-add")).show(ctx, |ui| {
                    ui.set_width(540.0);
                    ui.heading("Add to archive");
                    ui.add_space(4.0);
                    ui.label(format!("{} item(s): {}", form.inputs.len(), form.inputs.iter().map(|p| file_name(p)).collect::<Vec<_>>().join(", ")));
                    ui.add_space(6.0);
                    ui.label("Archive name");
                    ui.add(egui::TextEdit::singleline(&mut form.archive).desired_width(f32::INFINITY));
                    ui.add_space(6.0);
                    egui::Grid::new("add-grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                        ui.label("Format");
                        ui.horizontal(|ui| {
                            for f in Format::WRITABLE {
                                let label = match f {
                                    Format::Pcraft => "PackCraft (smallest)",
                                    Format::Zip => "ZIP (opens everywhere)",
                                    _ => "7z",
                                };
                                if ui.radio(form.format == f, label).clicked() && form.format != f {
                                    form.format = f;
                                    form.level = if f == Format::Zip { 9 } else { 7 };
                                    let mut p = PathBuf::from(form.archive.trim());
                                    p.set_extension(f.extension());
                                    form.archive = p.display().to_string();
                                }
                            }
                        });
                        ui.end_row();
                        ui.label("Compression");
                        if form.format == Format::Pcraft {
                            ui.horizontal(|ui| {
                                ui.radio_value(&mut form.fast, false, "Best (smallest)");
                                ui.radio_value(&mut form.fast, true, "Fast (quick to extract)");
                            });
                        } else {
                            let hint = match form.level {
                                0 => "store",
                                1..=3 => "fast",
                                4..=6 => "normal",
                                _ => "maximum",
                            };
                            ui.add(egui::Slider::new(&mut form.level, 0..=9).text(hint));
                        }
                        ui.end_row();
                        ui.label("Split into parts");
                        ui.horizontal(|ui| {
                            egui::ComboBox::from_id_salt("split").selected_text(SPLITS[form.split].0).show_ui(ui, |ui| {
                                for (i, (label, _)) in SPLITS.iter().enumerate() {
                                    ui.selectable_value(&mut form.split, i, *label);
                                }
                            });
                            if SPLITS[form.split].1 == u64::MAX {
                                ui.add(egui::TextEdit::singleline(&mut form.custom_split).desired_width(90.0).hint_text("250 MB"));
                            }
                        });
                        ui.end_row();
                        let can_encrypt = form.format != Format::Pcraft;
                        ui.label("Password");
                        ui.add_enabled_ui(can_encrypt, |ui| {
                            ui.horizontal(|ui| {
                                ui.add(egui::TextEdit::singleline(&mut form.password).password(true).desired_width(150.0).hint_text("none"));
                                ui.add(egui::TextEdit::singleline(&mut form.confirm).password(true).desired_width(150.0).hint_text("repeat"));
                            });
                        });
                        ui.end_row();
                        if !can_encrypt {
                            ui.label("");
                            ui.label(RichText::new("Encryption needs ZIP or 7z (AES-256).").weak());
                            ui.end_row();
                        }
                    });
                    if form.format == Format::Pcraft {
                        form.password.clear();
                        form.confirm.clear();
                    }
                    ui.checkbox(&mut form.verify, "Test the archive after creating it");
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
                    let form = (**form).clone();
                    start = Some(Box::new(move |app: &mut App| app.start_create(form, false)));
                }
                close = ok || cancel || resp.should_close();
            }
            Dialog::Password { archive, retry, input, wrong } => {
                let resp = Modal::new(Id::unique("packcraft-password")).show(ctx, |ui| {
                    ui.set_width(420.0);
                    ui.heading("Password");
                    ui.add_space(4.0);
                    ui.label(format!("{} is encrypted.", file_name(archive)));
                    if *wrong {
                        ui.label(RichText::new("That password is wrong. Try again.").color(ui.visuals().error_fg_color));
                    }
                    ui.add_space(6.0);
                    let r = ui.add(egui::TextEdit::singleline(input).password(true).desired_width(f32::INFINITY));
                    r.request_focus();
                    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                    ui.add_space(8.0);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let ok = ui.add(egui::Button::new(RichText::new("OK").strong()).fill(ACCENT.linear_multiply(0.8))).clicked();
                        let cancel = ui.button("Cancel").clicked();
                        (ok || enter, cancel)
                    })
                    .inner
                });
                let (ok, cancel) = resp.inner;
                if ok && !input.is_empty() {
                    let (archive, retry, pw) = (archive.clone(), retry.clone(), input.clone());
                    start = Some(Box::new(move |app: &mut App| {
                        app.passwords.insert(archive.clone(), pw);
                        app.run_retry(archive, retry);
                    }));
                }
                close = (ok && !input.is_empty()) || cancel || resp.should_close();
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
                            *dest = archive.parent().unwrap_or(Path::new(".")).join(set_stem(archive)).display().to_string();
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
                    start = Some(Box::new(move |app: &mut App| app.start_extract(archive, only, dest, open_after)));
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

/// The name of a set without its archive and part suffixes:
/// `photos.part01.rar`, `photos.7z.001`, `photos.r00` and `photos.zip` all give `photos`.
/// `p` if nothing is there yet, else "name 2.ext", "name 3.ext" ...
fn free_name(p: &Path) -> PathBuf {
    if !p.exists() {
        return p.to_path_buf();
    }
    let parent = p.parent().unwrap_or(Path::new("."));
    let name = file_name(p);
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() && !p.is_dir() => (s.to_string(), format!(".{e}")),
        _ => (name.clone(), String::new()),
    };
    (2..).map(|i| parent.join(format!("{stem} {i}{ext}"))).find(|c| !c.exists()).unwrap()
}

fn set_stem(p: &Path) -> String {
    let mut name = file_name(p);
    loop {
        let Some((head, ext)) = name.rsplit_once('.') else { break };
        let e = ext.to_ascii_lowercase();
        let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        let strip = matches!(e.as_str(), "zip" | "7z" | "rar" | "pcraft")
            || digits(&e)
            || (e.len() == 3 && matches!(e.as_bytes()[0], b'r'..=b'z') && digits(&e[1..]))
            || (e.starts_with("part") && digits(&e[4..]));
        if !strip || head.is_empty() {
            break;
        }
        name = head.to_string();
    }
    name
}

fn archive_kind(p: &Path) -> String {
    let e = p.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    match e.as_str() {
        "pcraft" => "PackCraft archive".into(),
        "zip" => "ZIP archive".into(),
        "7z" => "7z archive".into(),
        "rar" => "RAR archive".into(),
        _ => "Archive part".into(),
    }
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
        let is_archive = !is_dir && formats::is_archive(&path);
        let kind = if is_dir {
            "Folder".to_string()
        } else if is_archive {
            archive_kind(&path)
        } else {
            path.extension().map(|e| format!("{} file", e.to_string_lossy().to_uppercase())).unwrap_or_else(|| "File".into())
        };
        rows.push(Row { name, is_dir, is_archive, size: if is_dir { 0 } else { meta.len() }, files: 0, kind, modified: meta.modified().ok() });
    }
    Ok(rows)
}

fn archive_rows(items: &[Item], prefix: &str) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    let mut dirs: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for it in items {
        let Some(rest) = it.path.strip_prefix(prefix) else { continue };
        if rest.is_empty() {
            continue;
        }
        // An explicit folder entry shows the folder even when it is empty.
        let split = if it.is_dir { Some((rest.split('/').next().unwrap_or(rest), "")) } else { rest.split_once('/') };
        match split {
            Some((dir, _)) => {
                let k = *dirs.entry(dir.to_string()).or_insert_with(|| {
                    rows.push(Row { name: dir.to_string(), is_dir: true, is_archive: false, size: 0, files: 0, kind: "Folder".into(), modified: None });
                    rows.len() - 1
                });
                if !it.is_dir {
                    rows[k].size += it.size;
                    rows[k].files += 1;
                }
            }
            None => rows.push(Row {
                name: rest.to_string(),
                is_dir: false,
                is_archive: false,
                size: it.size,
                files: 1,
                kind: if it.encrypted { format!("{} (encrypted)", it.method) } else { it.method.clone() },
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

/// Shows the file selected in Finder / Explorer (Linux: opens its folder).
fn open_with_system_reveal(p: &Path) {
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open").arg("-R").arg(p).spawn();
    #[cfg(target_os = "windows")]
    let r = std::process::Command::new("explorer").arg(format!("/select,{}", p.display())).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let r = std::process::Command::new("xdg-open").arg(p.parent().unwrap_or(Path::new("."))).spawn();
    if let Err(e) = r {
        eprintln!("cannot show {}: {e}", p.display());
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
        let it = |p: &str, s| Item { path: p.into(), size: s, packed: None, method: "LZMA2".into(), is_dir: false, encrypted: false };
        let mut items = vec![it("p/a.txt", 3), it("p/sub/b.txt", 4), it("p/sub/c/d.txt", 5), it("top.txt", 1)];
        items.push(Item { is_dir: true, ..it("p/empty", 0) });
        let root = archive_rows(&items, "");
        assert_eq!(root.len(), 2);
        let p = archive_rows(&items, "p/");
        let sub = p.iter().find(|r| r.name == "sub").unwrap();
        assert!(sub.is_dir && sub.size == 9 && sub.files == 2);
        assert!(p.iter().any(|r| r.name == "a.txt" && !r.is_dir));
        assert!(p.iter().any(|r| r.name == "empty" && r.is_dir && r.files == 0));
    }

    #[test]
    fn set_stems() {
        for (name, want) in [("photos.part01.rar", "photos"), ("photos.7z.001", "photos"), ("photos.r00", "photos"), ("photos.zip", "photos"), ("my.notes.zip", "my.notes"), ("a.z01", "a")] {
            assert_eq!(set_stem(Path::new(name)), want, "{name}");
        }
    }

    #[test]
    fn free_names_never_overwrite() {
        let d = std::env::temp_dir().join(format!("packcraft-free-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("photos")).unwrap();
        std::fs::write(d.join("photos.zip"), b"x").unwrap();
        std::fs::write(d.join("photos 2.zip"), b"x").unwrap();
        assert_eq!(free_name(&d.join("photos.zip")), d.join("photos 3.zip"));
        assert_eq!(free_name(&d.join("photos")), d.join("photos 2"));
        assert_eq!(free_name(&d.join("new.7z")), d.join("new.7z"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn launch_modes() {
        assert!(matches!(Launch::from_args(vec!["--add".into(), "a".into()]), Launch::Add(v) if v.len() == 1));
        assert!(matches!(Launch::from_args(vec!["x.zip".into()]), Launch::Open(Some(_))));
        assert!(matches!(Launch::from_args(vec!["--extract-here".into(), "a.rar".into(), "b.zip".into()]), Launch::ExtractHere(v) if v.len() == 2));
        assert!(matches!(Launch::from_args(vec!["--extract-to".into(), "a.rar".into()]), Launch::ExtractTo(v) if v.len() == 1));
    }

    #[test]
    fn human_sizes() {
        assert_eq!(human(999), "999 B");
        assert_eq!(human(1_500_000), "1.5 MB");
    }
}
