//! ext4fs-tool GUI — a read-only ext2/3/4 image/disk browser.
//!
//! Build:  cargo build --bin ext4fs-tool-gui
//! Run:    target/debug/ext4fs-tool-gui

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use egui_extras::{Column, TableBuilder};
use ext4fs_tool::ext4::copy::{copy_to_parallel, count_tree_with_bytes, CopyProgress};
use ext4fs_tool::ext4::inode::{read_inode_data, EXT4_ROOT_INO};
use ext4fs_tool::ext4::Ext4;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

const MAX_TEXT: usize = 512 * 1024;
const MAX_HEX: usize = 128 * 1024;

fn main() -> eframe::Result {
    let open_disk = std::env::args().any(|a| a == "--open-disk");
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([820.0, 480.0])
            .with_title("ext4fs-tool GUI"),
        ..Default::default()
    };
    eframe::run_native(
        "ext4fs-tool GUI",
        options,
        Box::new(move |cc| Ok(Box::new(GuiApp::new(cc, open_disk)))),
    )
}

// ---------------------------------------------------------------------------
// data
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct RowInfo {
    ino: u32,
    ft: u8,
    size: u64,
    name: String,
}

struct Content {
    name: String,
    data: Vec<u8>,
    text: String,
    truncated: bool,
    hex: bool,
    save_msg: Option<String>,
}

#[derive(Clone)]
struct TreeNode {
    ino: u32,
    name: String,
    is_dir: bool,
}

/// Active rubber-band (marquee) selection.
struct Marquee {
    start: egui::Pos2,
    current: egui::Pos2,
    additive: bool,
}

enum CtxAction {
    CopyOne(usize),
    CopyMany,
    Rename(String, String),
}

/// Dialog for "copy to a folder under a new name".
struct RenameDialog {
    src: String,
    name: String,
    folder: Option<String>,
}

#[derive(Clone, Copy, PartialEq)]
enum SortCol {
    Type,
    Size,
    Name,
}

#[derive(Clone, Copy, PartialEq)]
enum ThemePref {
    Dark,
    Light,
    System,
}

#[derive(Clone, Copy, PartialEq)]
enum Lang {
    Zh,
    En,
}

#[derive(Clone, Copy, PartialEq)]
enum ThreadSetting {
    Auto,
    N(u8),
}

struct PartRow {
    index: u32,
    start_bytes: u64,
    sectors: u64,
    name: String,
    kind: String,
    fs: String,
    label: String,
}

struct DiskEntry {
    path: String,
    size: Option<u64>,
    model: Option<String>,
    sector_size: u64,
    parts: Option<Vec<PartRow>>,
    error: Option<String>,
}

struct DiskDialog {
    disks: Vec<DiskEntry>,
    selected: Option<(usize, usize)>,
}

/// A background copy/extract job with shared progress and cancel flag.
struct CopyJob {
    progress: Arc<CopyProgress>,
    cancel: Arc<AtomicBool>,
    /// Filled by the worker thread when finished: (dest display, errors).
    done: Arc<Mutex<Option<(String, Vec<String>)>>>,
    workers: usize,
}

struct GuiApp {
    image_path: String,
    offset: u64,
    fs: Option<Arc<Ext4>>,
    path: String,
    rows: Vec<RowInfo>,
    /// Row highlighted in the details panel (last clicked).
    selected: Option<usize>,
    /// Multi-selection set (marquee / ctrl / shift). Used for extract & copy.
    sel: HashSet<usize>,
    anchor: Option<usize>,
    marquee: Option<Marquee>,
    /// Right-click context menu state: (row index, screen position).
    ctx_menu: Option<(usize, egui::Pos2)>,
    ctx_menu_ready: bool,
    error: Option<String>,
    status: String,
    content: Option<Content>,
    disk_dialog: Option<DiskDialog>,
    copy_job: Option<CopyJob>,
    // appearance
    theme: ThemePref,
    lang: Lang,
    thread_setting: ThreadSetting,
    custom_threads: u8,
    sort_col: SortCol,
    sort_asc: bool,
    // filter / view
    filter: String,
    /// Real indices (into `rows`) currently visible after filtering.
    shown: Vec<usize>,
    rename_dialog: Option<RenameDialog>,
    show_about: bool,
    // directory tree state
    tree_cache: HashMap<u32, Vec<TreeNode>>,
    tree_expanded: HashSet<u32>,
}

impl GuiApp {
    /// Pick the localized string for the current language.
    fn tr<'a>(&self, zh: &'a str, en: &'a str) -> &'a str {
        match self.lang {
            Lang::Zh => zh,
            Lang::En => en,
        }
    }

    /// Run the localized format closure for the current language.
    fn ftr(&self, zh: impl FnOnce() -> String, en: impl FnOnce() -> String) -> String {
        match self.lang {
            Lang::Zh => zh(),
            Lang::En => en(),
        }
    }

    fn new(cc: &eframe::CreationContext<'_>, open_disk: bool) -> Self {
        style(&cc.egui_ctx);
        let mut app = Self {
            image_path: String::new(),
            offset: 0,
            fs: None,
            path: "/".into(),
            rows: Vec::new(),
            selected: None,
            sel: HashSet::new(),
            anchor: None,
            marquee: None,
            ctx_menu: None,
            ctx_menu_ready: false,
            error: None,
            status: String::new(),
            content: None,
            disk_dialog: None,
            copy_job: None,
            theme: ThemePref::Dark,
            lang: Lang::Zh,
            thread_setting: ThreadSetting::Auto,
            custom_threads: 4,
            sort_col: SortCol::Name,
            sort_asc: true,
            filter: String::new(),
            shown: Vec::new(),
            rename_dialog: None,
            show_about: false,
            tree_cache: HashMap::new(),
            tree_expanded: HashSet::new(),
        };
        #[cfg(windows)]
        if !is_admin() {
            app.status = "当前不是管理员：访问物理磁盘需要提权（点 Open disk 会弹出 UAC 授权）。".into();
        }
        if open_disk {
            app.disk_dialog = Some(DiskDialog::scan());
        }
        app
    }

    fn open_image(&mut self, path: &str) {
        self.open_image_with_offset(path, 0);
    }

    fn open_image_with_offset(&mut self, path: &str, offset: u64) {
        match Ext4::open(path, offset) {
            Ok(fs) => {
                let summary = format!(
                    "{}: {} blocks, {} inodes, block size {}, {} groups",
                    path,
                    fs.sb.blocks_count,
                    fs.sb.inodes_count,
                    fs.block_size,
                    fs.groups.len()
                );
                self.fs = Some(Arc::new(fs));
                self.offset = offset;
                self.path = "/".into();
                self.selected = None;
                self.content = None;
                self.sel.clear();
                self.tree_cache.clear();
                self.tree_expanded.insert(EXT4_ROOT_INO);
                self.reload();
                if offset == 0 {
                    self.status = format!("opened {}", summary);
                } else {
                    self.status = format!("opened {} at partition offset {} : {}", path, offset, summary);
                }
            }
            Err(e) => {
                let mut msg = format!("cannot open {}: {}", path, e);
                if path.starts_with("\\\\.\\") {
                    msg.push_str("  (raw disks need administrator privileges; run the app as administrator)");
                }
                self.error = Some(msg);
            }
        }
    }

    fn reload(&mut self) {
        let result = self.fs.as_ref().map(|fs| load_entries(fs, &self.path));
        match result {
            Some(Ok(rows)) => {
                self.rows = rows;
                self.error = None;
            }
            Some(Err(m)) => self.error = Some(m),
            None => self.error = Some("no image loaded".into()),
        }
        self.selected = None;
        self.sel.clear();
        self.content = None;
        self.recompute_shown();
        self.expand_path();
    }

    /// Rebuild the filtered/visible row index list from the current filter.
    fn recompute_shown(&mut self) {
        let f = self.filter.to_lowercase();
        self.shown = if f.is_empty() {
            (0..self.rows.len()).collect()
        } else {
            self.rows
                .iter()
                .enumerate()
                .filter(|(_, r)| r.name.to_lowercase().contains(&f))
                .map(|(i, _)| i)
                .collect()
        };
        self.clear_selection();
    }

