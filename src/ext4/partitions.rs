//! MBR / GPT partition table parsing and filesystem-type detection.

use crate::ext4::error::{ExtError, Result};
use crate::ext4::util::{u16, u32, u64 as rd_u64};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

#[derive(Debug, Clone, PartialEq)]
pub enum PartKind {
    Mbr(u8),
    Gpt([u8; 16]),
}

#[derive(Debug, Clone)]
pub struct Partition {
    pub index: u32,
    pub start_lba: u64,
    pub sectors: u64,
    pub kind: PartKind,
    pub name: String,
}

impl Partition {
    pub fn start_bytes(&self, sector_size: u64) -> u64 {
        self.start_lba * sector_size
    }
    pub fn size_bytes(&self, sector_size: u64) -> u64 {
        self.sectors * sector_size
    }
}

pub struct PartitionTable {
    pub is_gpt: bool,
    pub partitions: Vec<Partition>,
}

/// Parse the partition table at the start of a disk or disk image.
/// Returns an error if there is no valid MBR/GPT signature.
pub fn read_partition_table(file: &mut File, sector_size: u64) -> Result<PartitionTable> {
    file.seek(SeekFrom::Start(0))?;
    let mut mbr = [0u8; 512];
    let n = file.read(&mut mbr)?;
    // Signature bytes are 0x55 0xAA at offsets 510/511, i.e. 0xAA55 read LE.
    if n < 512 || u16(&mbr, 510) != 0xAA55 {
        return Err(ExtError::Unsupported(
            "no MBR/GPT signature found (raw filesystem without a partition table?)".into(),
        ));
    }

    let mut protective = false;
    let mut parts = Vec::new();
    for i in 0..4u32 {
        let o = 446 + i as usize * 16;
        let pt = mbr[o + 4];
        if pt == 0 {
            continue;
        }
        if pt == 0xEE {
            protective = true;
        }
        parts.push(Partition {
            index: i + 1,
            start_lba: u32(&mbr, o + 8) as u64,
            sectors: u32(&mbr, o + 12) as u64,
            kind: PartKind::Mbr(pt),
            name: String::new(),
        });
    }

    if protective {
        return read_gpt(file, sector_size);
    }
    Ok(PartitionTable {
        is_gpt: false,
        partitions: parts,
    })
}

fn read_gpt(file: &mut File, sector_size: u64) -> Result<PartitionTable> {
    file.seek(SeekFrom::Start(sector_size))?;
    let mut hdr = [0u8; 512];
    let n = file.read(&mut hdr)?;
    if n < 92 || &hdr[0..8] != b"EFI PART".as_slice() {
        return Err(ExtError::Unsupported("invalid GPT header".into()));
    }
    let entries_lba = rd_u64(&hdr, 72);
    let num_entries = u32(&hdr, 80);
    let entry_size = u32(&hdr, 84) as usize;
    if entry_size < 128 || num_entries == 0 {
        return Err(ExtError::Unsupported("unexpected GPT entry size/count".into()));
    }
    let mut entries = vec![0u8; num_entries as usize * entry_size];
    file.seek(SeekFrom::Start(entries_lba * sector_size))?;
    file.read_exact(&mut entries)?;

    let mut parts = Vec::new();
    for i in 0..num_entries as usize {
        let o = i * entry_size;
        if entries[o..o + 16].iter().all(|&b| b == 0) {
            continue;
        }
        let first = rd_u64(&entries, o + 32);
        let last = rd_u64(&entries, o + 40);
        parts.push(Partition {
            index: (i + 1) as u32,
            start_lba: first,
            sectors: if last >= first { last - first + 1 } else { 0 },
            kind: PartKind::Gpt(entries[o..o + 16].try_into().unwrap()),
            name: decode_utf16(&entries[o + 56..o + 128]),
        });
    }
    Ok(PartitionTable {
        is_gpt: true,
        partitions: parts,
    })
}

