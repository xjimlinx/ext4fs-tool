//! Copy files and directories out of an ext filesystem (read-only, best effort),
//! with optional progress reporting and cancellation.

use crate::ext4::error::Result;
use crate::ext4::inode::read_inode_data;
use crate::ext4::Ext4;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

/// Shared progress state, updated from the worker thread and read by the UI.
pub struct CopyProgress {
    total: u64,
    done: AtomicU64,
    current: Mutex<String>,
}

impl CopyProgress {
    pub fn new(total: u64) -> Self {
        Self {
            total,
            done: AtomicU64::new(0),
            current: Mutex::new(String::new()),
        }
    }
    pub fn total(&self) -> u64 {
        self.total
    }
    pub fn done(&self) -> u64 {
        self.done.load(Ordering::Relaxed)
    }
    pub fn fraction(&self) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            self.done.load(Ordering::Relaxed) as f32 / self.total as f32
        }
    }
    pub fn current(&self) -> String {
        self.current.lock().map(|s| s.clone()).unwrap_or_default()
    }
    fn tick(&self, path: &str) {
        *self.current.lock().unwrap() = path.to_string();
        self.done.fetch_add(1, Ordering::Relaxed);
    }
}

fn basename_of(src_path: &str) -> String {
    src_path
        .trim_end_matches('/')
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or("root")
        .to_string()
}

/// Extract `src_path` (file or directory) into `dest_dir`, preserving the
/// basename and the directory structure. Returns the destination created and a
/// list of per-item warnings. Errors that prevent the whole operation are
/// returned as `Err`.
pub fn extract(fs: &Ext4, src_path: &str, dest_dir: &Path) -> Result<(PathBuf, Vec<String>)> {
    let ino = fs.resolve(src_path)?;
    let name = basename_of(src_path);
    let out_display = dest_dir.join(&name);
    let base = long_path(dest_dir);
    let mut errors = Vec::new();
    let cancel = AtomicBool::new(false);
    extract_node(fs, ino, src_path, &base.join(&name), 0, &mut errors, None, &cancel);
    Ok((out_display, errors))
}

/// Like [`extract`] but reports progress and can be cancelled. `prog.total`
/// should be the value from [`count_tree`]; the caller is expected to count
/// first so the progress bar has a total.
pub fn extract_with_progress(
    fs: &Ext4,
    src_path: &str,
    dest_dir: &Path,
    prog: &CopyProgress,
    cancel: &AtomicBool,
) -> Result<(PathBuf, Vec<String>)> {
    let ino = fs.resolve(src_path)?;
    let name = basename_of(src_path);
    let out_display = dest_dir.join(&name);
    let base = long_path(dest_dir);
    let mut errors = Vec::new();
    extract_node(fs, ino, src_path, &base.join(&name), 0, &mut errors, Some(prog), cancel);
    Ok((out_display, errors))
}

/// Count the number of items (files, dirs, symlinks) reachable below `ino`,
/// excluding "." and "..". Best effort.
pub fn count_tree(fs: &Ext4, ino: u32) -> u64 {
    fn rec(fs: &Ext4, ino: u32, depth: u32, acc: &mut u64) {
        if depth > 64 {
            return;
        }
        let inode = match fs.read_inode(ino) {
            Ok(i) => i,
            Err(_) => {
                *acc += 1;
                return;
            }
        };
        if inode.is_dir() {
            *acc += 1;
            if let Ok(entries) = fs.list_dir(&inode) {
                for e in entries {
                    if e.name == "." || e.name == ".." {
                        continue;
                    }
                    rec(fs, e.ino, depth + 1, acc);
                }
            }
        } else {
            *acc += 1;
        }
    }
    let mut acc = 0u64;
    rec(fs, ino, 0, &mut acc);
    acc
}

fn join_path(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{}{}", dir, name)
    } else {
        format!("{}/{}", dir, name)
    }
}

fn parent_of(path: &str) -> String {
    let p = path.trim_end_matches('/');
    match p.rfind('/') {
        Some(0) => "/".into(),
        Some(i) => p[..i].into(),
        None => "/".into(),
    }
}