    fn navigate_to(&mut self, path: &str) {
        self.path = path.to_string();
        self.reload();
    }

    // ------------------------------------------------------------------
    // directory tree
    // ------------------------------------------------------------------

    fn expand_path(&mut self) {
        let Some(fs) = self.fs.as_ref() else {
            return;
        };
        let mut cur = EXT4_ROOT_INO;
        self.tree_expanded.insert(cur);
        for comp in self.path.split('/') {
            if comp.is_empty() || comp == "." || comp == ".." {
                continue;
            }
            let dir_ino = match fs.read_inode(cur) {
                Ok(i) => i,
                Err(_) => break,
            };
            match fs.lookup_dir(&dir_ino, comp) {
                Ok(Some((ino, ft))) => {
                    let is_dir = ft == 2 || fs.read_inode(ino).map(|i| i.is_dir()).unwrap_or(false);
                    if !is_dir {
                        break;
                    }
                    cur = ino;
                    self.tree_expanded.insert(cur);
                }
                _ => break,
            }
        }
    }

    fn ensure_children(&mut self, dir_ino: u32) {
        if self.tree_cache.contains_key(&dir_ino) {
            return;
        }
        let kids = self.load_tree_children(dir_ino);
        self.tree_cache.insert(dir_ino, kids);
    }

