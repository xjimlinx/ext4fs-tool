//! Test-image generator: constructs a valid ext4 image and a valid ext2 image
//! from scratch (a tiny mkfs), used to exercise the read-only reader.
//!
//! Run with:  cargo run --example mkimg

use std::fs;

const BLK4K: usize = 4096;

fn w16(d: &mut [u8], o: usize, v: u16) {
    d[o..o + 2].copy_from_slice(&v.to_le_bytes());
}
fn w32(d: &mut [u8], o: usize, v: u32) {
    d[o..o + 4].copy_from_slice(&v.to_le_bytes());
}
fn w64(d: &mut [u8], o: usize, v: u64) {
    d[o..o + 8].copy_from_slice(&v.to_le_bytes());
}

// feature flags
const COMPAT_DIR_INDEX: u32 = 0x0020;
const INCOMPAT_FILETYPE: u32 = 0x0002;
const INCOMPAT_EXTENTS: u32 = 0x0040;
const INCOMPAT_64BIT: u32 = 0x0080;
const INCOMPAT_FLEX_BG: u32 = 0x0200;
const INCOMPAT_INLINE_DATA: u32 = 0x8000;
const ROCOMPAT_SPARSE_SUPER: u32 = 0x0001;
const ROCOMPAT_LARGE_FILE: u32 = 0x0002;
const ROCOMPAT_EXTRA_ISIZE: u32 = 0x0040;

// inode flags
const FL_EXTENTS: u32 = 0x0008_0000;
const FL_INDEX: u32 = 0x0000_1000;
const FL_INLINE: u32 = 0x1000_0000;

const MODE_REG: u16 = 0o100644;
const MODE_DIR: u16 = 0o040755;
const MODE_LNK: u16 = 0o120777;

struct Mkfs {
    data: Vec<u8>,
    bs: usize,
    bpg: usize,
    ipg: usize,
    groups: usize,
    inode_size: usize,
    desc_size: usize,
    itb: usize,
    block_bitmap: Vec<usize>,
    inode_bitmap: Vec<usize>,
    inode_table: Vec<usize>,
    used: Vec<bool>,
    next_inode: u32,
    next_block: usize,
    dir_count: Vec<u32>,
    filetype: bool,
}

impl Mkfs {
    fn new(bs: usize, total_blocks: usize, bpg: usize, ipg: usize, inode_size: usize, desc_size: usize, filetype: bool) -> Mkfs {
        let groups = total_blocks.div_ceil(bpg);
        Mkfs {
            data: vec![0u8; total_blocks * bs],
            bs,
            bpg,
            ipg,
            groups,
            inode_size,
            desc_size,
            itb: (ipg * inode_size).div_ceil(bs),
            block_bitmap: vec![0; groups],
            inode_bitmap: vec![0; groups],
            inode_table: vec![0; groups],
            used: vec![false; total_blocks],
            next_inode: 1, // first alloc_inode returns 2
            next_block: 0,
            dir_count: vec![0; groups],
            filetype,
        }
    }

    fn mark(&mut self, b: usize) {
        self.used[b] = true;
    }

    fn alloc_block(&mut self) -> usize {
        while self.next_block < self.used.len() && self.used[self.next_block] {
            self.next_block += 1;
        }
        assert!(self.next_block < self.used.len(), "out of blocks");
        self.used[self.next_block] = true;
        let b = self.next_block;
        self.next_block += 1;
        b
    }

    fn alloc_inode(&mut self, mode: u16, flags: u32) -> u32 {
        let ino = loop {
            let i = self.next_inode;
            self.next_inode += 1;
            if i != 1 {
                break i; // inode 1 is reserved ("bad blocks")
            }
        };
        let g = ((ino - 1) / self.ipg as u32) as usize;
        let idx = (ino - 1) % self.ipg as u32;
        let off = self.inode_table[g] * self.bs + idx as usize * self.inode_size;
        let raw = &mut self.data[off..off + self.inode_size];
        w16(raw, 0, mode);
        w16(raw, 26, 1); // links
        w32(raw, 32, flags);
        if self.inode_size > 128 {
            w16(raw, 128, 32); // i_extra_isize
        }
        ino
    }