/// Best-effort filesystem type detection at `offset` bytes into the device.
pub fn detect_fs(file: &mut File, offset: u64) -> String {
    let mut buf = [0u8; 2048];
    let n = match file.seek(SeekFrom::Start(offset)).and_then(|_| file.read(&mut buf)) {
        Ok(n) => n,
        Err(_) => return "unknown".into(),
    };
    let has = |from: usize, pat: &[u8]| n >= from + pat.len() && buf[from..from + pat.len()] == *pat;
    // ext superblock: magic 0xEF53 at sb offset 56 (1024 + 56 within the partition)
    if n > 1082 && u16(&buf, 1024 + 56) == 0xEF53 {
        return "ext2/3/4".into();
    }
    if has(3, b"NTFS    ") {
        return "NTFS".into();
    }
    if has(3, b"EXFAT") {
        return "exFAT".into();
    }
    if has(82, b"FAT32   ") {
        return "FAT32".into();
    }
    if has(54, b"FAT16   ") || has(54, b"FAT12   ") {
        return "FAT".into();
    }
    "unknown".into()
}

/// Read the volume label of the filesystem starting at `offset`.
pub fn detect_fs_label(file: &mut File, offset: u64) -> String {
    let mut buf = vec![0u8; 2048];
    let n = match file.seek(SeekFrom::Start(offset)).and_then(|_| file.read(&mut buf)) {
        Ok(n) => n,
        Err(_) => return String::new(),
    };
    let label_ascii = |from: usize, len: usize| {
        let end = buf[from..(from + len).min(buf.len())]
            .iter()
            .position(|&c| c == 0)
            .map(|i| from + i)
            .unwrap_or(from + len);
        String::from_utf8_lossy(&buf[from..end]).trim().to_string()
    };
    // NTFS / exFAT store the label as UTF-16LE.
    let label_utf16 = |from: usize, max_chars: usize| {
        let mut units: Vec<u16> = Vec::new();
        let mut i = from;
        while i + 1 < buf.len() && units.len() < max_chars {
            let u = u16::from_le_bytes([buf[i], buf[i + 1]]);
            if u == 0 {
                break;
            }
            units.push(u);
            i += 2;
        }
        String::from_utf16_lossy(&units).trim().to_string()
    };
    if n > 1082 && u16(&buf, 1024 + 56) == 0xEF53 {
        return label_ascii(1024 + 120, 16); // ext4 s_volume_name
    }
    let has = |from: usize, pat: &[u8]| n >= from + pat.len() && buf[from..from + pat.len()] == *pat;
    if has(3, b"NTFS    ") {
        return label_utf16(72, 11); // NTFS volume label
    }
    if has(3, b"EXFAT") {
        return label_utf16(107, 11); // exFAT volume label
    }
    if has(82, b"FAT32   ") || has(54, b"FAT16   ") || has(54, b"FAT12   ") {
        return label_ascii(43, 11); // FAT volume label
    }
    String::new()
}

fn decode_utf16(b: &[u8]) -> String {
    let units: Vec<u16> = b
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&u| u != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn with_boot_sector(tag: &str, patch: &dyn Fn(&mut [u8])) -> String {
        let mut bs = vec![0u8; 2048];
        patch(&mut bs);
        let path = std::env::temp_dir().join(format!("ext4fs_{}_test.bin", tag));
        std::fs::File::create(&path).unwrap().write_all(&bs).unwrap();
        let mut f = std::fs::File::open(&path).unwrap();
        let s = detect_fs_label(&mut f, 0);
        let _ = std::fs::remove_file(&path);
        s
    }

    #[test]
    fn ntfs_label_is_utf16() {
        let label = with_boot_sector("ntfs", &|bs| {
            bs[3..11].copy_from_slice(b"NTFS    ");
            let bytes: Vec<u8> = "Windows 数据".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
            bs[72..72 + bytes.len()].copy_from_slice(&bytes);
        });
        assert_eq!(label, "Windows 数据");
    }

    #[test]
    fn fat_label_is_ascii() {
        let label = with_boot_sector("fat", &|bs| {
            bs[82..90].copy_from_slice(b"FAT32   ");
            bs[43..49].copy_from_slice(b"MYDATA");
        });
        assert_eq!(label, "MYDATA");
    }
}