    fn load_tree_children(&self, dir_ino: u32) -> Vec<TreeNode> {
        let Some(fs) = self.fs.as_ref() else {
            return Vec::new();
        };
        let inode = match fs.read_inode(dir_ino) {
            Ok(i) => i,
            Err(_) => return Vec::new(),
        };
        let entries = match fs.list_dir(&inode) {
            Ok(e) => e,
            Err(_) => return Vec::new(),
        };
        let mut out = Vec::new();
        for e in entries {
            if e.name == "." || e.name == ".." {
                continue;
            }
            let is_dir = e.file_type == 2
                || fs.read_inode(e.ino).map(|i| i.is_dir()).unwrap_or(false);
            if is_dir {
                out.push(TreeNode {
                    ino: e.ino,
                    name: e.name,
                    is_dir: true,
                });
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    fn render_tree(&mut self, ui: &mut egui::Ui) {
        if self.fs.is_none() {
            ui.weak(self.tr("未打开镜像", "No image"));
            return;
        }
        let root = EXT4_ROOT_INO;
        self.ensure_children(root);
        let mut navigate: Option<String> = None;
        let root_children = self.tree_cache.get(&root).cloned().unwrap_or_default();
        for c in root_children {
            let p = format!("/{}", c.name);
            self.render_tree_node(ui, c, 0, &p, &mut navigate);
        }
        if let Some(p) = navigate {
            self.navigate_to(&p);
        }
    }

    fn render_tree_node(
        &mut self,
        ui: &mut egui::Ui,
        node: TreeNode,
        depth: usize,
        path: &str,
        navigate: &mut Option<String>,
    ) {
        let cur = self.path == path;
        ui.horizontal(|ui| {
            ui.add_space(depth as f32 * 16.0);
            if node.is_dir {
                let expanded = self.tree_expanded.contains(&node.ino);
                let arrow = if expanded { "▼" } else { "▶" };
                let r = ui.selectable_label(false, arrow);
                if r.clicked() {
                    if expanded {
                        self.tree_expanded.remove(&node.ino);
                    } else {
                        self.tree_expanded.insert(node.ino);
                    }
                }
            } else {
                ui.add_space(14.0);
            }
            let label = if node.is_dir {
                egui::RichText::new(format!("📁 {}", node.name)).color(egui::Color32::from_rgb(120, 180, 255))
            } else {
                egui::RichText::new(&node.name)
            };
            let r = ui.selectable_label(cur, label);
            if r.clicked() {
                *navigate = Some(path.to_string());
            }
        });
        if node.is_dir && self.tree_expanded.contains(&node.ino) {
            self.ensure_children(node.ino);
            let kids = self.tree_cache.get(&node.ino).cloned().unwrap_or_default();
            for c in kids {
                let cp = format!("{}/{}", path, c.name);
                self.render_tree_node(ui, c, depth + 1, &cp, navigate);
            }
        }
    }

    // ------------------------------------------------------------------
    // actions
    // ------------------------------------------------------------------

    fn open_disk_requested(&mut self) {
        #[cfg(windows)]
        {
            if is_admin() {
                self.disk_dialog = Some(DiskDialog::scan());
                return;
            }
            // Not elevated: ask the user, then relaunch this app as administrator.
            let answer = rfd::MessageDialog::new()
                .set_title("需要管理员权限")
                .set_description(
                    "访问物理磁盘需要管理员权限。\n是否以管理员身份重新启动本应用，并自动打开磁盘浏览器？",
                )
                .set_buttons(rfd::MessageButtons::YesNo)
                .show();
            if answer != rfd::MessageDialogResult::Yes {
                self.error = Some("未提权，无法读取物理磁盘。".into());
                return;
            }
            if relaunch_elevated("--open-disk") {
                self.status = "已请求管理员权限，请在弹出的 UAC 窗口中点击“是”…".into();
                std::process::exit(0);
            } else {
                self.error = Some("提权失败（UAC 被取消或系统限制）。".into());
            }
        }
        #[cfg(not(windows))]
        {
            self.disk_dialog = Some(DiskDialog::scan());
        }
    }

    fn activate(&mut self, row_idx: usize) {
        let Some(row) = self.rows.get(row_idx).cloned() else {
            return;
        };
        self.selected = Some(row_idx);
        if row.ft == 2 {
            let base = self.path.trim_end_matches('/');
            let p = if base.is_empty() {
                format!("/{}", row.name)
            } else {
                format!("{}/{}", base, row.name)
            };
            self.navigate_to(&p);
            return;
        }
        let Some(fs) = self.fs.as_ref() else { return };
        match fs.read_inode(row.ino) {
            Ok(inode) => {
                let mut data = Vec::new();
                match read_inode_data(fs, &inode, &mut data) {
                    Ok(()) => {
                        let truncated = data.len() > MAX_TEXT;
                        let text =
                            String::from_utf8_lossy(&data[..data.len().min(MAX_TEXT)]).into_owned();
                        self.content = Some(Content {
                            name: row.name.clone(),
                            data,
                            text,
                            truncated,
                            hex: false,
                            save_msg: None,
                        });
                    }
                    Err(e) => self.error = Some(format!("cannot read {}: {}", row.name, e)),
                }
            }
            Err(e) => self.error = Some(format!("cannot stat {}: {}", row.name, e)),
        }
    }

    fn extract_selected(&mut self) {
        if self.copy_job.is_some() {
            self.status = self.tr("已有复制任务在运行", "A copy job is already running").into();
            return;
        }
        let Some(dir) = rfd::FileDialog::new().pick_folder() else {
            return;
        };
        let dir = dir.to_string_lossy().into_owned();
        let mut items: Vec<(String, PathBuf)> = Vec::new();
        let mut idxs: Vec<usize> = self.sel.iter().cloned().collect();
        idxs.sort_unstable();
        for i in idxs {
            if let Some(r) = self.rows.get(i) {
                items.push((
                    join_fs_path(&self.path, &r.name),
                    Path::new(&dir).join(&r.name),
                ));
            }
        }
        if items.is_empty() {
            self.status = self.tr("未选择要导出的项目", "Nothing selected to extract").into();
            return;
        }
        let n = items.len();
        let msg = self.ftr(
            || format!("已导出 {} 个项目", n),
            || format!("extracted {} item(s)", n),
        );
        self.start_copy_job(items, msg);
    }

    fn start_copy_job(&mut self, items: Vec<(String, PathBuf)>, success_prefix: String) {
        let Some(fs) = self.fs.clone() else {
            return;
        };
        let dest_dir = items
            .first()
            .and_then(|(_, d)| d.parent())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let workers = self.copy_workers(&dest_dir);
        // Totals are unknown yet; computed in the background thread.
        let progress = Arc::new(CopyProgress::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let done: Arc<Mutex<Option<(String, Vec<String>)>>> = Arc::new(Mutex::new(None));

        let (p2, c2, d2) = (progress.clone(), cancel.clone(), done.clone());
        let items2 = items.clone();
        std::thread::spawn(move || {
            // Counting is done off the UI thread so the interface stays smooth.
            let mut total_items = 0u64;
            let mut total_bytes = 0u64;
            for (src, _) in &items2 {
                if let Ok(ino) = fs.resolve(src) {
                    let (i, b) = count_tree_with_bytes(&fs, ino);
                    total_items += i;
                    total_bytes += b;
                } else {
                    total_items += 1;
                }
            }
            p2.set_totals(total_items, total_bytes);

            let mut errors = Vec::new();
            let mut cancelled = false;
            for (src, dest) in &items2 {
                let r = copy_to_parallel(&fs, src, dest, &p2, &c2, workers);
                match r {
                    Ok((_, errs)) => errors.extend(errs),
                    Err(e) => errors.push(format!("{}: {}", src, e)),
                }
                if c2.load(Ordering::Relaxed) {
                    cancelled = true;
                    break;
                }
            }
            let summary = if cancelled {
                format!("{} (cancelled by user)", success_prefix)
            } else {
                format!("{} -> {}", success_prefix, dest_dir_display(&items2))
            };
            *d2.lock().unwrap() = Some((summary, errors));
        });
        self.copy_job = Some(CopyJob {
            progress,
            cancel,
            done,
            workers,
        });
    }

    /// Decide how many copy worker threads to use.
    fn copy_workers(&self, dest_dir: &str) -> usize {
        match self.thread_setting {
            ThreadSetting::N(n) => n.max(1) as usize,
            ThreadSetting::Auto => {
                #[cfg(windows)]
                {
                    use ext4fs_tool::ext4::device::windows::drive_rotational;
                    if let Some(rotational) = drive_rotational(dest_dir) {
                        if rotational {
                            // HDD: parallel writes thrash the heads; sequential wins.
                            return 1;
                        }
                    }
                }
                std::thread::available_parallelism()
                    .map(|p| p.get())
                    .unwrap_or(4)
                    .clamp(2, 8)
            }
        }
    }

    // ------------------------------------------------------------------
    // panels
    // ------------------------------------------------------------------

    fn show_top(&mut self, ui: &mut egui::Ui) {
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("ext4fs-tool").strong().size(16.0));
            ui.separator();
            if ui.button(self.tr("📂 打开镜像", "📂 Open image")).clicked() {
                if let Some(p) = rfd::FileDialog::new()
                    .add_filter("filesystem image", &["img", "bin", "ext2", "ext3", "ext4", "iso"])
                    .pick_file()
                {
                    let p = p.to_string_lossy().into_owned();
                    self.image_path = p.clone();
                    self.open_image(&p);
                }
            }
            if ui.button(self.tr("💿 打开磁盘", "💿 Open disk")).clicked() {
                self.open_disk_requested();
            }
            ui.separator();
            let nsel = self.sel.len();
            ui.add_enabled_ui(self.fs.is_some() && nsel > 0 && self.copy_job.is_none(), |ui| {
                let label = self.ftr(
                    || format!("📋 导出 {} 个选中项", nsel),
                    || format!("📋 Extract {} selected", nsel),
                );
                if ui.button(label).clicked() {
                    self.extract_selected();
                }
            });
            ui.separator();
            if ui.button(self.tr("↑ 上级", "↑ Up")).clicked() {
                self.navigate_to(&parent_path(&self.path));
            }
            if ui.button(self.tr("🏠 根目录", "🏠 Root")).clicked() {
                self.navigate_to("/");
            }
            if ui.button("🔄").on_hover_text(self.tr("刷新", "Refresh")).clicked() {
                self.reload();
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .button("ℹ️")
                    .on_hover_text(self.tr("关于", "About"))
                    .clicked()
                {
                    self.show_about = true;
                }
                ui.menu_button("⚙", |ui| {
                    ui.label(self.tr("复制线程数", "Copy threads"));
                    if ui
                        .selectable_label(
                            self.thread_setting == ThreadSetting::Auto,
                            self.tr("自动（按磁盘类型）", "Auto (by disk type)"),
                        )
                        .clicked()
                    {
                        self.thread_setting = ThreadSetting::Auto;
                        ui.close();
                    }
                    ui.separator();
                    ui.horizontal(|ui| {
                        for n in [1u8, 2, 4, 8, 16] {
                            if ui
                                .selectable_label(self.thread_setting == ThreadSetting::N(n), n.to_string())
                                .clicked()
                            {
                                self.thread_setting = ThreadSetting::N(n);
                                self.custom_threads = n;
                                ui.close();
                            }
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label(self.tr("自定义", "Custom"));
                        let mut v = self.custom_threads;
                        if ui.add(egui::DragValue::new(&mut v).range(1..=64)).changed() {
                            self.custom_threads = v;
                            self.thread_setting = ThreadSetting::N(v);
                        }
                        ui.label(self.tr("线程", "threads"));
                    });
                    ui.separator();
                    ui.weak(self.tr(
                        "提示：机械盘(HDD)建议 1 线程，SSD/NVMe 可多线程。",
                        "Tip: HDD -> 1 thread, SSD/NVMe -> more threads.",
                    ));
                });
                ui.menu_button("🌐", |ui| {
                    if ui.selectable_label(self.lang == Lang::Zh, "简体中文").clicked() {
                        self.lang = Lang::Zh;
                        ui.close();
                    }
                    if ui.selectable_label(self.lang == Lang::En, "English").clicked() {
                        self.lang = Lang::En;
                        ui.close();
                    }
                });
                let icon = match self.theme {
                    ThemePref::Dark => "🌙",
                    ThemePref::Light => "☀",
                    ThemePref::System => "🖥",
                };
                ui.menu_button(format!("{} {}", icon, self.tr("主题", "Theme")), |ui| {
                    for (p, (zh, en)) in [
                        (ThemePref::Dark, ("深色 (Dark)", "Dark")),
                        (ThemePref::Light, ("浅色 (Light)", "Light")),
                        (ThemePref::System, ("跟随系统 (System)", "System")),
                    ] {
                        let label = match self.lang {
                            Lang::Zh => zh,
                            Lang::En => en,
                        };
                        if ui
                            .selectable_label(self.theme == p, label)
                            .on_hover_text(self.tr("立即切换主题", "Switch theme"))
                            .clicked()
                        {
                            self.theme = p;
                            apply_theme(ui.ctx(), p);
                            ui.close();
                        }
                    }
                });
            });
        });

        // breadcrumb navigation
        if self.fs.is_some() {
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.label("📁");
                let mut nav: Option<String> = None;
                self.breadcrumb_ui(ui, &mut nav);
                if let Some(p) = nav {
                    self.navigate_to(&p);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label("🔍");
                    let prev = self.filter.clone();
                    let hint = self.tr("筛选当前目录…", "Filter current dir…");
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.filter)
                            .hint_text(hint)
                            .desired_width(200.0),
                    );
                    if resp.changed() {
                        if self.filter.trim().is_empty() {
                            if !prev.trim().is_empty() {
                                self.reload();
                            }
                        } else {
                            self.filter = self.filter.trim().to_string();
                            self.recompute_shown();
                        }
                    }
                });
            });
        }
        ui.add_space(2.0);
    }

    fn breadcrumb_ui(&mut self, ui: &mut egui::Ui, nav: &mut Option<String>) {
        let comps: Vec<&str> = self.path.split('/').filter(|s| !s.is_empty() && *s != ".").collect();
        let root_sel = self.path == "/";
        if ui.selectable_label(root_sel, egui::RichText::new("/").strong()).clicked() {
            *nav = Some("/".into());
        }
        let mut acc = String::new();
        let total = comps.len();
        for (i, c) in comps.iter().enumerate() {
            acc.push('/');
            acc.push_str(c);
            ui.label(egui::RichText::new("›").color(egui::Color32::GRAY));
            let last = i + 1 == total;
            let text = if last {
                egui::RichText::new(*c).strong()
            } else {
                egui::RichText::new(*c)
            };
            if ui.selectable_label(last, text).clicked() {
                *nav = Some(acc.clone());
            }
        }
    }

    fn show_left(&mut self, ui: &mut egui::Ui) {
        ui.heading(self.tr("目录", "Directories"));
        ui.add_space(4.0);
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            self.render_tree(ui);
        });
    }

    fn show_right(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new(self.tr("文件系统", "Filesystem"))
            .default_open(true)
            .show(ui, |ui| match &self.fs {
                Some(fs) => {
                    let sb = &fs.sb;
                    ui.label(format!("{}: {}", self.tr("卷标", "volume"), sb.volume_name));
                    ui.label(format!("{}: {}", self.tr("大小", "size"), fmt_size(sb.blocks_count * fs.block_size)));
                    ui.label(format!("{}: {} ({} B)", self.tr("块数", "blocks"), sb.blocks_count, fs.block_size));
                    ui.label(format!("{}: {}", self.tr("inode 数", "inodes"), sb.inodes_count));
                    ui.label(format!("{}: {}", self.tr("块组数", "groups"), fs.groups.len()));
                    ui.label(format!("incompat: 0x{:08x}", sb.feature_incompat));
                }
                None => {
                    ui.weak(self.tr("未打开镜像", "No image"));
                }
            });
        ui.separator();
        ui.heading(self.tr("选中项", "Selected"));
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            match &self.fs {
                Some(fs) => match self.selected.and_then(|i| self.rows.get(i)) {
                    Some(r) => match fs.read_inode(r.ino) {
                        Ok(ino) => {
                            ui.label(
                                egui::RichText::new(&r.name)
                                    .strong()
                                    .size(15.0),
                            );
                            ui.add_space(4.0);
                            ui.label(format!("inode: {}", ino.ino));
                            ui.label(format!("{}: {}", self.tr("类型", "type"), {
                                if ino.is_symlink() {
                                    self.tr("符号链接", "symlink")
                                } else if ino.is_dir() {
                                    self.tr("目录", "directory")
                                } else {
                                    self.tr("文件", "file")
                                }
                            }));
                            ui.label(format!("{}: {}", self.tr("大小", "size"), fmt_size(ino.size)));
                            ui.label(format!("{}: {:o}", self.tr("权限", "mode"), ino.mode & 0o7777));
                            ui.label(format!("uid/gid: {}/{}", ino.uid, ino.gid));
                            ui.label(format!("{}: {}", self.tr("链接数", "links"), ino.links_count));
                            ui.label(format!("{}: {} (512B)", self.tr("块", "blocks"), ino.blocks));
                            ui.label(format!("flags: 0x{:08x}", ino.flags));
                            ui.label(format!("mtime: {}", ino.mtime));
                        }
                        Err(e) => {
                            ui.colored_label(egui::Color32::LIGHT_RED, e.to_string());
                        }
                    },
                    None => {
                        ui.weak(self.tr("双击文件查看内容", "Double-click a file to view content"));
                    }
                },
                None => {
                    ui.weak(self.tr("未打开镜像", "No image"));
                }
            }
        });
    }

    fn show_center(&mut self, ui: &mut egui::Ui) {
        if self.fs.is_none() {
            ui.centered_and_justified(|ui| {
                ui.label(
                    egui::RichText::new(self.tr(
                        "打开 ext2/3/4 镜像或磁盘分区开始浏览。",
                        "Open an ext2/3/4 image or disk partition to start.",
                    ))
                    .size(16.0)
                    .color(egui::Color32::GRAY),
                );
            });
            return;
        }
        let shown = self.shown.clone();
        let rows: Vec<RowInfo> = shown.iter().map(|&ri| self.rows[ri].clone()).collect();
        if rows.is_empty() {
            ui.label(self.tr("（空目录）", "(empty directory)"));
        }

        // Whole-table interaction layer: clicks, rubber-band drag and right-click.
        let area = ui.available_rect_before_wrap();
        let marquee_resp = ui.interact(area, ui.id().with("table-marquee"), egui::Sense::click_and_drag());
        let mods = ui.ctx().input(|i| i.modifiers);

        let mut row_rects: Vec<egui::Rect> = Vec::with_capacity(rows.len());
        let mut sort_req: Option<SortCol> = None;
        TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .column(Column::initial(52.0))
            .column(Column::initial(110.0).at_least(70.0))
            .column(Column::remainder().at_least(140.0))
            .header(24.0, |mut header| {
                header.col(|ui| {
                    if sort_header(ui, SortCol::Type, self.sort_col, self.sort_asc).clicked() {
                        sort_req = Some(SortCol::Type);
                    }
                });
                header.col(|ui| {
                    if sort_header(ui, SortCol::Size, self.sort_col, self.sort_asc).clicked() {
                        sort_req = Some(SortCol::Size);
                    }
                });
                header.col(|ui| {
                    if sort_header(ui, SortCol::Name, self.sort_col, self.sort_asc).clicked() {
                        sort_req = Some(SortCol::Name);
                    }
                });
            })
            .body(|mut body| {
                for (i, r) in rows.iter().enumerate() {
                    body.row(20.0, |mut row| {
                        row.set_selected(self.sel.contains(&shown[i]));
                        row.col(|ui| {
                            ui.label(type_label(r.ft));
                        });
                        row.col(|ui| {
                            ui.label(fmt_size(r.size));
                        });
                        row.col(|ui| {
                            ui.label(egui::RichText::new(&r.name).monospace());
                            row_rects.push(ui.max_rect());
                        });
                    });
                }
            });

        if let Some(col) = sort_req {
            if self.sort_col == col {
                self.sort_asc = !self.sort_asc;
            } else {
                self.sort_col = col;
                self.sort_asc = true;
            }
            self.apply_sort();
        }

        // rubber-band selection
        if marquee_resp.drag_started() {
            if let Some(p) = marquee_resp.interact_pointer_pos() {
                self.marquee = Some(Marquee {
                    start: p,
                    current: p,
                    additive: mods.ctrl,
                });
            }
        }
        if let Some(m) = self.marquee.as_mut() {
            if let Some(p) = marquee_resp.interact_pointer_pos() {
                m.current = p;
            }
            let rect = egui::Rect::from_two_pos(m.start, m.current);
            if !m.additive {
                self.sel.clear();
            }
            for (i, rr) in row_rects.iter().enumerate() {
                let real = shown[i];
                if rect.intersects(*rr) {
                    self.sel.insert(real);
                } else if !m.additive {
                    self.sel.remove(&real);
                }
            }
            if marquee_resp.drag_stopped() {
                self.marquee = None;
            }
            let fill = egui::Color32::from_rgba_unmultiplied(70, 160, 220, 36);
            ui.painter().rect_filled(rect, egui::CornerRadius::same(2), fill);
        }

        // single click / double click
        if marquee_resp.clicked() {
            let pos = marquee_resp.interact_pointer_pos().unwrap_or(area.center());
            match row_at(&row_rects, pos).map(|d| shown[d]) {
                Some(real) => self.apply_row_click(real, &mods),
                None => {
                    if !mods.ctrl {
                        self.clear_selection();
                    }
                }
            }
        }
        if marquee_resp.double_clicked() {
            if let Some(real) =
                row_at(&row_rects, marquee_resp.interact_pointer_pos().unwrap_or_default()).map(|d| shown[d])
            {
                self.apply_row_click(real, &mods);
                self.activate(real);
            }
        }

        // right-click context menu (manual, independent of egui hit-testing)
        if ui.ctx().input(|i| i.pointer.button_clicked(egui::PointerButton::Secondary)) {
            let pos = ui.ctx().pointer_latest_pos().unwrap_or_default();
            self.ctx_menu = row_at(&row_rects, pos).map(|d| (shown[d], pos));
            self.ctx_menu_ready = false;
            // right-click selects the item under the cursor if not already selected
            if let Some((real, _)) = self.ctx_menu {
                if !self.sel.contains(&real) {
                    self.select_single(real);
                    self.selected = Some(real);
                }
            }
        }
        self.render_context_menu(ui.ctx());
    }

    fn render_context_menu(&mut self, ctx: &egui::Context) {
        let Some((row_idx, pos)) = self.ctx_menu.take() else {
            return;
        };
        let mut action: Option<CtxAction> = None;
        let mut keep_open = true;

        let resp = egui::Area::new(egui::Id::new("file-context-menu"))
            .order(egui::Order::Foreground)
            .fixed_pos(pos)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    if ui.button(self.tr("复制到文件夹...", "Copy to folder...")).clicked() {
                        action = Some(CtxAction::CopyOne(row_idx));
                    }
                    if ui.button(self.tr("复制到... (重命名)", "Copy to... (rename)")).clicked() {
                        let src = self.rows.get(row_idx).map(|r| join_fs_path(&self.path, &r.name)).unwrap_or_default();
                        let name = self.rows.get(row_idx).map(|r| r.name.clone()).unwrap_or_default();
                        action = Some(CtxAction::Rename(src, name));
                    }
                    if self.sel.len() > 1 {
                        let label = self.ftr(
                            || format!("复制选中的 {} 项到文件夹...", self.sel.len()),
                            || format!("Copy {} selected to folder...", self.sel.len()),
                        );
                        if ui.button(label).clicked() {
                            action = Some(CtxAction::CopyMany);
                        }
                    }
                });
            });
        let menu_rect = resp.response.rect;

        if self.ctx_menu_ready {
            let clicked_outside = ctx.input(|i| {
                i.pointer.any_click() && !i.pointer.latest_pos().map_or(false, |p| menu_rect.contains(p))
            });
            if clicked_outside {
                keep_open = false;
            }
        }
        self.ctx_menu_ready = true;

        let acted = action.is_some();
        match action {
            Some(CtxAction::CopyOne(i)) => self.copy_row_to_folder(i),
            Some(CtxAction::CopyMany) => self.extract_selected(),
            Some(CtxAction::Rename(src, name)) => {
                self.rename_dialog = Some(RenameDialog {
                    src,
                    name,
                    folder: None,
                });
            }
            None => {}
        }
        if keep_open && !acted {
            self.ctx_menu = Some((row_idx, pos));
        }
    }

    fn show_copy_progress(&mut self, ctx: &egui::Context) {
        let Some(job) = self.copy_job.take() else {
            return;
        };
        if let Some((summary, errors)) = job.done.lock().unwrap().clone() {
            let mut msg = summary;
            for e in errors.iter().take(20) {
                msg.push_str(&format!("\n  warn: {}", e));
            }
            if errors.len() > 20 {
                msg.push_str(&format!("\n  ... and {} more", errors.len() - 20));
            }
            self.status = msg;
            return;
        }
        let progress = job.progress.clone();
        let cancel = job.cancel.clone();
        let workers = job.workers;
        let finished = job.done.lock().unwrap().is_some();

        let (zh, en) = (self.tr("复制进度", "Copy progress"), self.tr("取消", "Cancel"));
        let (zh_rem, en_rem) = (self.tr("剩余时间", "Remaining"), self.tr("速度", "Speed"));
        egui::Window::new(zh)
            .default_width(460.0)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.add_space(8.0);
                let frac = progress.fraction();
                ui.add(egui::ProgressBar::new(frac).show_percentage().desired_width(420.0));
                ui.add_space(4.0);
                let done = progress.done();
                let total = progress.total();
                if total == 0 {
                    ui.label(self.tr("正在统计文件…", "Counting files…"));
                } else {
                    let tb = progress.total_bytes();
                    let b = progress.bytes();
                    if tb > 0 {
                        ui.label(format!(
                            "{} / {}    {}: {} / {}",
                            fmt_size(b),
                            fmt_size(tb),
                            self.tr("线程", "threads"),
                            workers,
                            done
                        ));
                    } else {
                        ui.label(format!(
                            "{} / {} {}    {}: {}",
                            done,
                            total,
                            self.tr("项", "items"),
                            self.tr("线程", "threads"),
                            workers
                        ));
                    }
                    if let Some(eta) = progress.eta_seconds() {
                        ui.label(format!("{}: {}", zh_rem, fmt_duration(eta)));
                    }
                    let speed = progress.bytes() as f64 / progress.elapsed_secs().max(0.001);
                    ui.label(format!("{}: {:.1} MB/s", en_rem, speed / 1_000_000.0));
                }
                let cur = progress.current();
                if !cur.is_empty() {
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new(&cur).monospace().size(12.0));
                }
                ui.add_space(10.0);
                if ui.button(en).clicked() {
                    cancel.store(true, Ordering::Relaxed);
                }
            });
        if !finished {
            self.copy_job = Some(job);
            // Keep repainting so the progress bar updates smoothly while the
            // copy runs on background threads (egui otherwise only repaints on
            // input events, which makes progress look stuck).
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
    }

    fn show_rename_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut dlg) = self.rename_dialog.take() else {
            return;
        };
        let mut open = true;
        let mut confirm = false;
        let mut close_clicked = false;
        egui::Window::new(self.tr("复制到 (重命名)", "Copy to (rename)"))
            .default_width(420.0)
            .collapsible(false)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(self.tr("目标文件名:", "Target name:"));
                ui.add(egui::TextEdit::singleline(&mut dlg.name).desired_width(380.0));
                ui.add_space(4.0);
                let folder_label = dlg
                    .folder
                    .as_deref()
                    .map(|f| format!("📁 {}", f))
                    .unwrap_or_else(|| self.tr("选择目标文件夹…", "Choose folder…").to_string());
                if ui.button(folder_label).clicked() {
                    if let Some(p) = rfd::FileDialog::new().pick_folder() {
                        dlg.folder = Some(p.to_string_lossy().into_owned());
                    }
                }
                ui.add_space(8.0);
                let ready = dlg.folder.is_some() && !dlg.name.trim().is_empty();
                ui.horizontal(|ui| {
                    if ui.add_enabled(ready, egui::Button::new(self.tr("复制", "Copy"))).clicked() {
                        confirm = true;
                    }
                    if ui.button(self.tr("取消", "Cancel")).clicked() {
                        close_clicked = true;
                    }
                });
            });
        if close_clicked {
            open = false;
        }
        if confirm {
            if let (Some(folder), name) = (dlg.folder.clone(), dlg.name.trim().to_string()) {
                let dest = Path::new(&folder).join(&name);
                let msg = self.ftr(
                    || format!("已复制 {}", name),
                    || format!("copied {}", name),
                );
                self.start_copy_job(vec![(dlg.src.clone(), dest)], msg);
            }
            open = false;
        }
        if open {
            self.rename_dialog = Some(dlg);
        }
    }

    fn show_about(&mut self, ctx: &egui::Context) {
        if !self.show_about {
            return;
        }
        let title = self.tr("关于", "About").to_string();
        let desc = self
            .tr(
                "只读的 ext2/3/4 文件系统镜像与磁盘浏览器（Rust 编写）。",
                "A read-only ext2/3/4 filesystem image & disk browser written in Rust.",
            )
            .to_string();
        let author = self.tr("作者", "Author").to_string();
        let stack = self.tr("技术栈", "Tech stack").to_string();
        let tech = self
            .tr(
                "Rust · egui/eframe · 纯标准库解析 ext4",
                "Rust · egui/eframe · pure std parsing of ext4",
            )
            .to_string();
        let license = self.tr("许可：MIT", "License: MIT").to_string();
        egui::Window::new(title)
            .default_width(430.0)
            .open(&mut self.show_about)
            .show(ctx, |ui| {
                ui.label(egui::RichText::new("ext4fs-tool").strong().size(20.0));
                ui.label(format!("v{}", env!("CARGO_PKG_VERSION")));
                ui.add_space(8.0);
                ui.label(desc);
                ui.add_space(8.0);
                ui.label(format!("{}: xjimlinx", author));
                ui.hyperlink("https://github.com/xjimlinx/ext4fs-tool");
                ui.add_space(8.0);
                ui.label(stack);
                ui.label(tech);
                ui.add_space(8.0);
                ui.label(license);
            });
    }

    fn apply_row_click(&mut self, i: usize, mods: &egui::Modifiers) {
        if mods.shift {
            match self.anchor {
                Some(a) => self.select_range(a, i),
                None => self.select_single(i),
            }
        } else if mods.ctrl {
            self.toggle_sel(i);
        } else {
            self.select_single(i);
        }
        self.selected = Some(i);
    }

    fn select_single(&mut self, i: usize) {
        self.sel.clear();
        self.sel.insert(i);
        self.anchor = Some(i);
    }

    fn toggle_sel(&mut self, i: usize) {
        if !self.sel.remove(&i) {
            self.sel.insert(i);
        }
        self.anchor = Some(i);
    }

    fn select_range(&mut self, a: usize, b: usize) {
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        self.sel.clear();
        // only the currently visible (filtered) rows in the range get selected
        for &ri in &self.shown {
            if ri >= lo && ri <= hi {
                self.sel.insert(ri);
            }
        }
        self.anchor = Some(b);
    }

    fn clear_selection(&mut self) {
        self.sel.clear();
        self.selected = None;
        self.anchor = None;
    }

    fn apply_sort(&mut self) {
        let col = self.sort_col;
        let asc = self.sort_asc;
        self.rows.sort_by(|a, b| {
            let ord = match col {
                SortCol::Type => a.ft.cmp(&b.ft),
                SortCol::Size => a.size.cmp(&b.size),
                SortCol::Name => a.name.cmp(&b.name),
            };
            if asc {
                ord
            } else {
                ord.reverse()
            }
        });
        self.recompute_shown();
    }

    fn copy_row_to_folder(&mut self, i: usize) {
        if self.copy_job.is_some() {
            self.status = self.tr("已有复制任务在运行", "A copy job is already running").into();
            return;
        }
        let Some(dir) = rfd::FileDialog::new().pick_folder() else {
            return;
        };
        let Some(r) = self.rows.get(i) else {
            return;
        };
        let src = join_fs_path(&self.path, &r.name);
        let dest = dir.join(&r.name);
        let msg = self.ftr(
            || format!("已复制 {}", r.name),
            || format!("copied {}", r.name),
        );
        self.start_copy_job(vec![(src, dest)], msg);
    }

    fn show_bottom(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if self.fs.is_some() {
                let shown = self.shown.len();
                let label = self.ftr(
                    || format!("{} 个项目  {}", shown, self.path),
                    || format!("{} item(s)  {}", shown, self.path),
                );
                ui.label(label);
            } else {
                ui.label(self.tr("未打开镜像", "No image"));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(err) = &self.error {
                    ui.colored_label(egui::Color32::LIGHT_RED, err);
                } else if !self.status.is_empty() {
                    ui.label(egui::RichText::new(&self.status).color(egui::Color32::from_rgb(160, 160, 170)));
                }
            });
        });
    }

    fn show_content(&mut self, ctx: &egui::Context) {
        let Some(mut content) = self.content.take() else {
            return;
        };
        let mut visible = true;
        let mut close_clicked = false;
        let mut save: Option<String> = None;
        egui::Window::new(self.tr("文件内容", "File content"))
            .default_width(620.0)
            .default_height(460.0)
            .resizable(true)
            .open(&mut visible)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(&content.name).strong().size(15.0),
                    );
                    ui.label(format!("({} bytes)", content.data.len()));
                    if content.truncated {
                        let msg = self.ftr(
                            || format!("仅显示前 {} 字节", MAX_TEXT),
                            || format!("showing first {} bytes", MAX_TEXT),
                        );
                        ui.colored_label(egui::Color32::YELLOW, msg);
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button(self.tr("关闭", "Close")).clicked() {
                            close_clicked = true;
                        }
                        if ui.button(self.tr("另存为...", "Save as...")).clicked() {
                            if let Some(p) = rfd::FileDialog::new()
                                .set_file_name(&content.name)
                                .save_file()
                            {
                                save = Some(p.to_string_lossy().into_owned());
                            }
                        }
                        ui.checkbox(&mut content.hex, self.tr("十六进制", "hex"));
                    });
                });
                if let Some(m) = &content.save_msg {
                    ui.label(m);
                }
                ui.separator();
                egui::ScrollArea::vertical()
                    .auto_shrink(false)
                    .max_height(380.0)
                    .show(ui, |ui| {
                        if content.hex {
                            let hex = hex_dump(&content.data[..content.data.len().min(MAX_HEX)]);
                            ui.monospace(hex);
                        if content.data.len() > MAX_HEX {
                            let msg = self.ftr(
                                || format!("（十六进制视图截断于 {} 字节）", MAX_HEX),
                                || format!("(hex view truncated at {} bytes)", MAX_HEX),
                            );
                            ui.label(msg);
                        }
                        } else {
                            ui.add(
                                egui::TextEdit::multiline(&mut content.text)
                                    .font(egui::TextStyle::Monospace)
                                    .desired_rows(24),
                            );
                        }
                    });
            });
        if close_clicked {
            visible = false;
        }
        if let Some(p) = save {
            let msg = match std::fs::write(&p, &content.data) {
                Ok(()) => format!("saved {} bytes to {}", content.data.len(), p),
                Err(e) => format!("save failed: {}", e),
            };
            content.save_msg = Some(msg);
        }
        if visible {
            self.content = Some(content);
        }
    }

    fn show_disk_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.disk_dialog.take() else {
            return;
        };
        let mut open = true;
        let mut open_clicked = false;
        let mut close_clicked = false;
        let mut sel = dialog.selected;
        egui::Window::new(self.tr("物理磁盘与分区", "Disks & partitions"))
            .default_width(680.0)
            .default_height(480.0)
            .resizable(true)
            .open(&mut open)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink(false)
                    .max_height(400.0)
                    .show(ui, |ui| {
                        for (di, d) in dialog.disks.iter().enumerate() {
                            ui.separator();
                            let mut head = d.path.clone();
                            if let Some(m) = &d.model {
                                head.push_str(&format!("   {}", m));
                            }
                            if let Some(sz) = d.size {
                                head.push_str(&format!("   ({} bytes)", fmt_size(sz)));
                            }
                            ui.label(egui::RichText::new(head).strong().color(egui::Color32::from_rgb(120, 180, 255)));
                            if let Some(err) = &d.error {
                                ui.colored_label(
                                    egui::Color32::LIGHT_RED,
                                    format!("{}  (run as administrator to read raw disks)", err),
                                );
                            }
                            if let Some(parts) = &d.parts {
                                if parts.is_empty() {
                                    ui.weak(self.tr("无分区", "no partitions"));
                                }
                                for (pi, p) in parts.iter().enumerate() {
                                    let label = format!(
                                        "#{}   {}   fs: {}   label: {}   {}   {}",
                                        p.index,
                                        fmt_size(p.sectors * d.sector_size),
                                        p.fs,
                                        if p.label.is_empty() { "-" } else { &p.label },
                                        p.name,
                                        p.kind
                                    );
                                    if ui.radio(sel == Some((di, pi)), label).clicked() {
                                        sel = Some((di, pi));
                                    }
                                }
                            }
                        }
                    });
                ui.separator();
                ui.horizontal(|ui| {
                    if sel.is_some() {
                        if ui.button(self.tr("打开选中分区", "Open selected partition")).clicked() {
                            open_clicked = true;
                        }
                    } else {
                        ui.add_enabled(false, egui::Button::new(self.tr("打开选中分区", "Open selected partition")));
                    }
                    if ui.button(self.tr("关闭", "Close")).clicked() {
                        close_clicked = true;
                    }
                });
            });
        if close_clicked {
            open = false;
        }
        dialog.selected = sel;
        if open_clicked {
            dialog.open_selected(self);
        }
        if open {
            self.disk_dialog = Some(dialog);
        }
    }
}

