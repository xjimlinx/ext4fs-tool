//! ext4 superblock parsing (primary copy at byte offset 1024).

use super::util::{cstr, u16, u32};
use std::io::{Read, Seek, SeekFrom};

pub const EXT4_SUPER_MAGIC: u16 = 0xEF53;
pub const SUPERBLOCK_OFFSET: u64 = 1024;

#[derive(Debug, Clone)]
pub struct Superblock {
    pub magic: u16,
    pub inodes_count: u64,
    pub blocks_count: u64,
    pub r_blocks_count: u64,
    pub free_blocks_count: u64,
    pub free_inodes_count: u32,
    pub first_data_block: u32,
    pub log_block_size: u32,
    pub blocks_per_group: u32,
    pub clusters_per_group: u32,
    pub inodes_per_group: u32,
    pub mtime: u32,
    pub wtime: u32,
    pub mount_count: u16,
    pub max_mount_count: u16,
    pub state: u16,
    pub errors: u16,
    pub rev_level: u32,
    pub inode_size: u16,
    pub feature_compat: u32,
    pub feature_incompat: u32,
    pub feature_ro_compat: u32,
    pub uuid: [u8; 16],
    pub volume_name: String,
    pub last_mounted: String,
    pub desc_size: u32,
    pub first_meta_bg: u32,
    pub min_extra_isize: u16,
    pub want_extra_isize: u16,
    pub flags: u32,
    pub default_hash_version: u8,
    pub checksum: u32,
}

impl Superblock {
    pub fn parse<R: Read + Seek>(r: &mut R, start: u64) -> std::io::Result<Superblock> {
        r.seek(SeekFrom::Start(start + SUPERBLOCK_OFFSET))?;
        let mut b = [0u8; 2048];
        r.read_exact(&mut b)?;

        let magic = u16(&b, 56);
        let sb = Superblock {
            magic,
            inodes_count: u32(&b, 0) as u64,
            blocks_count: u32(&b, 4) as u64 | (u32(&b, 336) as u64) << 32,
            r_blocks_count: u32(&b, 8) as u64 | (u32(&b, 340) as u64) << 32,
            free_blocks_count: u32(&b, 12) as u64 | (u32(&b, 344) as u64) << 32,
            free_inodes_count: u32(&b, 16),
            first_data_block: u32(&b, 20),
            log_block_size: u32(&b, 24),
            blocks_per_group: u32(&b, 32),
            clusters_per_group: u32(&b, 36),
            inodes_per_group: u32(&b, 40),
            mtime: u32(&b, 44),
            wtime: u32(&b, 48),
            mount_count: u16(&b, 52),
            max_mount_count: u16(&b, 54),
            state: u16(&b, 58),
            errors: u16(&b, 60),
            rev_level: u32(&b, 76),
            inode_size: u16(&b, 88),
            feature_compat: u32(&b, 92),
            feature_incompat: u32(&b, 96),
            feature_ro_compat: u32(&b, 100),
            uuid: b[104..120].try_into().unwrap(),
            volume_name: cstr(&b[120..136]),
            last_mounted: cstr(&b[136..200]),
            desc_size: u32::from(u16(&b, 254)),
            first_meta_bg: u32(&b, 260),
            min_extra_isize: u16(&b, 348),
            want_extra_isize: u16(&b, 350),
            flags: u32(&b, 352),
            default_hash_version: b[252],
            checksum: u32(&b, 1020),
        };
        if magic != EXT4_SUPER_MAGIC {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("bad superblock magic 0x{:04x} (not an ext2/3/4 filesystem?)", magic),
            ));
        }
        Ok(sb)
    }

    /// Block size in bytes (1024 << s_log_block_size).
    pub fn block_size(&self) -> u64 {
        1024u64 << self.log_block_size
    }

    pub fn has_incompat(&self, f: u32) -> bool {
        self.feature_incompat & f != 0
    }
    pub fn has_compat(&self, f: u32) -> bool {
        self.feature_compat & f != 0
    }
    pub fn has_ro_compat(&self, f: u32) -> bool {
        self.feature_ro_compat & f != 0
    }
}

// feature flags
pub const EXT4_FEATURE_COMPAT_HAS_JOURNAL: u32 = 0x0004;
pub const EXT4_FEATURE_COMPAT_DIR_INDEX: u32 = 0x0020;
pub const EXT4_FEATURE_INCOMPAT_FILETYPE: u32 = 0x0002;
pub const EXT4_FEATURE_INCOMPAT_EXTENTS: u32 = 0x0040;
pub const EXT4_FEATURE_INCOMPAT_64BIT: u32 = 0x0080;
pub const EXT4_FEATURE_INCOMPAT_FLEX_BG: u32 = 0x0200;
pub const EXT4_FEATURE_INCOMPAT_MMP: u32 = 0x0100;
pub const EXT4_FEATURE_INCOMPAT_INLINE_DATA: u32 = 0x8000;
pub const EXT4_FEATURE_RO_COMPAT_SPARSE_SUPER: u32 = 0x0001;
pub const EXT4_FEATURE_RO_COMPAT_LARGE_FILE: u32 = 0x0002;
pub const EXT4_FEATURE_RO_COMPAT_HUGE_FILE: u32 = 0x0008;
pub const EXT4_FEATURE_RO_COMPAT_GDT_CSUM: u32 = 0x0010;
