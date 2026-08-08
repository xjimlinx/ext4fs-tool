//! Read-only ext4 filesystem reader.

#![allow(dead_code)]

pub mod copy;
pub mod dir;
pub mod device;
pub mod error;
pub mod group;
pub mod inode;
pub mod partitions;
pub mod superblock;
pub mod util;

use error::{ExtError, Result};
use group::GroupDesc;
use inode::Inode;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::sync::Mutex;
use superblock::Superblock;

use self::inode::{read_inode_data, EXT4_ROOT_INO};

pub struct Ext4 {
    file: Mutex<File>,
    pub start: u64,
    pub sb: Superblock,
    pub block_size: u64,
    pub groups: Vec<GroupDesc>,
    /// Size of each group descriptor on disk (32 or 64 bytes).
    pub desc_size: u64,
    pub is_64bit: bool,
}

impl Ext4 {
    /// Open a filesystem image. `start` is a byte offset in `path` where the
    /// filesystem partition begins (0 for a raw filesystem image).
    pub fn open(path: &str, start: u64) -> Result<Ext4> {
        let mut file = File::open(path)?;
        let sb = Superblock::parse(&mut file, start)?;
        let block_size = sb.block_size();
        if block_size < 1024 || block_size > 65536 || !block_size.is_power_of_two() {
            return Err(ExtError::Corrupt(format!("invalid block size {}", block_size)));
        }

        let is_64bit = sb.has_incompat(superblock::EXT4_FEATURE_INCOMPAT_64BIT);
        let desc_size = if is_64bit {
            if sb.desc_size < 64 {
                return Err(ExtError::Corrupt("64bit feature set but s_desc_size < 64".into()));
            }
            sb.desc_size as u64
        } else {
            32
        };

        let groups_count = sb.blocks_count.div_ceil(sb.blocks_per_group as u64);
        let groups = group::parse_all(&mut file, start, block_size, groups_count, desc_size, is_64bit)?;

        Ok(Ext4 {
            file: Mutex::new(file),
            start,
            sb,
            block_size,
            groups,
            desc_size,
            is_64bit,
        })
    }

    /// Read raw bytes at a filesystem-absolute offset.
    pub fn read_bytes_at(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let mut f = self.file.lock().unwrap();
        f.seek(SeekFrom::Start(self.start + offset))?;
        let mut buf = vec![0u8; len];
        f.read_exact(&mut buf)?;
        Ok(buf)
    }

    /// Read a whole physical block.
    pub fn read_block(&self, pblock: u64) -> Result<Vec<u8>> {
        let mut f = self.file.lock().unwrap();
        f.seek(SeekFrom::Start(self.start + pblock * self.block_size))?;
        let mut buf = vec![0u8; self.block_size as usize];
        f.read_exact(&mut buf)?;
        Ok(buf)
    }

    /// Read an inode's data as a stream of bytes (for files and symlinks).
    pub fn read_inode_data(&self, inode: &Inode, out: &mut Vec<u8>) -> Result<()> {
        crate::ext4::inode::read_inode_data(self, inode, out)
    }

    /// Resolve an absolute path to an inode number, following symlinks.
    pub fn resolve(&self, path: &str) -> Result<u32> {
        self.resolve_inner(path, 0)
    }

    fn resolve_inner(&self, path: &str, depth: u32) -> Result<u32> {
        if depth > 40 {
            return Err(ExtError::Unsupported("too many levels of symbolic links".into()));
        }
        let mut comps: Vec<String> = path
            .split('/')
            .filter(|s| !s.is_empty() && *s != ".")
            .map(String::from)
            .collect();

        let mut cur = EXT4_ROOT_INO;
        let mut i = 0usize;
        while i < comps.len() {
            let name = comps[i].clone();

            if name == ".." {
                let d = self.read_inode(cur)?;
                let (parent, _) = self
                    .lookup_dir(&d, "..")?
                    .ok_or_else(|| ExtError::NotFound("..".into()))?;
                cur = parent;
                i += 1;
                continue;
            }

            let d = self.read_inode(cur)?;
            if !d.is_dir() {
                return Err(ExtError::NotDir(cur));
            }
            let found = self.lookup_dir(&d, &name)?;
            let (ino, _ft) = match found {
                Some(f) => f,
                None => return Err(ExtError::NotFound(path.to_string())),
            };
            let inode = self.read_inode(ino)?;

            if inode.is_symlink() {
                let mut target = Vec::new();
                read_inode_data(self, &inode, &mut target)?;
                let t = String::from_utf8_lossy(&target).into_owned();
                let rest: Vec<String> = comps[i + 1..].to_vec();
                if t.starts_with('/') {
                    let mut full = t;
                    for r in &rest {
                        full.push('/');
                        full.push_str(r);
                    }
                    return self.resolve_inner(&full, depth + 1);
                } else {
                    let tcomps: Vec<String> = t
                        .split('/')
                        .filter(|s| !s.is_empty() && *s != ".")
                        .map(String::from)
                        .collect();
                    comps = [tcomps, rest].concat();
                    i = 0;
                    continue;
                }
            }

            if i + 1 == comps.len() {
                return Ok(ino);
            }
            if !inode.is_dir() {
                return Err(ExtError::NotDir(ino));
            }
            cur = ino;
            i += 1;
        }
        Ok(cur)
    }
}