impl eframe::App for GuiApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Ctrl+A: select all visible items
        if ui.ctx().input(|i| i.modifiers.command && i.key_pressed(egui::Key::A)) && self.fs.is_some() {
            self.sel = self.shown.iter().cloned().collect();
            self.selected = self.shown.first().copied();
        }
        egui::Panel::top("top").show(ui, |ui| self.show_top(ui));
        egui::Panel::left("left")
            .resizable(true)
            .default_size(220.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| self.show_left(ui));
            });
        egui::Panel::right("right")
            .resizable(true)
            .default_size(260.0)
            .show(ui, |ui| self.show_right(ui));
        egui::Panel::bottom("bottom").show(ui, |ui| self.show_bottom(ui));
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::both().show(ui, |ui| self.show_center(ui));
        });
        let ctx = ui.ctx().clone();
        self.show_content(&ctx);
        self.show_disk_dialog(&ctx);
        self.show_copy_progress(&ctx);
        self.show_rename_dialog(&ctx);
        self.show_about(&ctx);
    }
}

// ---------------------------------------------------------------------------
// Administrator elevation helpers
// ---------------------------------------------------------------------------

#[cfg(windows)]
fn is_admin() -> bool {
    std::fs::File::open("\\\\.\\PhysicalDrive0").is_ok()
}

