//! ext4fs-tool �?a read-only ext2/ext3/ext4 filesystem inspector.
//!
//! Usage:
//!   ext4fs-tool [--offset <bytes>] <command> <image> [args...]
//!
//! Commands:
//!   info <image>                     print superblock / group summary
//!   ls   <image> <path>              list a directory
//!   stat <image> <path>              show inode details for a path
//!   cat  <image> <path>              dump a file's contents to stdout
//!   dump <image> <path> <outfile>    save a file's contents to disk
//!   extract <image> <path> <dir>     copy a file/dir out to a local folder
//!   devices                          list physical disks and their partitions
//!   parts  <device-or-diskimage>     show partitions (MBR/GPT) and fs types
//!
//! `<image>` may be a filesystem image file, a disk image, or a raw device such
//! as `\\.\PhysicalDrive1` (needs administrator rights). Use `--offset` to point
//! at a partition inside a disk/disk image.

use ext4fs_tool::ext4::error::{ExtError, Result};
use ext4fs_tool::ext4::inode::{
    read_inode_data, S_IFBLK, S_IFCHR, S_IFDIR, S_IFIFO, S_IFLNK, S_IFMT, S_IFSOCK,
};
use ext4fs_tool::ext4::Ext4;

fn usage() -> ! {
    eprintln!("usage: ext4fs-tool [--offset <bytes>] <info|ls|stat|cat|dump|extract|devices|parts> <image|device> [args...]");
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        usage();
    }

    let mut offset = 0u64;
    let mut pos: Vec<String> = Vec::new();
    let mut it = args.iter().peekable();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--offset" => {
                offset = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
            }
            s if s.starts_with("--offset=") => {
                offset = s["--offset=".len()..].parse().unwrap_or(0);
            }
            "--" => {
                while let Some(x) = it.next() {
                    pos.push(x.clone());
                }
            }
            s if s.starts_with('-') && s.len() > 1 => {
                eprintln!("unknown option: {}", s);
                usage();
            }
            s => pos.push(s.to_string()),
        }
    }

    if pos.is_empty() {
        usage();
    }
    let cmd = pos[0].as_str();

    // Commands that do not need an open filesystem.
    if cmd == "devices" {
        match cmd_devices() {
            Ok(()) => return,
            Err(e) => {
                eprintln!("{}", e);
                std::process::exit(1);
            }
        }
    }
    if pos.len() < 2 {
        usage();
    }
    let image = pos[1].as_str();

    if cmd == "parts" {
        match cmd_parts(image) {
            Ok(()) => return,
            Err(e) => {
                eprintln!("{}", e);
                std::process::exit(1);
            }
        }
    }

    let fs = match open_fs(image, offset) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error opening {}: {}", image, e);
            std::process::exit(1);
        }
    };

    let result = match cmd {
        "info" => cmd_info(&fs),
        "ls" => {
            let path = pos.get(2).map(String::as_str).unwrap_or("/");
            cmd_ls(&fs, path)
        }
        "stat" => {
            let path = pos.get(2).map(String::as_str).unwrap_or("/");
            cmd_stat(&fs, path)
        }
        "cat" => {
            let path = pos.get(2).map(String::as_str).unwrap_or("/");
            cmd_cat(&fs, path)
        }
        "dump" => {
            if pos.len() < 4 {
                eprintln!("usage: ext4fs-tool dump <image> <path> <outfile>");
                std::process::exit(2);
            }
            cmd_dump(&fs, pos[2].as_str(), pos[3].as_str())
        }
        "extract" => {
            if pos.len() < 4 {
                eprintln!("usage: ext4fs-tool extract <image> <path> <destdir>");
                std::process::exit(2);
            }
            cmd_extract(&fs, pos[2].as_str(), pos[3].as_str())
        }
        "copy" => {
            if pos.len() < 4 {
                eprintln!("usage: ext4fs-tool copy <image> <path> <destfile-or-dir>");
                std::process::exit(2);
            }
            cmd_copy(&fs, pos[2].as_str(), pos[3].as_str())
        }
        _ => {
            eprintln!("unknown command: {}", cmd);
            usage();
        }
    };

    if let Err(e) = result {
        eprintln!("{}", e);
        std::process::exit(1);
    }
}

fn open_fs(image: &str, offset: u64) -> Result<Ext4> {
    match Ext4::open(image, offset) {
        Err(ExtError::Io(e))
            if e.kind() == std::io::ErrorKind::PermissionDenied && image.starts_with("\\\\.\\") =>
        {
            Err(ExtError::Unsupported(format!(
                "reading {} was denied. Raw disk access requires administrator privileges (run as administrator).",
                image
            )))
        }
        r => r,
    }
}

fn cmd_extract(fs: &Ext4, path: &str, dest: &str) -> Result<()> {
    let (out, errors) = ext4fs_tool::ext4::copy::extract(fs, path, std::path::Path::new(dest))?;
    println!("extracted {} -> {}", path, out.display());
    for e in &errors {
        println!("  warn: {}", e);
    }
    Ok(())
}

