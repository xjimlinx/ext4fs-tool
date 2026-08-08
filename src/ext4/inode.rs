//! Inode parsing, file-type helpers and logical->physical block mapping
//! (extent trees and traditional indirect blocks).

use crate::ext4::error::ExtError;
use crate::ext4::util::{u16, u32};
use crate::ext4::Ext4;
use crate::ext4::Result;

pub const EXT4_ROOT_INO: u32 = 2;
pub const EXT4_EXTENTS_FL: u32 = 0x0008_0000;
pub const EXT4_INDEX_FL: u32 = 0x0000_1000;
pub const EXT4_INLINE_DATA_FL: u32 = 0x1000_0000;

// mode type bits
pub const S_IFMT: u16 = 0o170000;
pub const S_IFSOCK: u16 = 0o140000;
pub const S_IFLNK: u16 = 0o120000;
pub const S_IFREG: u16 = 0o100000;
pub const S_IFBLK: u16 = 0o060000;
pub const S_IFDIR: u16 = 0o040000;
pub const S_IFCHR: u16 = 0o020000;
pub const S_IFIFO: u16 = 0o010000;

/// On-disk size of the "good old" inode (pre-extra-isize).
pub const GOOD_OLD_INODE_SIZE: u16 = 128;

#[derive(Debug, Clone)]
pub struct Inode {
    pub ino: u32,
    pub mode: u16,
    pub uid: u32,
    pub size: u64,
    pub atime: u32,
    pub ctime: u32,
    pub mtime: u32,
    pub dtime: u32,
    pub gid: u32,
    pub links_count: u16,
    pub blocks: u64,
    pub flags: u32,
    pub block: [u32; 15],
    /// Raw i_block bytes (used for fast symlinks and inline data).
    pub block_raw: [u8; 60],
    pub generation: u32,
    pub file_acl: u64,
    pub extra_isize: u16,
    pub checksum: u32,
}

impl Inode {
    pub fn parse(raw: &[u8], ino: u32) -> Inode {
        let size_lo = u32(raw, 4);
        let size_high = u32(raw, 108);
        let uid = u16(raw, 2) as u32 | (u16(raw, 120) as u32) << 16;
        let gid = u16(raw, 24) as u32 | (u16(raw, 122) as u32) << 16;
        let blocks_lo = u32(raw, 28) as u64;
        let blocks_high = u16(raw, 116) as u64;
        let file_acl_lo = u32(raw, 104) as u64;
        let file_acl_high = u16(raw, 118) as u64;
        let checksum_lo = u16(raw, 124);
        let checksum_hi = if raw.len() >= 132 { u16(raw, 130) } else { 0 };
        let mut block = [0u32; 15];
        for (i, b) in block.iter_mut().enumerate() {
            *b = u32(raw, 40 + i * 4);
        }
        let block_raw = raw[40..100].try_into().unwrap();
        Inode {
            ino,
            mode: u16(raw, 0),
            uid,
            size: (size_high as u64) << 32 | size_lo as u64,
            atime: u32(raw, 8),
            ctime: u32(raw, 12),
            mtime: u32(raw, 16),
            dtime: u32(raw, 20),
            gid,
            links_count: u16(raw, 26),
            blocks: blocks_high << 32 | blocks_lo,
            flags: u32(raw, 32),
            block,
            block_raw,
            generation: u32(raw, 100),
            file_acl: file_acl_high << 32 | file_acl_lo,
            extra_isize: if raw.len() >= 130 { u16(raw, 128) } else { 0 },
            checksum: (checksum_hi as u32) << 16 | checksum_lo as u32,
        }
    }

    pub fn file_type(&self) -> u16 {
        self.mode & S_IFMT
    }
    pub fn is_dir(&self) -> bool {
        self.file_type() == S_IFDIR
    }
    pub fn is_file(&self) -> bool {
        self.file_type() == S_IFREG
    }
    pub fn is_symlink(&self) -> bool {
        self.file_type() == S_IFLNK
    }
    pub fn is_fast_symlink(&self) -> bool {
        self.is_symlink() && self.size <= 60 && self.blocks == 0 && self.flags & EXT4_INLINE_DATA_FL == 0
    }
}

impl Ext4 {
    /// Read the raw on-disk bytes of an inode.
    pub fn read_inode_raw(&self, ino: u32) -> Result<Vec<u8>> {
        let per_group = self.sb.inodes_per_group as u64;
        if ino == 0 {
            return Err(ExtError::Corrupt("inode number 0".into()));
        }
        let n = ino as u64 - 1;
        let group = (n / per_group) as usize;
        let idx = n % per_group;
        let gd = self
            .groups
            .get(group)
            .ok_or_else(|| ExtError::Corrupt(format!("inode {} outside group table", ino)))?;
        let inode_size = self.sb.inode_size as u64;
        let byte = idx * inode_size;
        let pblock = gd.inode_table + byte / self.block_size;
        let off = (byte % self.block_size) as usize;
        let blk = self.read_block(pblock)?;
        let end = off + inode_size as usize;
        if end > blk.len() {
            return Err(ExtError::Corrupt("inode crosses block boundary".into()));
        }
        Ok(blk[off..end].to_vec())
    }