/// Relaunch this executable with a UAC elevation prompt (run as administrator).
#[cfg(windows)]
fn relaunch_elevated(args: &str) -> bool {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;

    #[link(name = "shell32")]
    extern "system" {
        fn ShellExecuteW(
            hwnd: *mut c_void,
            operation: *const u16,
            file: *const u16,
            parameters: *const u16,
            directory: *const u16,
            show: i32,
        ) -> isize;
    }

    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => return false,
    };
    let exe_w: Vec<u16> = exe.as_os_str().encode_wide().chain(Some(0)).collect();
    let op_w: Vec<u16> = "runas".encode_utf16().chain(Some(0)).collect();
    let args_w: Vec<u16> = args.encode_utf16().chain(Some(0)).collect();
    let r = unsafe {
        ShellExecuteW(
            ptr::null_mut(),
            op_w.as_ptr(),
            exe_w.as_ptr(),
            args_w.as_ptr(),
            ptr::null(),
            1, // SW_SHOWNORMAL
        )
    };
    r > 32
}

// ---------------------------------------------------------------------------
// Disk / partition browser
// ---------------------------------------------------------------------------

impl DiskDialog {
    fn scan() -> Self {
        let mut disks = Vec::new();
        #[cfg(windows)]
        {
            use ext4fs_tool::ext4::device::windows::{enumerate_disks, sector_size_of};
            use ext4fs_tool::ext4::partitions::{detect_fs, detect_fs_label, read_partition_table, PartKind};
            for d in enumerate_disks() {
                if let Some(err) = &d.error {
                    disks.push(DiskEntry {
                        path: d.path,
                        size: None,
                        model: None,
                        sector_size: 512,
                        parts: None,
                        error: Some(err.clone()),
                    });
                    continue;
                }
                match std::fs::File::open(&d.path) {
                    Ok(mut f) => {
                        let sector = sector_size_of(&f);
                        let parts = read_partition_table(&mut f, sector)
                            .map(|t| {
                                t.partitions
                                    .iter()
                                    .map(|p| {
                                        let kind = match &p.kind {
                                            PartKind::Mbr(t) => format!("MBR 0x{:02x}", t),
                                            PartKind::Gpt(_) => "GPT".to_string(),
                                        };
                                        let start = p.start_bytes(sector);
                                        PartRow {
                                            index: p.index,
                                            start_bytes: start,
                                            sectors: p.sectors,
                                            name: p.name.clone(),
                                            kind,
                                            fs: detect_fs(&mut f, start),
                                            label: detect_fs_label(&mut f, start),
                                        }
                                    })
                                    .collect()
                            })
                            .map_err(|e| e.to_string());
                        let (parts, error) = match parts {
                            Ok(p) => (Some(p), None),
                            Err(e) => (None, Some(e)),
                        };
                        disks.push(DiskEntry {
                            path: d.path,
                            size: d.size,
                            model: d.model,
                            sector_size: sector,
                            parts,
                            error,
                        });
                    }
                    Err(e) => disks.push(DiskEntry {
                        path: d.path,
                        size: d.size,
                        model: d.model,
                        sector_size: 512,
                        parts: None,
                        error: Some(e.to_string()),
                    }),
                }
            }
        }
        #[cfg(not(windows))]
        {
            let _ = disks;
        }
        DiskDialog {
            disks,
            selected: None,
        }
    }