fn cmd_copy(fs: &Ext4, path: &str, dest: &str) -> Result<()> {
    let (out, errors) = ext4fs_tool::ext4::copy::copy_to(fs, path, std::path::Path::new(dest))?;
    println!("copied {} -> {}", path, out.display());
    for e in &errors {
        println!("  warn: {}", e);
    }
    Ok(())
}

fn cmd_devices() -> Result<()> {
    #[cfg(windows)]
    {
        let disks = ext4fs_tool::ext4::device::windows::enumerate_disks();
        if disks.is_empty() {
            println!("no physical disks found");
        }
        for d in disks {
            let mut head = d.path.clone();
            if let Some(m) = &d.model {
                head.push_str(&format!("   {}", m));
            }
            if let Some(sz) = d.size {
                head.push_str(&format!("   ({} bytes)", sz));
            }
            println!("{}", head);
            if let Some(err) = &d.error {
                println!("  {}", err);
                continue;
            }
            match list_partitions(&d.path) {
                Ok(t) => print_table(&mut std::io::stdout().lock(), &t, &d.path),
                Err(e) => println!("  (no partition table: {})", e),
            }
        }
    }
    #[cfg(not(windows))]
    {
        println!("device enumeration is only supported on Windows");
    }
    Ok(())
}

fn list_partitions(device: &str) -> Result<ext4fs_tool::ext4::partitions::PartitionTable> {
    let f = std::fs::File::open(device)?;
    #[cfg(windows)]
    let sector = ext4fs_tool::ext4::device::windows::sector_size_of(&f);
    #[cfg(not(windows))]
    let sector = 512u64;
    let mut f = f;
    ext4fs_tool::ext4::partitions::read_partition_table(&mut f, sector)
}

fn cmd_parts(device: &str) -> Result<()> {
    let table = list_partitions(device)?;
    print_table(&mut std::io::stdout().lock(), &table, device);
    Ok(())
}

fn print_table(
    out: &mut dyn std::io::Write,
    table: &ext4fs_tool::ext4::partitions::PartitionTable,
    device: &str,
) {
    let sector = {
        #[cfg(windows)]
        {
            let f = std::fs::File::open(device).ok();
            match &f {
                Some(f) => ext4fs_tool::ext4::device::windows::sector_size_of(f),
                None => 512,
            }
        }
        #[cfg(not(windows))]
        {
            512u64
        }
    };
    let _ = writeln!(out, "  {}", if table.is_gpt { "GPT partition table" } else { "MBR partition table" });
    if table.partitions.is_empty() {
        let _ = writeln!(out, "  (no partitions)");
    }
    for p in &table.partitions {
        let kind = match &p.kind {
            ext4fs_tool::ext4::partitions::PartKind::Mbr(t) => format!("MBR type 0x{:02x}", t),
            ext4fs_tool::ext4::partitions::PartKind::Gpt(g) => format!("GPT {}", guid_to_string(g)),
        };
        let start = p.start_bytes(sector);
        let fs = {
            let mut f = std::fs::File::open(device).ok();
            let f = f.as_mut();
            match f {
                Some(f) => ext4fs_tool::ext4::partitions::detect_fs(f, start),
                None => "?".into(),
            }
        };
        let label = {
            let mut f = std::fs::File::open(device).ok();
            let f = f.as_mut();
            match f {
                Some(f) => ext4fs_tool::ext4::partitions::detect_fs_label(f, start),
                None => String::new(),
            }
        };
        let _ = writeln!(
            out,
            "  #{} lba={} sectors={} bytes={} fs={} label={} {}  {}",
            p.index,
            p.start_lba,
            p.sectors,
            start,
            fs,
            label,
            p.name,
            kind
        );
    }
}

