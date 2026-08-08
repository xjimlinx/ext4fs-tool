//! ext4fs-tool GUI — a read-only ext2/3/4 image/disk browser.
//!
//! Build:  cargo build --bin ext4fs-tool-gui
//! Run:    target/debug/ext4fs-tool-gui

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use egui_extras::{Column, TableBuilder};
use ext4fs_tool::ext4::copy::{count_tree, extract_with_progress, CopyProgress};
use ext4fs_tool::ext4::inode::{read_inode_data, EXT4_ROOT_INO};
use ext4fs_tool::ext4::Ext4;
use std::collections::{HashMap, HashSet};
use std::path::Path;
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
}

struct PartRow {
    index: u32,
    start_bytes: u64,
    sectors: u64,
    name: String,
    kind: String,
    fs: String,
}

struct DiskEntry {
    path: String,
    size: Option<u64>,
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
    // directory tree state
    tree_cache: HashMap<u32, Vec<TreeNode>>,
    tree_expanded: HashSet<u32>,
}

impl GuiApp {
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
        self.expand_path();
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
            ui.weak("no image");
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
                egui::RichText::new(&node.name).color(egui::Color32::from_rgb(120, 180, 255))
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
            self.status = "a copy job is already running".into();
            return;
        }
        let Some(dir) = rfd::FileDialog::new().pick_folder() else {
            return;
        };
        let mut items: Vec<(String, String)> = Vec::new();
        let mut idxs: Vec<usize> = self.sel.iter().cloned().collect();
        idxs.sort_unstable();
        for i in idxs {
            if let Some(r) = self.rows.get(i) {
                items.push((join_fs_path(&self.path, &r.name), r.name.clone()));
            }
        }
        if items.is_empty() {
            self.status = "nothing selected to extract".into();
            return;
        }
        let n = items.len();
        self.start_copy_job(items, &dir.to_string_lossy(), format!("extracted {} item(s)", n));
    }

    fn start_copy_job(&mut self, items: Vec<(String, String)>, dest_dir: &str, success_prefix: String) {
        let Some(fs) = self.fs.clone() else {
            return;
        };
        let mut total = 0u64;
        for (src, _) in &items {
            if let Ok(ino) = fs.resolve(src) {
                total += count_tree(&fs, ino);
            } else {
                total += 1;
            }
        }
        let progress = Arc::new(CopyProgress::new(total));
        let cancel = Arc::new(AtomicBool::new(false));
        let done: Arc<Mutex<Option<(String, Vec<String>)>>> = Arc::new(Mutex::new(None));

        let (p2, c2, d2) = (progress.clone(), cancel.clone(), done.clone());
        let dest = dest_dir.to_string();
        let items2 = items.clone();
        std::thread::spawn(move || {
            let mut errors = Vec::new();
            let mut cancelled = false;
            for (src, _name) in items2 {
                let r = extract_with_progress(&fs, &src, Path::new(&dest), &p2, &c2);
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
                format!("{} -> {}", success_prefix, dest)
            };
            *d2.lock().unwrap() = Some((summary, errors));
        });
        self.copy_job = Some(CopyJob {
            progress,
            cancel,
            done,
        });
    }

    // ------------------------------------------------------------------
    // panels
    // ------------------------------------------------------------------

    fn show_top(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("ext4fs-tool");
            ui.separator();
            if ui.button("Open image").clicked() {
                if let Some(p) = rfd::FileDialog::new()
                    .add_filter("filesystem image", &["img", "bin", "ext2", "ext3", "ext4", "iso"])
                    .pick_file()
                {
                    let p = p.to_string_lossy().into_owned();
                    self.image_path = p.clone();
                    self.open_image(&p);
                }
            }
            if ui.button("Open disk").clicked() {
                self.open_disk_requested();
            }
            ui.separator();
            let nsel = self.sel.len();
            ui.add_enabled_ui(self.fs.is_some() && nsel > 0, |ui| {
                if ui.button(format!("Extract {} selected", nsel)).clicked() {
                    self.extract_selected();
                }
            });
            ui.separator();
            ui.label("Path:");
            let resp = ui.add(egui::TextEdit::singleline(&mut self.path).desired_width(300.0));
            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                self.reload();
            }
            if ui.button("Go").clicked() {
                self.reload();
            }
            if ui.button("Up").clicked() {
                self.navigate_to(&parent_path(&self.path));
            }
            if ui.button("Root").clicked() {
                self.navigate_to("/");
            }
            if ui.button("⟳").clicked() {
                self.reload();
            }
        });
        ui.add_space(2.0);
    }

    fn show_left(&mut self, ui: &mut egui::Ui) {
        ui.heading("Directories");
        ui.add_space(4.0);
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            self.render_tree(ui);
        });
    }

    fn show_right(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("Filesystem")
            .default_open(true)
            .show(ui, |ui| match &self.fs {
                Some(fs) => {
                    let sb = &fs.sb;
                    ui.label(format!("volume: {}", sb.volume_name));
                    ui.label(format!("size: {}", fmt_size(sb.blocks_count * fs.block_size)));
                    ui.label(format!("blocks: {} ({} B)", sb.blocks_count, fs.block_size));
                    ui.label(format!("inodes: {}", sb.inodes_count));
                    ui.label(format!("groups: {}", fs.groups.len()));
                    ui.label(format!("incompat: 0x{:08x}", sb.feature_incompat));
                }
                None => {
                    ui.weak("no image");
                }
            });
        ui.separator();
        ui.heading("Selected");
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
                            ui.label(format!(
                                "type: {}",
                                if ino.is_symlink() {
                                    "symlink"
                                } else if ino.is_dir() {
                                    "directory"
                                } else {
                                    "file"
                                }
                            ));
                            ui.label(format!("size: {}", fmt_size(ino.size)));
                            ui.label(format!("mode: {:o}", ino.mode & 0o7777));
                            ui.label(format!("uid/gid: {}/{}", ino.uid, ino.gid));
                            ui.label(format!("links: {}", ino.links_count));
                            ui.label(format!("blocks: {} (512B)", ino.blocks));
                            ui.label(format!("flags: 0x{:08x}", ino.flags));
                            ui.label(format!("mtime: {}", ino.mtime));
                        }
                        Err(e) => {
                            ui.colored_label(egui::Color32::LIGHT_RED, e.to_string());
                        }
                    },
                    None => {
                        ui.weak("double-click a file to\nview its content");
                    }
                },
                None => {
                    ui.weak("no image");
                }
            }
        });
    }

    fn show_center(&mut self, ui: &mut egui::Ui) {
        if self.fs.is_none() {
            ui.centered_and_justified(|ui| {
                ui.label(
                    egui::RichText::new("Open an ext2/3/4 filesystem image or disk partition to start.")
                        .size(16.0)
                        .color(egui::Color32::GRAY),
                );
            });
            return;
        }
        let rows = self.rows.clone();
        if rows.is_empty() {
            ui.label("(empty directory)");
        }

        // Whole-table interaction layer: clicks, rubber-band drag and right-click.
        let area = ui.available_rect_before_wrap();
        let marquee_resp = ui.interact(area, ui.id().with("table-marquee"), egui::Sense::click_and_drag());
        let mods = ui.ctx().input(|i| i.modifiers);

        let mut row_rects: Vec<egui::Rect> = Vec::with_capacity(rows.len());
        TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .column(Column::initial(48.0))
            .column(Column::initial(110.0).at_least(70.0))
            .column(Column::remainder().at_least(140.0))
            .header(22.0, |mut header| {
                header.col(|ui| {
                    ui.strong("Type");
                });
                header.col(|ui| {
                    ui.strong("Size");
                });
                header.col(|ui| {
                    ui.strong("Name");
                });
            })
            .body(|mut body| {
                for (i, r) in rows.iter().enumerate() {
                    body.row(20.0, |mut row| {
                        row.set_selected(self.sel.contains(&i));
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
                if rect.intersects(*rr) {
                    self.sel.insert(i);
                } else if !m.additive {
                    self.sel.remove(&i);
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
            match row_at(&row_rects, pos) {
                Some(i) => self.apply_row_click(i, &mods),
                None => {
                    if !mods.ctrl {
                        self.clear_selection();
                    }
                }
            }
        }
        if marquee_resp.double_clicked() {
            if let Some(i) = row_at(&row_rects, marquee_resp.interact_pointer_pos().unwrap_or_default()) {
                self.apply_row_click(i, &mods);
                self.activate(i);
            }
        }

        // right-click context menu (manual, independent of egui hit-testing)
        if ui.ctx().input(|i| i.pointer.button_clicked(egui::PointerButton::Secondary)) {
            let pos = ui.ctx().pointer_latest_pos().unwrap_or_default();
            self.ctx_menu = row_at(&row_rects, pos).map(|i| (i, pos));
            self.ctx_menu_ready = false;
            // right-click selects the item under the cursor if not already selected
            if let Some((i, _)) = self.ctx_menu {
                if !self.sel.contains(&i) {
                    self.select_single(i);
                    self.selected = Some(i);
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
                    if ui.button("Copy to folder...").clicked() {
                        action = Some(CtxAction::CopyOne(row_idx));
                    }
                    if self.sel.len() > 1 {
                        if ui.button(format!("Copy {} selected to folder...", self.sel.len()))
                            .clicked()
                        {
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

        match action {
            Some(CtxAction::CopyOne(i)) => self.copy_row_to_folder(i),
            Some(CtxAction::CopyMany) => self.extract_selected(),
            None => {}
        }
        if keep_open && action.is_none() {
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
        let finished = job.done.lock().unwrap().is_some();
        egui::Window::new("Copy progress")
            .default_width(460.0)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.add_space(8.0);
                let frac = progress.fraction();
                ui.add(egui::ProgressBar::new(frac).show_percentage().desired_width(420.0));
                ui.add_space(4.0);
                ui.label(format!("{} / {} items", progress.done(), progress.total()));
                let cur = progress.current();
                if !cur.is_empty() {
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new(&cur).monospace().size(12.0));
                }
                ui.add_space(10.0);
                if ui.button("Cancel").clicked() {
                    cancel.store(true, Ordering::Relaxed);
                }
            });
        if !finished {
            self.copy_job = Some(job);
        }
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
        for k in lo..=hi {
            self.sel.insert(k);
        }
        self.anchor = Some(b);
    }

    fn clear_selection(&mut self) {
        self.sel.clear();
        self.selected = None;
        self.anchor = None;
    }

    fn copy_row_to_folder(&mut self, i: usize) {
        if self.copy_job.is_some() {
            self.status = "a copy job is already running".into();
            return;
        }
        let Some(dir) = rfd::FileDialog::new().pick_folder() else {
            return;
        };
        let Some(r) = self.rows.get(i) else {
            return;
        };
        let src = join_fs_path(&self.path, &r.name);
        self.start_copy_job(
            vec![(src, r.name.clone())],
            &dir.to_string_lossy(),
            format!("copied {}", r.name),
        );
    }

    fn show_bottom(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if self.fs.is_some() {
                ui.label(format!("{} item(s)   {}", self.rows.len(), self.path));
            } else {
                ui.label("no image");
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
        egui::Window::new("File content")
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
                        ui.colored_label(
                            egui::Color32::YELLOW,
                            format!("showing first {} bytes", MAX_TEXT),
                        );
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Close").clicked() {
                            close_clicked = true;
                        }
                        if ui.button("Save as...").clicked() {
                            if let Some(p) = rfd::FileDialog::new()
                                .set_file_name(&content.name)
                                .save_file()
                            {
                                save = Some(p.to_string_lossy().into_owned());
                            }
                        }
                        ui.checkbox(&mut content.hex, "hex");
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
                                ui.label(format!("(hex view truncated at {} bytes)", MAX_HEX));
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
        egui::Window::new("Physical disks & partitions")
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
                            let mut head = format!("{}", d.path);
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
                                    ui.weak("no partitions");
                                }
                                for (pi, p) in parts.iter().enumerate() {
                                    let label = format!(
                                        "#{}   {}   fs: {}   {}   {}",
                                        p.index,
                                        fmt_size(p.sectors * d.sector_size),
                                        p.fs,
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
                        if ui.button("Open selected partition").clicked() {
                            open_clicked = true;
                        }
                    } else {
                        ui.add_enabled(false, egui::Button::new("Open selected partition"));
                    }
                    if ui.button("Close").clicked() {
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
            use ext4fs_tool::ext4::partitions::{detect_fs, read_partition_table, PartKind};
            for d in enumerate_disks() {
                if let Some(err) = &d.error {
                    disks.push(DiskEntry {
                        path: d.path,
                        size: None,
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
                                        PartRow {
                                            index: p.index,
                                            start_bytes: p.start_bytes(sector),
                                            sectors: p.sectors,
                                            name: p.name.clone(),
                                            kind,
                                            fs: detect_fs(&mut f, p.start_bytes(sector)),
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
                            sector_size: sector,
                            parts,
                            error,
                        });
                    }
                    Err(e) => disks.push(DiskEntry {
                        path: d.path,
                        size: d.size,
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
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = egui::Color32::from_rgb(32, 34, 40);
    visuals.window_fill = egui::Color32::from_rgb(38, 40, 48);
    visuals.extreme_bg_color = egui::Color32::from_rgb(24, 26, 32);
    visuals.selection.bg_fill = egui::Color32::from_rgb(38, 96, 128);
    visuals.selection.stroke = egui::Stroke::new(1.0, egui::Color32::from_rgb(70, 160, 200));
    visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(52, 58, 70);
    visuals.widgets.active.bg_fill = egui::Color32::from_rgb(46, 84, 104);
    ctx.set_visuals(visuals);
    ctx.set_zoom_factor(1.05);
    install_cjk_fonts(ctx);
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
    let mut rows = Vec::with_capacity(es.len());
    for e in es {
        let size = fs.read_inode(e.ino).map(|i| i.size).unwrap_or(0);
        rows.push(RowInfo {
            ino: e.ino,
            ft: e.file_type,
            size,
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

/// Join a filesystem path component onto a directory path.
fn join_fs_path(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{}{}", dir, name)
    } else {
        format!("{}/{}", dir, name)
    }
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