    fn open_selected(&mut self, app: &mut GuiApp) {
        let Some((di, pi)) = self.selected else {
            return;
        };
        let Some(d) = self.disks.get(di) else {
            return;
        };
        let Some(parts) = &d.parts else {
            return;
        };
        let Some(p) = parts.get(pi) else {
            return;
        };
        let offset = p.start_bytes;
        app.open_image_with_offset(&d.path, offset);
        self.selected = None;
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn style(ctx: &egui::Context) {
    apply_spacing(ctx);
    apply_theme(ctx, ThemePref::Dark);
    install_cjk_fonts(ctx);
}

fn apply_spacing(ctx: &egui::Context) {
    for theme in [egui::Theme::Dark, egui::Theme::Light] {
        ctx.style_mut_of(theme, |style| {
            style.spacing.item_spacing = egui::vec2(8.0, 6.0);
            style.spacing.button_padding = egui::vec2(10.0, 4.0);
            style.spacing.window_margin = egui::Margin::same(10);
            style.spacing.menu_margin = egui::Margin::same(6);
            style.spacing.indent = 12.0;
            style.spacing.interact_size = egui::vec2(26.0, 22.0);
            style.interaction.resize_grab_radius_side = 6.0;
            style.interaction.resize_grab_radius_corner = 8.0;
            style.text_styles.insert(
                egui::TextStyle::Heading,
                egui::FontId::new(17.0, egui::FontFamily::Proportional),
            );
            style.text_styles.insert(
                egui::TextStyle::Body,
                egui::FontId::new(14.0, egui::FontFamily::Proportional),
            );
            style.text_styles.insert(
                egui::TextStyle::Button,
                egui::FontId::new(14.0, egui::FontFamily::Proportional),
            );
            style.text_styles.insert(
                egui::TextStyle::Monospace,
                egui::FontId::new(13.5, egui::FontFamily::Monospace),
            );
        });
    }
}

fn apply_theme(ctx: &egui::Context, pref: ThemePref) {
    // Configure both palettes up-front so switching is instant.
    ctx.set_visuals_of(egui::Theme::Dark, build_visuals(true));
    ctx.set_visuals_of(egui::Theme::Light, build_visuals(false));
    match pref {
        ThemePref::Dark => ctx.set_theme(egui::ThemePreference::Dark),
        ThemePref::Light => ctx.set_theme(egui::ThemePreference::Light),
        ThemePref::System => ctx.set_theme(egui::ThemePreference::System),
    }
    ctx.set_zoom_factor(1.0);
}

fn build_visuals(dark: bool) -> egui::Visuals {
    let mut v = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    let corner = egui::CornerRadius::same(5);
    if dark {
        v.panel_fill = egui::Color32::from_rgb(30, 31, 36);
        v.window_fill = egui::Color32::from_rgb(37, 38, 44);
        v.extreme_bg_color = egui::Color32::from_rgb(22, 23, 27);
        v.faint_bg_color = egui::Color32::from_rgb(37, 39, 45);
        v.code_bg_color = egui::Color32::from_rgb(28, 30, 36);
        v.window_stroke = egui::Stroke::new(1.0, egui::Color32::from_rgb(56, 58, 68));
        v.window_corner_radius = egui::CornerRadius::same(8);
        v.selection.bg_fill = egui::Color32::from_rgb(40, 96, 134);
        v.selection.stroke = egui::Stroke::new(1.0, egui::Color32::from_rgb(90, 175, 230));
        v.hyperlink_color = egui::Color32::from_rgb(100, 185, 240);
        v.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, egui::Color32::from_rgb(200, 203, 210));
        v.widgets.inactive.bg_fill = egui::Color32::from_rgb(52, 55, 63);
        v.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, egui::Color32::from_rgb(222, 225, 232));
        v.widgets.hovered.bg_fill = egui::Color32::from_rgb(68, 74, 86);
        v.widgets.hovered.fg_stroke = egui::Stroke::new(1.0, egui::Color32::WHITE);
        v.widgets.active.bg_fill = egui::Color32::from_rgb(42, 100, 140);
        v.widgets.active.fg_stroke = egui::Stroke::new(1.0, egui::Color32::WHITE);
        v.widgets.open.bg_fill = egui::Color32::from_rgb(48, 66, 80);
    } else {
        v.panel_fill = egui::Color32::from_rgb(241, 243, 246);
        v.window_fill = egui::Color32::from_rgb(255, 255, 255);
        v.extreme_bg_color = egui::Color32::from_rgb(244, 245, 247);
        v.faint_bg_color = egui::Color32::from_rgb(246, 248, 250);
        v.code_bg_color = egui::Color32::from_rgb(245, 246, 248);
        v.window_stroke = egui::Stroke::new(1.0, egui::Color32::from_rgb(214, 218, 224));
        v.window_corner_radius = egui::CornerRadius::same(8);
        v.selection.bg_fill = egui::Color32::from_rgb(201, 224, 247);
        v.selection.stroke = egui::Stroke::new(1.0, egui::Color32::from_rgb(0, 102, 204));
        v.hyperlink_color = egui::Color32::from_rgb(0, 102, 204);
        v.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, egui::Color32::from_rgb(62, 66, 72));
        v.widgets.inactive.bg_fill = egui::Color32::from_rgb(228, 231, 236);
        v.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, egui::Color32::from_rgb(30, 33, 38));
        v.widgets.hovered.bg_fill = egui::Color32::from_rgb(229, 242, 255);
        v.widgets.hovered.fg_stroke = egui::Stroke::new(1.0, egui::Color32::from_rgb(10, 12, 14));
        v.widgets.active.bg_fill = egui::Color32::from_rgb(201, 224, 247);
        v.widgets.active.fg_stroke = egui::Stroke::new(1.0, egui::Color32::from_rgb(0, 20, 40));
        v.widgets.open.bg_fill = egui::Color32::from_rgb(235, 240, 245);
    }
    for w in [&mut v.widgets.noninteractive, &mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
        w.corner_radius = corner;
    }
    v
}