    /// Read an inode by number (1-based, like the on-disk numbering).
    pub fn read_inode(&self, ino: u32) -> Result<Inode> {
        let raw = self.read_inode_raw(ino)?;
        Ok(Inode::parse(&raw, ino))
    }

    /// Read inline file data stored inside the inode itself.
    pub fn inline_data(&self, inode: &Inode) -> Result<Vec<u8>> {
        let raw = self.read_inode_raw(inode.ino)?;
        let pos = (GOOD_OLD_INODE_SIZE + inode.extra_isize) as usize;
        let size = inode.size as usize;
        if pos + size > raw.len() {
            return Err(ExtError::Corrupt(format!(
                "inline data of inode {} overruns inode (pos {}, size {})",
                inode.ino, pos, size
            )));
        }
        Ok(raw[pos..pos + size].to_vec())
    }

    /// Map a logical file block to a physical block number.
    /// `Ok(None)` means the block is a hole (or unwritten) and reads as zeros.
    pub fn block_to_paddr(&self, inode: &Inode, logical: u64) -> Result<Option<u64>> {
        if inode.flags & EXT4_EXTENTS_FL != 0 {
            self.extent_lookup(inode, logical)
        } else {
            self.indirect_lookup(inode, logical)
        }
    }
}

/// Stream an inode's data into `out` (handles inline data, fast symlinks,
/// sparse holes and normal block maps).
pub fn read_inode_data(fs: &Ext4, inode: &Inode, out: &mut Vec<u8>) -> Result<()> {
    if inode.flags & EXT4_INLINE_DATA_FL != 0 {
        out.extend_from_slice(&fs.inline_data(inode)?);
        return Ok(());
    }
    if inode.is_fast_symlink() {
        out.extend_from_slice(&inode.block_raw[..inode.size as usize]);
        return Ok(());
    }
    let size = inode.size;
    let mut remaining = size;
    let mut logical = 0u64;
    while remaining > 0 {
        let chunk = remaining.min(fs.block_size) as usize;
        match fs.block_to_paddr(inode, logical)? {
            Some(p) => {
                let blk = fs.read_block(p)?;
                out.extend_from_slice(&blk[..chunk]);
            }
            None => out.resize(out.len() + chunk, 0),
        }
        remaining -= chunk as u64;
        logical += 1;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Extent trees
// ---------------------------------------------------------------------------

const EXTENT_MAGIC: u16 = 0xF30A;

#[derive(Debug, Clone, Copy)]
struct ExtentHeader {
    entries: u16,
    depth: u16,
}

impl Ext4 {
    fn extent_lookup(&self, inode: &Inode, logical: u64) -> Result<Option<u64>> {
        let hdr = self.parse_extent_header(&inode.block_raw)?;
        self.extent_walk(inode, &hdr, logical, &mut 0)
    }

    fn parse_extent_header(&self, buf: &[u8]) -> Result<ExtentHeader> {
        if u16(buf, 0) != EXTENT_MAGIC {
            return Err(ExtError::Corrupt("bad extent header magic".into()));
        }
        Ok(ExtentHeader {
            entries: u16(buf, 2),
            depth: u16(buf, 6),
        })
    }

    fn extent_walk(
        &self,
        inode: &Inode,
        hdr: &ExtentHeader,
        logical: u64,
        guard: &mut u32,
    ) -> Result<Option<u64>> {
        *guard += 1;
        if *guard > 16 {
            return Err(ExtError::Corrupt("extent tree too deep".into()));
        }
        if hdr.depth == 0 {
            for i in 0..hdr.entries {
                let e = 12 + i as usize * 12;
                let ee_block = u32(&inode.block_raw, e) as u64;
                let len_raw = u16(&inode.block_raw, e + 4);
                let len = (len_raw & 0x7fff) as u64;
                let unwritten = len_raw & 0x8000 != 0;
                let start = (u16(&inode.block_raw, e + 6) as u64) << 32 | u32(&inode.block_raw, e + 8) as u64;
                if logical >= ee_block && logical < ee_block + len {
                    if unwritten {
                        return Ok(None);
                    }
                    return Ok(Some(start + (logical - ee_block)));
                }
            }
            Ok(None)
        } else {
            let mut chosen = None;
            for i in 0..hdr.entries {
                let e = 12 + i as usize * 12;
                let idx_block = u32(&inode.block_raw, e) as u64;
                if idx_block <= logical {
                    chosen = Some(e);
                } else {
                    break;
                }
            }
            let e = chosen.ok_or_else(|| ExtError::Corrupt("no extent index covers block".into()))?;
            let leaf = (u16(&inode.block_raw, e + 8) as u64) << 32 | u32(&inode.block_raw, e + 4) as u64;
            let buf = self.read_block(leaf)?;
            let hdr = self.parse_extent_header(&buf)?;
            self.extent_walk_indexed(hdr, leaf, &buf, logical, guard)
        }
    }

    /// Walk an extent tree stored in a data block (index or leaf block).
    fn extent_walk_indexed(
        &self,
        hdr: ExtentHeader,
        _leaf: u64,
        buf: &[u8],
        logical: u64,
        guard: &mut u32,
    ) -> Result<Option<u64>> {
        *guard += 1;
        if *guard > 16 {
            return Err(ExtError::Corrupt("extent tree too deep".into()));
        }
        if hdr.depth == 0 {
            for i in 0..hdr.entries {
                let e = 12 + i as usize * 12;
                let ee_block = u32(buf, e) as u64;
                let len_raw = u16(buf, e + 4);
                let len = (len_raw & 0x7fff) as u64;
                let unwritten = len_raw & 0x8000 != 0;
                let start = (u16(buf, e + 6) as u64) << 32 | u32(buf, e + 8) as u64;
                if logical >= ee_block && logical < ee_block + len {
                    if unwritten {
                        return Ok(None);
                    }
                    return Ok(Some(start + (logical - ee_block)));
                }
            }
            Ok(None)
        } else {
            let mut chosen = None;
            for i in 0..hdr.entries {
                let e = 12 + i as usize * 12;
                let idx_block = u32(buf, e) as u64;
                if idx_block <= logical {
                    chosen = Some(e);
                } else {
                    break;
                }
            }
            let e = chosen.ok_or_else(|| ExtError::Corrupt("no extent index covers block".into()))?;
            let next = (u16(buf, e + 8) as u64) << 32 | u32(buf, e + 4) as u64;
            let nb = self.read_block(next)?;
            let hdr = self.parse_extent_header(&nb)?;
            self.extent_walk_indexed(hdr, next, &nb, logical, guard)
        }
    }
}

// ---------------------------------------------------------------------------
// Indirect blocks (ext2/3 style)
// ---------------------------------------------------------------------------

impl Ext4 {
    fn indirect_lookup(&self, inode: &Inode, logical: u64) -> Result<Option<u64>> {
        let ptrs = self.block_size / 4;
        if logical < 12 {
            let p = inode.block[logical as usize];
            return Ok(if p == 0 { None } else { Some(p as u64) });
        }
        let mut l = logical - 12;

        // single indirect
        if l < ptrs {
            return self.indirect_ptr(inode.block[12] as u64, l as usize);
        }
        l -= ptrs;

        // double indirect
        let p2 = ptrs * ptrs;
        if l < p2 {
            let blk = inode.block[13] as u64;
            let idx1 = (l / ptrs) as usize;
            let idx2 = (l % ptrs) as usize;
            let mid = self.indirect_ptr(blk, idx1)?.unwrap_or(0);
            if mid == 0 {
                return Ok(None);
            }
            return self.indirect_ptr(mid, idx2);
        }
        l -= p2;

        // triple indirect
        let p3 = ptrs * ptrs * ptrs;
        if l < p3 {
            let blk = inode.block[14] as u64;
            let idx1 = (l / (ptrs * ptrs)) as usize;
            let idx2 = ((l / ptrs) % ptrs) as usize;
            let idx3 = (l % ptrs) as usize;
            let mid1 = self.indirect_ptr(blk, idx1)?.unwrap_or(0);
            if mid1 == 0 {
                return Ok(None);
            }
            let mid2 = self.indirect_ptr(mid1, idx2)?.unwrap_or(0);
            if mid2 == 0 {
                return Ok(None);
            }
            return self.indirect_ptr(mid2, idx3);
        }
        Err(ExtError::Unsupported("file block number too large for indirect mapping".into()))
    }

    fn indirect_ptr(&self, block: u64, idx: usize) -> Result<Option<u64>> {
        if block == 0 {
            return Ok(None);
        }
        let buf = self.read_block(block)?;
        let o = idx * 4;
        if o + 4 > buf.len() {
            return Err(ExtError::Corrupt("indirect block index out of range".into()));
        }
        let p = u32(&buf, o);
        Ok(if p == 0 { None } else { Some(p as u64) })
    }
}