/// Best-effort recursive copy; individual failures are recorded in `errors`
/// and do not abort the whole extraction. Returns `false` if cancelled.
fn extract_node(
    fs: &Ext4,
    ino: u32,
    cur_path: &str,
    dest: &Path,
    depth: u32,
    errors: &mut Vec<String>,
    prog: Option<&CopyProgress>,
    cancel: &AtomicBool,
) -> bool {
    if cancel.load(Ordering::Relaxed) {
        return false;
    }
    if depth > 64 {
        errors.push(format!("max depth exceeded at {} (symlink loop?)", cur_path));
        return true;
    }
    if let Some(p) = prog {
        p.tick(cur_path);
    }
    let inode = match fs.read_inode(ino) {
        Ok(i) => i,
        Err(e) => {
            errors.push(format!("{}: {}", cur_path, e));
            return true;
        }
    };
    if inode.is_dir() {
        if let Err(e) = std::fs::create_dir_all(dest) {
            errors.push(format!("{}: {}", cur_path, e));
            return true;
        }
        let entries = match fs.list_dir(&inode) {
            Ok(e) => e,
            Err(e) => {
                errors.push(format!("{}: {}", cur_path, e));
                return true;
            }
        };
        for e in entries {
            if e.name == "." || e.name == ".." {
                continue;
            }
            let child = join_path(cur_path, &e.name);
            let (safe, changed) = sanitize_name(&e.name);
            if changed {
                errors.push(format!("{}: renamed to '{}' for Windows compatibility", child, safe));
            }
            if !extract_node(fs, e.ino, &child, &dest.join(&safe), depth + 1, errors, prog, cancel) {
                return false;
            }
        }
        true
    } else if inode.is_symlink() {
        let mut target = Vec::new();
        if let Err(e) = read_inode_data(fs, &inode, &mut target) {
            errors.push(format!("{}: {}", cur_path, e));
            return true;
        }
        let t = String::from_utf8_lossy(&target).into_owned();
        let resolved = if t.starts_with('/') {
            fs.resolve(&t)
        } else {
            let parent = parent_of(cur_path);
            fs.resolve(&join_path(&parent, t.trim_start_matches('/')))
        };
        match resolved {
            Ok(ino) => extract_node(fs, ino, cur_path, dest, depth + 1, errors, prog, cancel),
            Err(e) => {
                errors.push(format!("{}: broken symlink ({}): {}", cur_path, t, e));
                true
            }
        }
    } else {
        let mut data = Vec::new();
        if let Err(e) = read_inode_data(fs, &inode, &mut data) {
            errors.push(format!("{}: {}", cur_path, e));
            return true;
        }
        if let Some(p) = dest.parent() {
            if let Err(e) = std::fs::create_dir_all(p) {
                errors.push(format!("{}: {}", cur_path, e));
                return true;
            }
        }
        if let Err(e) = std::fs::write(dest, &data) {
            errors.push(format!("{}: {}", cur_path, e));
        }
        true
    }
}

/// On Windows, convert an absolute path to the `\\?\` extended-length form so
/// deep trees (paths longer than MAX_PATH) can be written.
#[cfg(windows)]
fn long_path(p: &Path) -> PathBuf {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    let wide: Vec<u16> = p.as_os_str().encode_wide().collect();
    const PREFIX: [u16; 4] = [b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16];
    if wide.starts_with(&PREFIX) || p.as_os_str().is_empty() {
        return p.to_path_buf();
    }
    // Only drive-letter absolute paths can be extended safely.
    if wide.len() >= 2 && wide[1] == b':' as u16 {
        let mut ext = PREFIX.to_vec();
        ext.extend_from_slice(&wide);
        PathBuf::from(std::ffi::OsString::from_wide(&ext))
    } else {
        p.to_path_buf()
    }
}

#[cfg(not(windows))]
fn long_path(p: &Path) -> PathBuf {
    p.to_path_buf()
}

/// Make a file name usable on Windows: strip invalid characters, trailing dots
/// / spaces and reserved device names. Returns (safe name, changed?).
fn sanitize_name(name: &str) -> (String, bool) {
    let mut out: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c => c,
        })
        .collect();
    while out.ends_with('.') || out.ends_with(' ') {
        out.pop();
    }
    if out.is_empty() {
        out = "_".into();
    }
    let base = out.split('.').next().unwrap_or("").to_uppercase();
    let reserved = matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$")
        || ((base.starts_with("COM") || base.starts_with("LPT"))
            && base[3..].parse::<u32>().map_or(false, |n| (1..=9).contains(&n)));
    if reserved {
        out.insert(0, '_');
    }
    let changed = out != name;
    (out, changed)
}