/// Load a Windows CJK font so Chinese/UTF-8 text (e.g. file names) renders
/// correctly. Tries a list of common system fonts.
fn install_cjk_fonts(ctx: &egui::Context) {
    const FONTS: [&str; 6] = [
        "C:\\Windows\\Fonts\\msyh.ttc",   // 微软雅黑
        "C:\\Windows\\Fonts\\simhei.ttf", // 黑体
        "C:\\Windows\\Fonts\\Deng.ttf",   // 等线
        "C:\\Windows\\Fonts\\simsun.ttc", // 宋体
        "C:\\Windows\\Fonts\\kaiu.ttf",   // 楷体
        "C:\\Windows\\Fonts\\msjh.ttc",   // 微软正黑体
    ];
    for path in FONTS {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        let name = format!("cjk-{}", std::path::Path::new(path).file_name().unwrap().to_string_lossy());
        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert(name.clone(), egui::FontData::from_owned(bytes).into());
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts.families.entry(family).or_default().push(name.clone());
        }
        ctx.set_fonts(fonts);
        return;
    }
}

fn load_entries(fs: &Ext4, path: &str) -> Result<Vec<RowInfo>, String> {
    let ino = fs.resolve(path).map_err(|e| e.to_string())?;
    let inode = fs.read_inode(ino).map_err(|e| e.to_string())?;
    if !inode.is_dir() {
        return Err("path is not a directory".into());
    }
    let mut es = fs.list_dir(&inode).map_err(|e| e.to_string())?;
    es.sort_by(|a, b| a.name.cmp(&b.name));
    let inos: Vec<u32> = es.iter().map(|e| e.ino).collect();
    let inodes = fs.read_inodes_batch(&inos);
    let mut rows = Vec::with_capacity(es.len());
    for (e, ino_opt) in es.into_iter().zip(inodes.into_iter()) {
        rows.push(RowInfo {
            ino: e.ino,
            ft: e.file_type,
            size: ino_opt.map(|i| i.size).unwrap_or(0),
            name: e.name,
        });
    }
    Ok(rows)
}