    fn set_inode_size(&mut self, ino: u32, size: u64) {
        let (off, _) = self.inode_loc(ino);
        let raw = &mut self.data[off..off + self.inode_size];
        w32(raw, 4, size as u32);
        w32(raw, 108, (size >> 32) as u32);
    }

    fn set_inode_blocks(&mut self, ino: u32, blocks512: u64) {
        let (off, _) = self.inode_loc(ino);
        let raw = &mut self.data[off..off + self.inode_size];
        w32(raw, 28, blocks512 as u32);
        w16(raw, 116, (blocks512 >> 32) as u16);
    }

    fn inode_loc(&self, ino: u32) -> (usize, usize) {
        let g = ((ino - 1) / self.ipg as u32) as usize;
        let idx = (ino - 1) % self.ipg as u32;
        (
            self.inode_table[g] * self.bs + idx as usize * self.inode_size,
            g,
        )
    }

    fn inode_block_area(&mut self, ino: u32) -> &mut [u8] {
        let (off, _) = self.inode_loc(ino);
        &mut self.data[off + 40..off + 100]
    }

    /// Write a single-level extent tree into the inode's i_block area.
    fn set_extents(&mut self, ino: u32, extents: &[(u32, u16, u64)]) {
        let area = self.inode_block_area(ino);
        w16(area, 0, 0xF30A);
        w16(area, 2, extents.len() as u16);
        w16(area, 4, 4);
        w16(area, 6, 0);
        for (i, (l, len, phys)) in extents.iter().enumerate() {
            let e = 12 + i * 12;
            w32(area, e, *l);
            w16(area, e + 4, *len);
            w16(area, e + 6, (*phys >> 32) as u16);
            w32(area, e + 8, *phys as u32);
        }
    }

    /// Write a depth-1 extent tree: root index in inode pointing to a leaf block.
    fn set_extents_indexed(&mut self, ino: u32, leaf_block: u64, extents: &[(u32, u16, u64)]) {
        let area = self.inode_block_area(ino);
        w16(area, 0, 0xF30A);
        w16(area, 2, 1);
        w16(area, 4, 4);
        w16(area, 6, 1);
        let ie = 12;
        w32(area, ie, 0);
        w32(area, ie + 4, leaf_block as u32);
        w16(area, ie + 8, (leaf_block >> 32) as u16);

        let leaf = &mut self.data[(leaf_block as usize) * self.bs..(leaf_block as usize + 1) * self.bs];
        w16(leaf, 0, 0xF30A);
        w16(leaf, 2, extents.len() as u16);
        w16(leaf, 4, ((self.bs - 12) / 12) as u16);
        w16(leaf, 6, 0);
        for (i, (l, len, phys)) in extents.iter().enumerate() {
            let e = 12 + i * 12;
            w32(leaf, e, *l);
            w16(leaf, e + 4, *len);
            w16(leaf, e + 6, (*phys >> 32) as u16);
            w32(leaf, e + 8, *phys as u32);
        }
    }

    fn write_direct_blocks(&mut self, ino: u32, blocks: &[usize]) {
        let area = self.inode_block_area(ino);
        for (i, b) in blocks.iter().enumerate() {
            w32(area, i * 4, *b as u32);
        }
    }

    fn write_indirect_block(&mut self, block: usize, ptrs: &[u32]) {
        let b = &mut self.data[block * self.bs..(block + 1) * self.bs];
        for (i, p) in ptrs.iter().enumerate() {
            w32(b, i * 4, *p);
        }
    }

    fn write_dir_block(&mut self, block: usize, entries: &[(u32, &str, u8)], final_block: bool) {
        let n = entries.len();
        let mut off = 0usize;
        for (i, (ino, name, ft)) in entries.iter().enumerate() {
            let name_len = name.len();
            let align = if self.filetype { 8 } else { 4 };
            let mut sz = 8 + name_len;
            sz = (sz + align - 1) & !(align - 1);
            let rec_len = if final_block && i == n - 1 {
                self.bs - off
            } else {
                sz
            };
            let d = &mut self.data[block * self.bs..(block + 1) * self.bs];
            w32(d, off, *ino);
            w16(d, off + 4, rec_len as u16);
            if self.filetype {
                d[off + 6] = name_len as u8;
                d[off + 7] = *ft;
            } else {
                w16(d, off + 6, name_len as u16);
            }
            d[off + 8..off + 8 + name_len].copy_from_slice(name.as_bytes());
            off += sz;
        }
    }

