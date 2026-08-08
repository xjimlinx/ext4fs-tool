//! Directory reading: linear scan for classic (unindexed) directories and
//! hash-tree (htree) traversal for indexed directories.

use crate::ext4::error::ExtError;
use crate::ext4::inode::{EXT4_INDEX_FL, Inode};
use crate::ext4::superblock::EXT4_FEATURE_INCOMPAT_FILETYPE;
use crate::ext4::util::{u16, u32};
use crate::ext4::Ext4;
use crate::ext4::Result;

/// Directory entry file_type values (used when FILETYPE feature is enabled).
pub const FT_UNKNOWN: u8 = 0;
pub const FT_REG: u8 = 1;
pub const FT_DIR: u8 = 2;
pub const FT_CHRDEV: u8 = 3;
pub const FT_BLKDEV: u8 = 4;
pub const FT_FIFO: u8 = 5;
pub const FT_SOCK: u8 = 6;
pub const FT_SYMLINK: u8 = 7;

#[derive(Debug, Clone)]
pub struct DirEntry {
    pub ino: u32,
    pub rec_len: u16,
    pub name: String,
    pub file_type: u8,
}

fn entry_header_size(filetype_feature: bool) -> usize {
    if filetype_feature {
        8
    } else {
        8
    }
}

fn name_len_offset(filetype_feature: bool) -> usize {
    if filetype_feature {
        6
    } else {
        6
    }
}

impl Ext4 {
    fn filetype_feature(&self) -> bool {
        self.sb.has_incompat(EXT4_FEATURE_INCOMPAT_FILETYPE)
    }

    /// Collect the physical blocks that make up a directory's entries.
    fn dir_blocks(&self, inode: &Inode) -> Result<Vec<u64>> {
        let nblocks = (inode.size + self.block_size - 1) / self.block_size;
        let mut out = Vec::new();
        for lb in 0..nblocks {
            match self.block_to_paddr(inode, lb)? {
                Some(p) => out.push(p),
                None => {}
            }
        }
        Ok(out)
    }