fn parent_path(path: &str) -> String {
    let p = path.trim_end_matches('/');
    match p.rfind('/') {
        Some(0) => "/".into(),
        Some(i) => p[..i].into(),
        None => "/".into(),
    }
}

/// A clickable sortable table header cell.
fn sort_header(ui: &mut egui::Ui, col: SortCol, active: SortCol, asc: bool) -> egui::Response {
    let name = match col {
        SortCol::Type => "Type",
        SortCol::Size => "Size",
        SortCol::Name => "Name",
    };
    let arrow = if active == col {
        if asc {
            " ▲"
        } else {
            " ▼"
        }
    } else {
        ""
    };
    ui.selectable_label(active == col, format!("{}{}", name, arrow))
}

/// Join a filesystem path component onto a directory path.
fn join_fs_path(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{}{}", dir, name)
    } else {
        format!("{}/{}", dir, name)
    }
}

/// Show the common destination folder of a batch of copy items.
fn dest_dir_display(items: &[(String, PathBuf)]) -> String {
    items
        .first()
        .and_then(|(_, d)| d.parent())
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}

/// Find the row whose rect contains `pos`.
fn row_at(rects: &[egui::Rect], pos: egui::Pos2) -> Option<usize> {
    rects.iter().position(|r| r.contains(pos))
}

fn type_label(ft: u8) -> egui::RichText {
    match ft {
        2 => egui::RichText::new("dir").color(egui::Color32::from_rgb(120, 180, 255)),
        7 => egui::RichText::new("link").color(egui::Color32::from_rgb(255, 200, 90)),
        3 => egui::RichText::new("char").color(egui::Color32::from_rgb(170, 140, 255)),
        4 => egui::RichText::new("blk").color(egui::Color32::from_rgb(170, 140, 255)),
        5 => egui::RichText::new("fifo").color(egui::Color32::from_rgb(170, 140, 255)),
        6 => egui::RichText::new("sock").color(egui::Color32::from_rgb(170, 140, 255)),
        _ => egui::RichText::new("file").color(egui::Color32::from_rgb(150, 165, 180)),
    }
}

fn fmt_size(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    if n < 1024 {
        return format!("{} B", n);
    }
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if v >= 100.0 {
        format!("{:.0} {}", v, UNITS[u])
    } else {
        format!("{:.1} {}", v, UNITS[u])
    }
}

fn fmt_duration(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{}:{:02}:{:02}", h, m, s)
    } else {
        format!("{}:{:02}", m, s)
    }
}

fn hex_dump(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 4);
    let mut line = [0u8; 16];
    for (i, chunk) in data.chunks(16).enumerate() {
        let n = chunk.len();
        line[..n].copy_from_slice(chunk);
        let mut s = format!("{:08x}  ", i * 16);
        for j in 0..16 {
            if j < n {
                s.push_str(&format!("{:02x} ", line[j]));
            } else {
                s.push_str("   ");
            }
            if j == 7 {
                s.push(' ');
            }
        }
        s.push_str(" |");
        for j in 0..n {
            let c = line[j];
            s.push(if (32..127).contains(&c) { c as char } else { '.' });
        }
        s.push('|');
        out.push_str(&s);
        out.push('\n');
    }
    out
}