    fn write_block_bitmaps(&mut self) {
        for g in 0..self.groups {
            let bb = self.block_bitmap[g];
            let start = g * self.bpg;
            let end = (start + self.bpg).min(self.used.len());
            let b = &mut self.data[bb * self.bs..(bb + 1) * self.bs];
            for (abs, used) in self.used.iter().enumerate() {
                if abs >= start && abs < end && *used {
                    let bit = abs - start;
                    b[bit / 8] |= 1 << (bit % 8);
                }
            }
        }
    }

    fn write_inode_bitmaps(&mut self) {
        for g in 0..self.groups {
            let ib = self.inode_bitmap[g];
            let first = g * self.ipg + 1;
            let b = &mut self.data[ib * self.bs..(ib + 1) * self.bs];
            for ino in first..(first + self.ipg) {
                if (ino as u32) < self.next_inode {
                    let bit = (ino - 1) % self.ipg;
                    b[bit / 8] |= 1 << (bit % 8);
                }
            }
        }
    }
}

fn main() {
    let dir = "test";
    fs::create_dir_all(dir).ok();
    build_ext4(&format!("{}/ext4.img", dir));
    build_ext2(&format!("{}/ext2.img", dir));
    build_mbr(&format!("{}/mbr.img", dir));
    build_gpt(&format!("{}/gpt.img", dir));
    println!("wrote test images");
}

/// Wrap the ext4 image in an MBR partition table (partition type 0x83).
fn build_mbr(path: &str) {
    let ext = fs::read("test/ext4.img").unwrap();
    let sector = 512u64;
    let start_lba = 2048u64;
    let sectors = (ext.len() as u64).div_ceil(sector);
    let total = ((start_lba + sectors + 64) * sector) as usize;
    let mut data = vec![0u8; total];
    data[446 + 4] = 0x83; // Linux
    w32(&mut data, 446 + 8, start_lba as u32);
    w32(&mut data, 446 + 12, sectors as u32);
    data[510] = 0x55;
    data[511] = 0xAA;
    let off = (start_lba * sector) as usize;
    data[off..off + ext.len()].copy_from_slice(&ext);
    fs::write(path, &data).unwrap();
}

/// Wrap the ext4 image in a GPT partition table (Linux filesystem GUID).
fn build_gpt(path: &str) {
    let ext = fs::read("test/ext4.img").unwrap();
    let sector = 512u64;
    let start_lba = 2048u64;
    let sectors = (ext.len() as u64).div_ceil(sector);
    let last_lba = start_lba + sectors - 1;
    let total_lba = last_lba + 33;
    let mut data = vec![0u8; (total_lba * sector) as usize];

    // protective MBR
    data[446 + 4] = 0xEE;
    w32(&mut data, 446 + 8, 1);
    w32(&mut data, 446 + 12, total_lba as u32);
    data[510] = 0x55;
    data[511] = 0xAA;

    // GPT header at LBA 1
    let h = (1 * sector) as usize;
    data[h..h + 8].copy_from_slice(b"EFI PART");
    w32(&mut data, h + 8, 0x0001_0000); // revision
    w32(&mut data, h + 12, 92); // header size
    w64(&mut data, h + 24, 1); // current lba
    w64(&mut data, h + 32, total_lba - 1); // backup lba
    w64(&mut data, h + 40, 34); // first usable
    w64(&mut data, h + 48, last_lba); // last usable
    w64(&mut data, h + 72, 2); // entries lba
    w32(&mut data, h + 80, 128); // num entries
    w32(&mut data, h + 84, 128); // entry size

    // one partition entry at LBA 2
    let e = (2 * sector) as usize;
    // Linux filesystem type GUID 0FC63DAF-8483-4772-8E79-3D69D8477DE4
    let guid: [u8; 16] = [0xAF, 0x3D, 0xC6, 0x0F, 0x83, 0x84, 0x72, 0x47, 0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D, 0xE4];
    data[e..e + 16].copy_from_slice(&guid);
    w64(&mut data, e + 32, start_lba);
    w64(&mut data, e + 40, last_lba);
    let name: Vec<u8> = "ext4 test"
        .encode_utf16()
        .flat_map(|u| u.to_le_bytes())
        .collect();
    data[e + 56..e + 56 + name.len()].copy_from_slice(&name);

    let off = (start_lba * sector) as usize;
    data[off..off + ext.len()].copy_from_slice(&ext);
    fs::write(path, &data).unwrap();
}

