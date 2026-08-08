//! Block group descriptor table parsing.

use super::util::u16;
use super::util::u32;
use std::io::{Read, Seek, SeekFrom};

#[derive(Debug, Clone)]
pub struct GroupDesc {
    pub block_bitmap: u64,
    pub inode_bitmap: u64,
    pub inode_table: u64,
    pub free_blocks_count: u32,
    pub free_inodes_count: u32,
    pub used_dirs_count: u32,
    pub flags: u16,
    pub checksum: u16,
}

/// First block containing the group descriptor table.
pub fn first_gdt_block(block_size: u64) -> u64 {
    if block_size == 1024 {
        2
    } else {
        1
    }
}

pub fn parse_all<R: Read + Seek>(
    r: &mut R,
    start: u64,
    block_size: u64,
    groups_count: u64,
    desc_size: u64,
    is_64bit: bool,
) -> std::io::Result<Vec<GroupDesc>> {
    let gdt_block = first_gdt_block(block_size);
    let bytes_needed = groups_count * desc_size;
    let blocks_needed = bytes_needed.div_ceil(block_size);
    let total = (blocks_needed * block_size) as usize;

    let mut data = vec![0u8; total];
    r.seek(SeekFrom::Start(start + gdt_block * block_size))?;
    r.read_exact(&mut data)?;

    let mut out = Vec::with_capacity(groups_count as usize);
    for g in 0..groups_count {
        let o = (g * desc_size) as usize;
        let (bmap, imap, itable, fbc, fic, udc, fl, cs) = if is_64bit {
            let bmap = u32(&data, o) as u64 | (u32(&data, o + 32) as u64) << 32;
            let imap = u32(&data, o + 4) as u64 | (u32(&data, o + 36) as u64) << 32;
            let itable = u32(&data, o + 8) as u64 | (u32(&data, o + 40) as u64) << 32;
            let fbc = u32(&data, o + 12) as u64 | (u16(&data, o + 44) as u64) << 16;
            let fic = u16(&data, o + 14) as u32 | (u16(&data, o + 46) as u32) << 16;
            let udc = u16(&data, o + 16) as u32 | (u16(&data, o + 48) as u32) << 16;
            let fl = u16(&data, o + 18);
            let cs = u16(&data, o + 30);
            (bmap, imap, itable, fbc as u32, fic, udc, fl, cs)
        } else {
            (
                u32(&data, o) as u64,
                u32(&data, o + 4) as u64,
                u32(&data, o + 8) as u64,
                u32(&data, o + 12),
                u16(&data, o + 14) as u32,
                u16(&data, o + 16) as u32,
                u16(&data, o + 18),
                u16(&data, o + 30),
            )
        };
        out.push(GroupDesc {
            block_bitmap: bmap,
            inode_bitmap: imap,
            inode_table: itable,
            free_blocks_count: fbc,
            free_inodes_count: fic,
            used_dirs_count: udc,
            flags: fl,
            checksum: cs,
        });
    }
    Ok(out)
}