fn guid_to_string(g: &[u8; 16]) -> String {
    let b = g;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[3], b[2], b[1], b[0], b[5], b[4], b[7], b[6], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

fn cmd_info(fs: &Ext4) -> Result<()> {
    println!("superblock:");
    println!("  magic            0x{:04x}", fs.sb.magic);
    println!("  blocks            {}", fs.sb.blocks_count);
    println!("  inodes            {}", fs.sb.inodes_count);
    println!("  free blocks       {}", fs.sb.free_blocks_count);
    println!("  free inodes       {}", fs.sb.free_inodes_count);
    println!("  block size        {}", fs.block_size);
    println!("  blocks per group  {}", fs.sb.blocks_per_group);
    println!("  inodes per group  {}", fs.sb.inodes_per_group);
    println!("  inode size        {}", fs.sb.inode_size);
    println!("  rev level         {}", fs.sb.rev_level);
    println!("  mount count       {} / {}", fs.sb.mount_count, fs.sb.max_mount_count);
    println!("  state             0x{:04x}", fs.sb.state);
    println!("  groups            {}", fs.groups.len());
    println!("  volume name       '{}'", fs.sb.volume_name);
    println!("  last mounted      '{}'", fs.sb.last_mounted);
    println!("  uuid              {}", hex(&fs.sb.uuid));
    println!("features:");
    println!("  compatible        0x{:08x}", fs.sb.feature_compat);
    println!("  incompatible      0x{:08x}", fs.sb.feature_incompat);
    println!("  ro-compatible     0x{:08x}", fs.sb.feature_ro_compat);
    println!("block groups:");
    for (i, g) in fs.groups.iter().enumerate() {
        println!(
            "  group {:4}: free_blocks {:<8} free_inodes {:<6} used_dirs {:<4} block_bitmap {:>12} inode_bitmap {:>12} inode_table {:>12}",
            i, g.free_blocks_count, g.free_inodes_count, g.used_dirs_count, g.block_bitmap, g.inode_bitmap, g.inode_table
        );
    }
    Ok(())
}

fn cmd_ls(fs: &Ext4, path: &str) -> Result<()> {
    let ino = fs.resolve(path)?;
    let inode = fs.read_inode(ino)?;
    if !inode.is_dir() {
        return Err(ExtError::NotDir(ino));
    }
    let entries = fs.list_dir(&inode)?;
    let inos: Vec<u32> = entries.iter().map(|e| e.ino).collect();
    let inodes = fs.read_inodes_batch(&inos);
    let mut lines: Vec<(String, String)> = Vec::new();
    for (e, ino_opt) in entries.iter().zip(inodes.iter()) {
        let detail = match ino_opt {
            Some(i) => format!("{:>10} {}", i.size, type_char(i)),
            None => format!("{:>10} {}", 0, type_from_ft(e.file_type)),
        };
        lines.push((detail, e.name.clone()));
    }
    // sort by name for deterministic output
    lines.sort_by(|a, b| a.1.cmp(&b.1));
    for (d, n) in lines {
        println!("{}  {}", d, n);
    }
    Ok(())
}

fn cmd_stat(fs: &Ext4, path: &str) -> Result<()> {
    let ino = fs.resolve(path)?;
    let inode = fs.read_inode(ino)?;
    println!("path:      {}", path);
    println!("inode:     {}", inode.ino);
    println!("mode:      {:o} ({})", inode.mode & 0o7777, type_char(&inode));
    println!("uid:       {}", inode.uid);
    println!("gid:       {}", inode.gid);
    println!("size:      {}", inode.size);
    println!("blocks:    {} (512B units)", inode.blocks);
    println!("links:     {}", inode.links_count);
    println!("atime:     {}", inode.atime);
    println!("ctime:     {}", inode.ctime);
    println!("mtime:     {}", inode.mtime);
    println!("dtime:     {}", inode.dtime);
    println!("flags:     0x{:08x}", inode.flags);
    println!("generation {}", inode.generation);
    println!("file_acl:  {}", inode.file_acl);
    println!("extra:     extra_isize={}", inode.extra_isize);
    println!("extents:   {}", if inode.flags & 0x0008_0000 != 0 { "yes" } else { "no" });
    if inode.flags & 0x1000_0000 != 0 {
        let data = fs.inline_data(&inode)?;
        println!("inline:    {} bytes", data.len());
    } else if !inode.is_symlink() {
        let nblocks = (inode.size + fs.block_size - 1) / fs.block_size;
        println!("block map:");
        for lb in 0..nblocks {
            match fs.block_to_paddr(&inode, lb)? {
                Some(p) => println!("  logical {} -> physical {}", lb, p),
                None => println!("  logical {} -> (hole)", lb),
            }
        }
    }
    Ok(())
}

fn cmd_cat(fs: &Ext4, path: &str) -> Result<()> {
    let ino = fs.resolve(path)?;
    let inode = fs.read_inode(ino)?;
    if inode.is_dir() {
        return Err(ExtError::NotDir(ino));
    }
    let mut data = Vec::new();
    read_inode_data(fs, &inode, &mut data)?;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    use std::io::Write;
    out.write_all(&data)?;
    out.flush()?;
    Ok(())
}

fn cmd_dump(fs: &Ext4, path: &str, outfile: &str) -> Result<()> {
    let ino = fs.resolve(path)?;
    let inode = fs.read_inode(ino)?;
    if inode.is_dir() {
        return Err(ExtError::NotDir(ino));
    }
    let mut data = Vec::new();
    read_inode_data(fs, &inode, &mut data)?;
    std::fs::write(outfile, &data)?;
    println!("wrote {} bytes to {}", data.len(), outfile);
    Ok(())
}

// formatting helpers
// ---------------------------------------------------------------------------

fn type_char(inode: &ext4fs_tool::ext4::inode::Inode) -> char {
    match inode.mode & S_IFMT {
        S_IFDIR => 'd',
        S_IFLNK => 'l',
        S_IFCHR => 'c',
        S_IFBLK => 'b',
        S_IFIFO => 'p',
        S_IFSOCK => 's',
        _ => '-',
    }
}

fn type_from_ft(ft: u8) -> char {
    match ft {
        2 => 'd',
        7 => 'l',
        3 => 'c',
        4 => 'b',
        5 => 'p',
        6 => 's',
        _ => '-',
    }
}

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2 + b.len().saturating_sub(1));
    for (i, x) in b.iter().enumerate() {
        if i > 0 {
            s.push('-');
        }
        s.push_str(&format!("{:02x}", x));
    }
    s
}