fn build_ext4(path: &str) {
    let bs = BLK4K;
    let bpg = 1024;
    let ipg = 64;
    let total_blocks = 4096; // 16 MB
    let inode_size = 256;
    let mut fs = Mkfs::new(bs, total_blocks, bpg, ipg, inode_size, 64, true);

    // ---- layout metadata -------------------------------------------------
    let gdt_blocks = (fs.groups * fs.desc_size).div_ceil(bs);
    for g in 0..fs.groups {
        if g == 0 {
            fs.mark(0); // superblock block
            for b in 1..1 + gdt_blocks {
                fs.mark(b); // GDT
            }
            let mut c = 1 + gdt_blocks;
            fs.block_bitmap[0] = c;
            fs.mark(c);
            c += 1;
            fs.inode_bitmap[0] = c;
            fs.mark(c);
            c += 1;
            fs.inode_table[0] = c;
            for b in c..c + fs.itb {
                fs.mark(b);
            }
        } else {
            let base = g * bpg;
            fs.block_bitmap[g] = base;
            fs.mark(base);
            fs.inode_bitmap[g] = base + 1;
            fs.mark(base + 1);
            fs.inode_table[g] = base + 2;
            for b in base + 2..base + 2 + fs.itb {
                fs.mark(b);
            }
        }
    }

    // ---- superblock --------------------------------------------------------
    let sb = &mut fs.data[1024..2048];
    w32(sb, 0, (fs.groups * fs.ipg) as u32); // inodes_count
    w32(sb, 4, total_blocks as u32); // blocks_count_lo
    w32(sb, 8, 0); // r_blocks_count_lo
    w32(sb, 12, 0); // free_blocks_count_lo
    w32(sb, 16, 0); // free_inodes_count
    w32(sb, 20, 0); // first_data_block
    w32(sb, 24, 2); // log_block_size -> 4096
    w32(sb, 28, 2); // log_cluster_size
    w32(sb, 32, bpg as u32);
    w32(sb, 36, bpg as u32);
    w32(sb, 40, ipg as u32);
    w16(sb, 56, 0xEF53); // magic
    w16(sb, 58, 1); // state: clean
    w16(sb, 60, 1); // errors: continue
    w32(sb, 76, 1); // rev_level: dynamic
    w32(sb, 84, 11); // first_ino
    w16(sb, 88, inode_size as u16);
    w32(sb, 92, COMPAT_DIR_INDEX);
    w32(
        sb,
        96,
        INCOMPAT_FILETYPE | INCOMPAT_EXTENTS | INCOMPAT_64BIT | INCOMPAT_FLEX_BG | INCOMPAT_INLINE_DATA,
    );
    w32(sb, 100, ROCOMPAT_SPARSE_SUPER | ROCOMPAT_LARGE_FILE | ROCOMPAT_EXTRA_ISIZE);
    sb[104..120].copy_from_slice(&[
        0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88,
    ]);
    sb[120..128].copy_from_slice(b"testvol\0");
    sb[252] = 1; // default_hash_version
    w16(sb, 254, 64); // desc_size
    w16(sb, 348, 28); // min_extra_isize
    w16(sb, 350, 32); // want_extra_isize
    w32(sb, 352, 0); // flags

    // ---- group descriptors ------------------------------------------------
    for g in 0..fs.groups {
        let off = fs.gdt_block_bytes(1, g);
        let d = &mut fs.data[off..off + 64];
        w32(d, 0, fs.block_bitmap[g] as u32);
        w32(d, 4, fs.inode_bitmap[g] as u32);
        w32(d, 8, fs.inode_table[g] as u32);
        w16(d, 16, 1); // used dirs (will be refined)
    }

    // ---- filesystem content ------------------------------------------------
    let root = fs.alloc_inode(MODE_DIR, FL_EXTENTS);
    assert_eq!(root, 2);
    fs.dir_count[0] = 1;

    // root directory contents (non-indexed)
    let root_block = fs.alloc_block();
    fs.set_extents(root, &[(0, 1, root_block as u64)]);
    let hello = fs.alloc_inode(MODE_REG, FL_EXTENTS);
    let subdir = fs.alloc_inode(MODE_DIR, FL_EXTENTS);
    let sparse = fs.alloc_inode(MODE_REG, FL_EXTENTS);
    let frag = fs.alloc_inode(MODE_REG, FL_EXTENTS);
    let longlink = fs.alloc_inode(MODE_LNK, FL_EXTENTS);
    let fastlink = fs.alloc_inode(MODE_LNK, 0);
    let inline_f = fs.alloc_inode(MODE_REG, FL_INLINE);
    let big = fs.alloc_inode(MODE_DIR, FL_EXTENTS | FL_INDEX);
    let s2 = fs.alloc_inode(MODE_REG, FL_EXTENTS); // file in subdir
    let link2 = fs.alloc_inode(MODE_LNK, 0); // symlink in subdir
    let zh_file = fs.alloc_inode(MODE_REG, FL_EXTENTS); // 中文文件
    let zh_dir = fs.alloc_inode(MODE_DIR, FL_EXTENTS); // 中文目录
    let zh_inner = fs.alloc_inode(MODE_REG, FL_EXTENTS); // 中文目录里的文件

    fs.write_dir_block(
        root_block,
        &[
            (root, ".", 2),
            (root, "..", 2),
            (hello, "hello.txt", 1),
            (subdir, "subdir", 2),
            (sparse, "sparse.bin", 1),
            (frag, "frag.bin", 1),
            (longlink, "longlink", 7),
            (fastlink, "fastlink", 7),
            (inline_f, "inline.txt", 1),
            (big, "big", 2),
            (zh_file, "中文文件.txt", 1),
            (zh_dir, "中文目录", 2),
        ],
        true,
    );
    fs.set_inode_size(root, bs as u64);
    fs.set_inode_blocks(root, 8); // 1 block in 512 units

    // 中文文件.txt
    let zhb = fs.alloc_block();
    let zh_text = "中文内容，UTF-8 测试！\n";
    fs.data[zhb * bs..zhb * bs + zh_text.len()].copy_from_slice(zh_text.as_bytes());
    fs.set_extents(zh_file, &[(0, 1, zhb as u64)]);
    fs.set_inode_size(zh_file, zh_text.len() as u64);
    fs.set_inode_blocks(zh_file, 8);

    // 中文目录/你好.txt
    let zdb = fs.alloc_block();
    fs.set_extents(zh_dir, &[(0, 1, zdb as u64)]);
    let zin = fs.alloc_block();
    let zh_inner_text = "你好，ext4！\n";
    fs.data[zin * bs..zin * bs + zh_inner_text.len()].copy_from_slice(zh_inner_text.as_bytes());
    fs.set_extents(zh_inner, &[(0, 1, zin as u64)]);
    fs.set_inode_size(zh_inner, zh_inner_text.len() as u64);
    fs.set_inode_blocks(zh_inner, 8);
    fs.write_dir_block(
        zdb,
        &[(zh_dir, ".", 2), (root, "..", 2), (zh_inner, "你好.txt", 1)],
        true,
    );
    fs.set_inode_size(zh_dir, bs as u64);
    fs.set_inode_blocks(zh_dir, 8);
    fs.dir_count[0] += 1;

    // hello.txt : 3 blocks of repeated text
    let h1 = fs.alloc_block();
    let h2 = fs.alloc_block();
    let h3 = fs.alloc_block();
    let text = b"Hello from ext4fs-tool!\n";
    let mut pat = Vec::new();
    while pat.len() < 2 * bs {
        pat.extend_from_slice(text);
    }
    fs.data[h1 * bs..(h1 + 1) * bs].copy_from_slice(&pat[..bs]);
    fs.data[h2 * bs..(h2 + 1) * bs].copy_from_slice(&pat[..bs]);
    let tail = &pat[..100];
    fs.data[h3 * bs..h3 * bs + 100].copy_from_slice(tail);
    fs.set_extents(hello, &[(0, 2, h1 as u64), (2, 1, h3 as u64)]);
    fs.set_inode_size(hello, (2 * bs + 100) as u64);
    fs.set_inode_blocks(hello, (3 * 8) as u64);

    // sparse.bin : block 0 written, blocks 1..2 holes, 100 tail bytes
    let s0 = fs.alloc_block();
    fs.data[s0 * bs..(s0 + 1) * bs].copy_from_slice(&pat[..bs]);
    fs.set_extents(sparse, &[(0, 1, s0 as u64)]);
    fs.set_inode_size(sparse, (3 * bs + 100) as u64);
    fs.set_inode_blocks(sparse, (1 * 8) as u64);

    // frag.bin : depth-1 extent tree, 5 single-block extents with holes
    let leaf = fs.alloc_block();
    let mut extents = Vec::new();
    for k in 0..5u32 {
        let b = fs.alloc_block();
        fs.data[b * bs..(b + 1) * bs].copy_from_slice(&pat[..bs]);
        extents.push((k * 10, 1, b as u64));
    }
    fs.set_extents_indexed(frag, leaf as u64, &extents);
    fs.set_inode_size(frag, (40 * bs + 100) as u64);
    fs.set_inode_blocks(frag, (5 * 8) as u64);

    // longlink : target > 60 bytes, stored in a data block
    let long_target = "/this/is/a/rather/long/symlink/target/that/needs/its/own/data/block/for/reading.txt";
    let lb = fs.alloc_block();
    fs.data[lb * bs..lb * bs + long_target.len()].copy_from_slice(long_target.as_bytes());
    fs.set_extents(longlink, &[(0, 1, lb as u64)]);
    fs.set_inode_size(longlink, long_target.len() as u64);
    fs.set_inode_blocks(longlink, 8);

    // fastlink : short target stored in i_block
    let ft = "/hello.txt";
    fs.inode_block_area(fastlink)[..ft.len()].copy_from_slice(ft.as_bytes());
    fs.set_inode_size(fastlink, ft.len() as u64);

    // inline.txt : data inside the inode
    {
        let content = b"inline data test!\n";
        {
            let (off, _) = fs.inode_loc(inline_f);
            let raw = &mut fs.data[off..off + fs.inode_size];
            let pos = 128 + 32; // GOOD_OLD_INODE_SIZE + i_extra_isize
            raw[pos..pos + content.len()].copy_from_slice(content);
            w32(raw, 40, 0); // i_block[0] extra inline size (informational)
        }
        fs.set_inode_size(inline_f, content.len() as u64);
    }

    // subdir
    let sub_block = fs.alloc_block();
    fs.set_extents(subdir, &[(0, 1, sub_block as u64)]);
    fs.write_dir_block(
        sub_block,
        &[(subdir, ".", 2), (root, "..", 2), (s2, "inner.txt", 1), (link2, "link", 7)],
        true,
    );
    fs.set_inode_size(subdir, bs as u64);
    fs.set_inode_blocks(subdir, 8);
    fs.dir_count[0] += 1;

    // inner.txt
    let c1 = fs.alloc_block();
    fs.data[c1 * bs..(c1 + 1) * bs].copy_from_slice(&pat[..bs]);
    fs.set_extents(s2, &[(0, 1, c1 as u64)]);
    fs.set_inode_size(s2, bs as u64);
    fs.set_inode_blocks(s2, 8);

    // subdir/link -> ../hello.txt (fast symlink)
    let l2 = "../hello.txt";
    fs.inode_block_area(link2)[..l2.len()].copy_from_slice(l2.as_bytes());
    fs.set_inode_size(link2, l2.len() as u64);

    // big : indexed directory with 200 entries
    {
        let mut inos = Vec::new();
        for _ in 0..200 {
            inos.push(fs.alloc_inode(MODE_REG, FL_EXTENTS));
        }
        let root_b = fs.alloc_block();
        let _filler = fs.alloc_block();
        let leaf_b = fs.alloc_block();
        // Non-contiguous so logical block != physical block (leaf at logical 1).
        fs.set_extents(big, &[(0, 1, root_b as u64), (1, 1, leaf_b as u64)]);
        fs.set_inode_size(big, (2 * bs) as u64);
        fs.set_inode_blocks(big, 16);

        // leaf entries
        let mut entries = Vec::new();
        for (i, ino) in inos.iter().enumerate() {
            let name: &'static str = Box::leak(format!("f{:03}", i).into_boxed_str());
            entries.push((*ino, name, 1));
        }
        fs.write_dir_block(leaf_b, &entries, true);

        // root block: dot, dotdot, dx_root_info, dx entries
        let d = &mut fs.data[root_b * bs..(root_b + 1) * bs];
        // "." at 0 (12 bytes)
        w32(d, 0, big);
        w16(d, 4, 12);
        d[6] = 1;
        d[7] = 2;
        d[8] = b'.';
        // ".." at 12 (12 bytes)
        w32(d, 12, root);
        w16(d, 16, 12);
        d[18] = 2;
        d[19] = 2;
        d[20..22].copy_from_slice(b"..");
        // dx_root_info at 24
        w32(d, 24, 0);
        d[28] = 1; // hash_version
        d[29] = 8; // info_length
        d[30] = 0; // indirect_levels
        d[31] = 0;
        // entries at 32: entries[0] pseudo (hash = limit<<16|count), block = LOGICAL block 1
        w16(d, 32, 2); // limit
        w16(d, 34, 1); // count (includes entries[0])
        w32(d, 36, 1); // logical block of the leaf
        fs.dir_count[0] += 1;
    }

    // free counts (unused but tidy)
    let free_blocks = total_blocks as u32 - fs.used.iter().filter(|&&u| u).count() as u32;
    w32(&mut fs.data[1024..2048], 12, free_blocks);

    fs.write_block_bitmaps();
    fs.write_inode_bitmaps();

    fs::write(path, &fs.data).unwrap();
}