    fn parse_entries<'a>(&self, data: &'a [u8]) -> Result<Vec<DirEntry>> {
        let mut out = Vec::new();
        let header = entry_header_size(self.filetype_feature());
        let name_off = name_len_offset(self.filetype_feature());
        let mut off = 0usize;
        while off + header <= data.len() {
            let rec_len = u16(data, off + 4) as usize;
            if rec_len == 0 {
                break;
            }
            if off + rec_len > data.len() {
                return Err(ExtError::Corrupt(format!(
                    "directory entry overruns block (off {}, rec_len {})",
                    off, rec_len
                )));
            }
            let ino = u32(data, off);
            if ino != 0 {
                let (nlen, ft) = if self.filetype_feature() {
                    (data[off + name_off] as usize, data[off + name_off + 1])
                } else {
                    (u16(data, off + 6) as usize, 0)
                };
                if off + header + nlen > data.len() {
                    return Err(ExtError::Corrupt(
                        "directory entry name overruns block".into(),
                    ));
                }
                let name = String::from_utf8_lossy(&data[off + header..off + header + nlen]).into_owned();
                out.push(DirEntry {
                    ino,
                    rec_len: rec_len as u16,
                    name,
                    file_type: ft,
                });
            }
            off += rec_len;
        }
        Ok(out)
    }

    /// List a directory's contents.
    pub fn list_dir(&self, inode: &Inode) -> Result<Vec<DirEntry>> {
        if !inode.is_dir() {
            return Err(ExtError::NotDir(inode.ino));
        }
        if inode.flags & EXT4_INDEX_FL != 0 {
            self.list_dir_indexed(inode)
        } else {
            let mut out = Vec::new();
            for p in self.dir_blocks(inode)? {
                let data = self.read_block(p)?;
                out.extend(self.parse_entries(&data)?);
            }
            Ok(out)
        }
    }

    /// Find a directory entry by name.
    pub fn lookup_dir(&self, inode: &Inode, name: &str) -> Result<Option<(u32, u8)>> {
        for e in self.list_dir(inode)? {
            if e.name == name {
                return Ok(Some((e.ino, e.file_type)));
            }
        }
        Ok(None)
    }

    // ------------------------------------------------------------------
    // Hash tree directories
    // ------------------------------------------------------------------

    fn list_dir_indexed(&self, inode: &Inode) -> Result<Vec<DirEntry>> {
        let mut out = Vec::new();

        // Root block: contains "." and ".." entries followed by dx_root_info
        // and the top-level dx_entry array. The "." entry sits at offset 0
        // and is 12 bytes; ".." at offset 12 is also 12 bytes (its rec_len
        // spans the rest of the block). dx_root_info follows at offset 24.
        let root_pb = self
            .block_to_paddr(inode, 0)?
            .ok_or_else(|| ExtError::Corrupt("indexed dir has no root block".into()))?;
        let data = self.read_block(root_pb)?;

        // Parse "." and ".." explicitly at their fixed offsets.
        let header = entry_header_size(self.filetype_feature());
        let name_off = name_len_offset(self.filetype_feature());
        for entry_off in [0usize, 12usize] {
            let ino = u32(&data, entry_off);
            if ino == 0 {
                continue;
            }
            let nlen = if self.filetype_feature() {
                data[entry_off + name_off] as usize
            } else {
                u16(&data, entry_off + 6) as usize
            };
            let ft = if self.filetype_feature() {
                data[entry_off + name_off + 1]
            } else {
                0
            };
            let name = String::from_utf8_lossy(&data[entry_off + header..entry_off + header + nlen]).into_owned();
            out.push(DirEntry {
                ino,
                rec_len: 0,
                name,
                file_type: ft,
            });
        }

        // dx_root_info fields:
        //   +0 reserved_zero (4), +4 hash_version (u8), +5 info_length (u8),
        //   +6 indirect_levels (u8), +7 unused_flags (u8)
        let info_start = 24usize;
        let info_len = data[info_start + 5] as usize; // info_length, usually 8
        let indirect_levels = data[info_start + 6];
        let entries_start = info_start + info_len;

        // The dx_entry array carries a {limit, count} header overlaid on the
        // hash field of the first entry (struct dx_countlimit). The count
        // includes that first entry, whose `block` field is still valid, so we
        // enumerate entries[0..count].
        let count = u16(&data, entries_start + 2) as usize;

        let mut leaves = Vec::new();
        for i in 0..count {
            let ent = entries_start + i * 8;
            // dx_entry.block is a *logical* block within the directory file;
            // map it to a physical block through the directory inode's extents.
            let logical = u32(&data, ent + 4) as u64;
            if indirect_levels == 0 {
                let pb = self
                    .block_to_paddr(inode, logical)?
                    .ok_or_else(|| ExtError::Corrupt("indexed dir leaf block is a hole".into()))?;
                leaves.push(pb);
            } else {
                self.collect_index_node(inode, logical, indirect_levels as u64, &mut leaves)?;
            }
        }

        for leaf in leaves {
            let lb = self.read_block(leaf)?;
            out.extend(self.parse_entries(&lb)?);
        }
        Ok(out)
    }

    /// Recursively collect leaf blocks below an index node. `logical` is the
    /// logical block of the index node within the directory file.
    fn collect_index_node(
        &self,
        inode: &Inode,
        logical: u64,
        level: u64,
        out: &mut Vec<u64>,
    ) -> Result<()> {
        let pb = self
            .block_to_paddr(inode, logical)?
            .ok_or_else(|| ExtError::Corrupt("indexed dir index block is a hole".into()))?;
        let data = self.read_block(pb)?;
        // Index nodes begin with an 8-byte fake dirent followed by dx entries.
        let entries_start = 8usize;
        let count = u16(&data, entries_start + 2) as usize;
        for i in 0..count {
            let ent = entries_start + i * 8;
            let child = u32(&data, ent + 4) as u64;
            if level == 1 {
                let cpb = self
                    .block_to_paddr(inode, child)?
                    .ok_or_else(|| ExtError::Corrupt("indexed dir leaf block is a hole".into()))?;
                out.push(cpb);
            } else {
                self.collect_index_node(inode, child, level - 1, out)?;
            }
        }
        Ok(())
    }
}
