//! Copy files and directories out of an ext filesystem (read-only, best effort),
//! with optional progress reporting and cancellation.

use crate::ext4::error::Result;
use crate::ext4::inode::{read_inode_data, read_inode_data_chunks, read_inode_data_chunks_handle};
use crate::ext4::Ext4;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Shared progress state, updated from the worker thread and read by the UI.
pub struct CopyProgress {
    total: AtomicU64,
    total_bytes: AtomicU64,
    done: AtomicU64,
    bytes: AtomicU64,
    started: Instant,
    current: Mutex<String>,
}

impl CopyProgress {
    pub fn new(total: u64) -> Self {
        Self {
            total: AtomicU64::new(total),
            total_bytes: AtomicU64::new(0),
            done: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            started: Instant::now(),
            current: Mutex::new(String::new()),
        }
    }
    /// Set the real totals once known (items and bytes).
    pub fn set_totals(&self, items: u64, bytes: u64) {
        self.total.store(items, Ordering::Relaxed);
        self.total_bytes.store(bytes, Ordering::Relaxed);
    }
    pub fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes.load(Ordering::Relaxed)
    }
    pub fn done(&self) -> u64 {
        self.done.load(Ordering::Relaxed)
    }
    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }
    /// Smooth progress: byte-based when the byte total is known, otherwise
    /// item-based. Clamped to [0, 1].
    pub fn fraction(&self) -> f32 {
        let tb = self.total_bytes.load(Ordering::Relaxed);
        if tb > 0 {
            (self.bytes.load(Ordering::Relaxed) as f32 / tb as f32).min(1.0)
        } else {
            let t = self.total.load(Ordering::Relaxed);
            if t == 0 {
                0.0
            } else {
                (self.done.load(Ordering::Relaxed) as f32 / t as f32).min(1.0)
            }
        }
    }
    pub fn current(&self) -> String {
        self.current.lock().map(|s| s.clone()).unwrap_or_default()
    }
    /// Items processed per second.
    pub fn rate_items(&self) -> f64 {
        let elapsed = self.started.elapsed().as_secs_f64().max(0.001);
        self.done.load(Ordering::Relaxed) as f64 / elapsed
    }
    /// Seconds elapsed since the job started.
    pub fn elapsed_secs(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }
    /// Estimated seconds until completion.
    pub fn eta_seconds(&self) -> Option<u64> {
        let tb = self.total_bytes.load(Ordering::Relaxed);
        let (remaining, rate) = if tb > 0 {
            let done_b = self.bytes.load(Ordering::Relaxed);
            let elapsed = self.started.elapsed().as_secs_f64().max(0.001);
            (tb.saturating_sub(done_b) as f64, done_b as f64 / elapsed)
        } else {
            let done = self.done.load(Ordering::Relaxed);
            if done == 0 || self.total.load(Ordering::Relaxed) == 0 {
                return None;
            }
            let elapsed = self.started.elapsed().as_secs_f64().max(0.001);
            ((self.total.load(Ordering::Relaxed) - done) as f64, done as f64 / elapsed)
        };
        if rate <= 0.0 {
            return None;
        }
        Some((remaining / rate) as u64)
    }
    fn tick(&self, path: &str) {
        *self.current.lock().unwrap() = path.to_string();
        self.done.fetch_add(1, Ordering::Relaxed);
    }
    fn add_bytes(&self, n: u64) {
        self.bytes.fetch_add(n, Ordering::Relaxed);
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

/// Like [`extract_with_progress`] but writes to the exact `dest` path, which
/// also lets the caller rename the copied item.
pub fn copy_to_with_progress(
    fs: &Ext4,
    src_path: &str,
    dest: &Path,
    prog: &CopyProgress,
    cancel: &AtomicBool,
) -> Result<(PathBuf, Vec<String>)> {
    let ino = fs.resolve(src_path)?;
    let base = long_path(dest);
    let mut errors = Vec::new();
    extract_node(fs, ino, src_path, &base, 0, &mut errors, Some(prog), cancel);
    Ok((dest.to_path_buf(), errors))
}

/// Copy `src_path` to the exact `dest` path (allows renaming), synchronously.
pub fn copy_to(fs: &Ext4, src_path: &str, dest: &Path) -> Result<(PathBuf, Vec<String>)> {
    let ino = fs.resolve(src_path)?;
    let base = long_path(dest);
    let mut errors = Vec::new();
    let cancel = AtomicBool::new(false);
    extract_node(fs, ino, src_path, &base, 0, &mut errors, None, &cancel);
    Ok((dest.to_path_buf(), errors))
}

/// Count items and total bytes reachable below `ino`. Best effort.
pub fn count_tree_with_bytes(fs: &Ext4, ino: u32) -> (u64, u64) {
    fn rec(fs: &Ext4, ino: u32, depth: u32, items: &mut u64, bytes: &mut u64) {
        if depth > 64 {
            return;
        }
        let inode = match fs.read_inode(ino) {
            Ok(i) => i,
            Err(_) => {
                *items += 1;
                return;
            }
        };
        if inode.is_dir() {
            *items += 1;
            if let Ok(entries) = fs.list_dir(&inode) {
                for e in entries {
                    if e.name == "." || e.name == ".." {
                        continue;
                    }
                    rec(fs, e.ino, depth + 1, items, bytes);
                }
            }
        } else {
            *items += 1;
            *bytes += inode.size;
        }
    }
    let mut items = 0u64;
    let mut bytes = 0u64;
    rec(fs, ino, 0, &mut items, &mut bytes);
    (items, bytes)
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
        if let Some(p) = dest.parent() {
            if let Err(e) = std::fs::create_dir_all(p) {
                errors.push(format!("{}: {}", cur_path, e));
                return true;
            }
        }
        let file = match std::fs::File::create(dest) {
            Ok(f) => f,
            Err(e) => {
                errors.push(format!("{}: {}", cur_path, e));
                return true;
            }
        };
        let mut writer = std::io::BufWriter::new(file);
        if let Some(p) = prog {
            p.add_bytes(inode.size);
        }
        let res = read_inode_data_chunks(fs, &inode, |chunk| writer.write_all(chunk));
        if let Err(e) = res {
            errors.push(format!("{}: {}", cur_path, e));
        } else if let Err(e) = writer.flush() {
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

// ---------------------------------------------------------------------------
// Parallel copy: enumerate the tree into jobs, then copy with N worker threads.
// ---------------------------------------------------------------------------

#[derive(Clone)]
enum Job {
    Dir(PathBuf),
    File(u32, PathBuf),
}

/// Copy `src_path` to the exact `dest` path (allows renaming), in parallel.
/// `fs` must be shared (`Arc`) so worker threads can open their own handles.
pub fn copy_to_parallel(
    fs: &Arc<Ext4>,
    src_path: &str,
    dest: &Path,
    prog: &Arc<CopyProgress>,
    cancel: &Arc<AtomicBool>,
    workers: usize,
) -> Result<(PathBuf, Vec<String>)> {
    let ino = fs.resolve(src_path)?;
    let base = long_path(dest);
    let mut errors = Vec::new();
    let mut jobs = Vec::new();
    collect_plan(fs, ino, src_path, &base, 0, &mut jobs, &mut errors, cancel);
    copy_plan(fs, &jobs, prog, cancel, &mut errors, workers);
    Ok((dest.to_path_buf(), errors))
}

fn collect_plan(
    fs: &Ext4,
    ino: u32,
    cur_path: &str,
    dest: &Path,
    depth: u32,
    jobs: &mut Vec<Job>,
    errors: &mut Vec<String>,
    cancel: &AtomicBool,
) -> bool {
    if cancel.load(Ordering::Relaxed) {
        return false;
    }
    if depth > 64 {
        errors.push(format!("max depth exceeded at {} (symlink loop?)", cur_path));
        return true;
    }
    let inode = match fs.read_inode(ino) {
        Ok(i) => i,
        Err(e) => {
            errors.push(format!("{}: {}", cur_path, e));
            return true;
        }
    };
    if inode.is_dir() {
        jobs.push(Job::Dir(dest.to_path_buf()));
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
            if !collect_plan(fs, e.ino, &child, &dest.join(&safe), depth + 1, jobs, errors, cancel) {
                return false;
            }
        }
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
            Ok(rino) => match fs.read_inode(rino) {
                Ok(r) if r.is_dir() => {
                    collect_plan(fs, rino, cur_path, dest, depth + 1, jobs, errors, cancel);
                }
                _ => jobs.push(Job::File(rino, dest.to_path_buf())),
            },
            Err(e) => errors.push(format!("{}: broken symlink ({}): {}", cur_path, t, e)),
        }
    } else {
        jobs.push(Job::File(ino, dest.to_path_buf()));
    }
    true
}

fn copy_plan(
    fs: &Arc<Ext4>,
    jobs: &[Job],
    prog: &Arc<CopyProgress>,
    cancel: &Arc<AtomicBool>,
    errors: &mut Vec<String>,
    workers: usize,
) {
    let n = jobs.len();
    if n == 0 {
        return;
    }
    let workers = workers.clamp(1, 64);
    // Small copies: sequential avoids thread-spawn overhead.
    if n < 8 || workers <= 1 {
        let mut handle = match fs.open_independent_handle() {
            Ok(f) => f,
            Err(_) => {
                errors.push("cannot open source handle".into());
                return;
            }
        };
        for job in jobs {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            process_job(fs, &mut handle, job, prog, cancel, errors);
        }
        return;
    }

    let next = Arc::new(AtomicUsize::new(0));
    let jobs_arc = Arc::new(jobs.to_vec());
    let errors_arc = Arc::new(Mutex::new(Vec::new()));
    let mut handles = Vec::new();
    for _ in 0..workers {
        let fs2 = fs.clone();
        let jobs2 = jobs_arc.clone();
        let next2 = next.clone();
        let errors2 = errors_arc.clone();
        let prog2 = prog.clone();
        let cancel2 = cancel.clone();
        handles.push(std::thread::spawn(move || {
            let mut handle = match fs2.open_independent_handle() {
                Ok(f) => f,
                Err(_) => return,
            };
            loop {
                if cancel2.load(Ordering::Relaxed) {
                    break;
                }
                let i = next2.fetch_add(1, Ordering::Relaxed);
                if i >= jobs2.len() {
                    break;
                }
                let mut local = Vec::new();
                process_job(&fs2, &mut handle, &jobs2[i], &prog2, &cancel2, &mut local);
                errors2.lock().unwrap().extend(local);
            }
        }));
    }
    for h in handles {
        let _ = h.join();
    }
    errors.extend(errors_arc.lock().unwrap().drain(..));
}

fn process_job(
    fs: &Ext4,
    handle: &mut std::fs::File,
    job: &Job,
    prog: &CopyProgress,
    cancel: &AtomicBool,
    errors: &mut Vec<String>,
) {
    if cancel.load(Ordering::Relaxed) {
        return;
    }
    match job {
        Job::Dir(dest) => {
            if let Err(e) = std::fs::create_dir_all(dest) {
                errors.push(format!("{}: {}", dest.display(), e));
            }
            prog.tick(&dest.display().to_string());
        }
        Job::File(ino, dest) => {
            let inode = match fs.read_inode(*ino) {
                Ok(i) => i,
                Err(e) => {
                    errors.push(format!("{}: {}", dest.display(), e));
                    return;
                }
            };
            if let Some(p) = dest.parent() {
                if let Err(e) = std::fs::create_dir_all(p) {
                    errors.push(format!("{}: {}", dest.display(), e));
                    return;
                }
            }
            let out = match std::fs::File::create(dest) {
                Ok(f) => f,
                Err(e) => {
                    errors.push(format!("{}: {}", dest.display(), e));
                    return;
                }
            };
            let mut writer = std::io::BufWriter::new(out);
            prog.add_bytes(inode.size);
            let res = read_inode_data_chunks_handle(fs, &inode, handle, |chunk| writer.write_all(chunk));
            if let Err(e) = res {
                errors.push(format!("{}: {}", dest.display(), e));
            } else if let Err(e) = writer.flush() {
                errors.push(format!("{}: {}", dest.display(), e));
            }
            prog.tick(&dest.display().to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ext4::Ext4;

    #[test]
    fn parallel_extract_big_dir() {
        let fs = Arc::new(Ext4::open("test/ext4.img", 0).unwrap());
        let prog = Arc::new(CopyProgress::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let dest = std::env::temp_dir().join("ext4fs_par_test");
        let _ = std::fs::remove_dir_all(&dest);
        let (out, errors) = copy_to_parallel(&fs, "/big", &dest, &prog, &cancel, 4).unwrap();
        let n = std::fs::read_dir(&out).unwrap().count();
        assert_eq!(n, 200, "errors: {:?}", errors);
        let _ = std::fs::remove_dir_all(&dest);
    }
}