fn build_ext2(path: &str) {
    let bs = 1024;
    let bpg = 1024;
    let ipg = 64;
    let total_blocks = 2048; // 2 MB
    let inode_size = 128;
    let mut fs = Mkfs::new(bs, total_blocks, bpg, ipg, inode_size, 32, false);

    let gdt_blocks = (fs.groups * fs.desc_size).div_ceil(bs);
    for g in 0..fs.groups {
        if g == 0 {
            fs.mark(0); // boot block
            fs.mark(1); // superblock block (offset 1024)
            for b in 2..2 + gdt_blocks {
                fs.mark(b); // GDT
            }
            let mut c = 2 + gdt_blocks;
            fs.block_bitmap[0] = c;
            fs.mark(c);
            c += 1;
            fs.inode_bitmap[0] = c;
            fs.mark(c);
            c += 1;
            fs.inode_table[0] = c;
            for b in c..c + fs.itb {
                fs.mark(b);
            }
        } else {
            let base = g * bpg;
            fs.block_bitmap[g] = base;
            fs.mark(base);
            fs.inode_bitmap[g] = base + 1;
            fs.mark(base + 1);
            fs.inode_table[g] = base + 2;
            for b in base + 2..base + 2 + fs.itb {
                fs.mark(b);
            }
        }
    }

    let sb = &mut fs.data[1024..2048];
    w32(sb, 0, (fs.groups * fs.ipg) as u32);
    w32(sb, 4, total_blocks as u32);
    w32(sb, 20, 1); // first_data_block = 1
    w32(sb, 24, 0); // log_block_size = 1024
    w32(sb, 28, 0);
    w32(sb, 32, bpg as u32);
    w32(sb, 36, bpg as u32);
    w32(sb, 40, ipg as u32);
    w16(sb, 56, 0xEF53);
    w16(sb, 58, 1);
    w16(sb, 60, 1);
    w32(sb, 76, 1); // dynamic rev
    w32(sb, 84, 11);
    w16(sb, 88, inode_size as u16);
    w32(sb, 92, 0); // compat
    w32(sb, 96, 0); // incompat
    w32(sb, 100, 0); // ro_compat
    sb[120..128].copy_from_slice(b"ext2vol\0");

    for g in 0..fs.groups {
        let off = fs.gdt_block_bytes(2, g);
        let d = &mut fs.data[off..off + 32];
        w32(d, 0, fs.block_bitmap[g] as u32);
        w32(d, 4, fs.inode_bitmap[g] as u32);
        w32(d, 8, fs.inode_table[g] as u32);
        w16(d, 16, 1);
    }

    // root
    let root = fs.alloc_inode(MODE_DIR, 0);
    assert_eq!(root, 2);
    fs.dir_count[0] = 1;

    let root_block = fs.alloc_block();
    fs.write_direct_blocks(root, &[root_block]);
    fs.set_inode_size(root, bs as u64);
    fs.set_inode_blocks(root, 2);

    // big.txt : 300 blocks -> exercises direct, single and double indirect
    let n = 300usize;
    let mut data_blocks: Vec<usize> = Vec::new();
    for _ in 0..n {
        data_blocks.push(fs.alloc_block());
    }
    // fill each data block with pattern based on logical block index
    for (logical, b) in data_blocks.iter().enumerate() {
        let blk = &mut fs.data[*b * bs..(*b + 1) * bs];
        let mark = format!("blk{:03}", logical);
        for i in 0..bs {
            blk[i] = mark.as_bytes()[i % mark.len()];
        }
    }

    let big = fs.alloc_inode(MODE_REG, 0);
    // direct[0..12]
    let mut direct = Vec::new();
    for i in 0..12 {
        direct.push(data_blocks[i]);
    }
    // single indirect block for logical 12..(12+256)
    let single = fs.alloc_block();
    let mut single_ptrs = vec![0u32; bs / 4];
    for i in 0..256 {
        single_ptrs[i] = data_blocks[12 + i] as u32;
    }
    fs.write_indirect_block(single, &single_ptrs);
    // double indirect: logical 268..300
    let double = fs.alloc_block();
    let mid = fs.alloc_block();
    let mut mid_ptrs = vec![0u32; bs / 4];
    for i in 0..(n - 268) {
        mid_ptrs[i] = data_blocks[268 + i] as u32;
    }
    fs.write_indirect_block(mid, &mid_ptrs);
    let mut double_ptrs = vec![0u32; bs / 4];
    double_ptrs[0] = mid as u32;
    fs.write_indirect_block(double, &double_ptrs);

    let area = fs.inode_block_area(big);
    for (i, b) in direct.iter().enumerate() {
        w32(area, i * 4, *b as u32);
    }
    w32(area, 12 * 4, single as u32);
    w32(area, 13 * 4, double as u32);
    fs.set_inode_size(big, (n * bs) as u64);
    fs.set_inode_blocks(big, ((n + 3) * 2) as u64);

    // root dir entries
    fs.write_dir_block(root_block, &[(root, ".", 0), (root, "..", 0), (big, "big.txt", 0)], true);

    let free_blocks = total_blocks as u32 - fs.used.iter().filter(|&&u| u).count() as u32;
    w32(&mut fs.data[1024..2048], 12, free_blocks);

    fs.write_block_bitmaps();
    fs.write_inode_bitmaps();

    fs::write(path, &fs.data).unwrap();
}

impl Mkfs {
    fn gdt_block_bytes(&self, gdt_block: usize, g: usize) -> usize {
        (gdt_block * self.bs) + g * self.desc_size
    }
}
