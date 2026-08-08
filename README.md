# ext4fs-tool

一个纯 Rust 实现、**只读**的 ext2/ext3/ext4 文件系统工具：既可作为命令行工具使用，也带一个图形界面（GUI）浏览器，支持直接读取 Windows 下的物理磁盘 / 分区，并把文件复制出来。

纯标准库解析 ext4（不依赖外部 C 库），GUI 基于 egui/eframe。

## 功能特性

- **超级块 / 块组 / inode / extent 树 / 间接块** 解析
  - 支持 64bit、flex_bg、extents、内联数据（inline data）、大文件
  - 传统 ext2/3 间接块映射（直接 / 单重 / 双重 / 三重间接）
- **目录遍历**
  - 经典线性目录 + htree 索引目录（`dx_root` / `dx_node`，逻辑块→物理块映射与内核一致）
  - UTF-8 文件名，GUI 自动加载系统中文字体显示中文
- **复制文件出来（只读）**
  - 递归复制目录 / 文件到本地，跟随符号链接，坏链接只警告不中断
  - Windows 长路径（`\\?\`）与非法文件名字符自动处理
- **分区与磁盘访问**
  - MBR / GPT 分区表解析 + 文件系统类型探测（ext / NTFS / FAT / exFAT）
  - 直接读取 `\\.\PhysicalDriveN`（需管理员权限，GUI 内一键提权重启）
- **图形界面**
  - 左侧目录树、中央文件列表、右侧详情面板、状态栏
  - 框选（拖拽矩形）/ Ctrl / Shift 多选，右键菜单复制到指定文件夹
  - 后台线程复制 + 进度窗口（进度条 / 当前文件 / 取消按钮）
  - "Open disk" 自动 UAC 提权

## 构建

需要 Rust stable（[rustup.rs](https://rustup.rs)）。

```bash
cargo build --release --all-targets
```

产物：
- `target/release/ext4fs-tool.exe` — 命令行工具
- `target/release/ext4fs-tool-gui.exe` — 图形界面

> Windows 提示：
> - 使用 MSVC 工具链需安装 Visual Studio Build Tools。
> - 若使用 GNU 工具链（如 w64devkit），编译链接时需要提供 `libgcc_eh.a`（可用空库，通过 `LIBRARY_PATH` 指向）。
> - 读取物理磁盘必须**以管理员身份运行**；GUI 中点击 "Open disk" 会自动弹出 UAC 提权并重启。

## 命令行用法

```text
ext4fs-tool [--offset <bytes>] <info|ls|stat|cat|dump|extract|devices|parts> <image|device> [args...]
```

`<image>` 可以是镜像文件、带分区的磁盘镜像，或原始设备路径（如 `\\.\PhysicalDrive1`）。

```bash
# 查看超级块与块组信息
ext4fs-tool info disk.img

# 列出根目录
ext4fs-tool ls disk.img /

# 查看文件 inode / 块映射
ext4fs-tool stat disk.img /some/file

# 输出文件内容
ext4fs-tool cat disk.img /some/file

# 保存文件到本地
ext4fs-tool dump disk.img /some/file out.bin

# 递归复制文件/目录出来
ext4fs-tool extract disk.img /some/dir D:\dest

# 枚举物理磁盘及其分区（需管理员）
ext4fs-tool devices

# 查看磁盘/镜像的分区表与文件系统类型
ext4fs-tool parts \\.\PhysicalDrive1

# 指定分区偏移读取（MBR 分区起始 1048576 字节）
ext4fs-tool ls disk.img / --offset 1048576
```

## 图形界面

```
cargo run --release --bin ext4fs-tool-gui
```

- **Open image**：打开镜像文件
- **Open disk**：扫描物理磁盘分区（非管理员时自动 UAC 提权）
- 左侧目录树导航，双击进入目录 / 打开文件
- 文件区：拖拽框选、`Ctrl` 点击、`Shift` 点击多选
- 右键 → "Copy to folder..." 或 "Copy N selected to folder..." 复制到本地文件夹
- 复制在后台线程执行，弹出进度窗口，可随时取消

## 生成测试镜像

`examples/mkimg.rs` 是一个纯 Rust 的迷你 mkfs，用于验证解析器：

```bash
cargo run --example mkimg
```

生成：
- `test/ext4.img` — ext4：extents、htree 索引目录、内联数据、符号链接、中文文件名、稀疏文件
- `test/ext2.img` — ext2：直接/单重/双重间接块
- `test/mbr.img`、`test/gpt.img` — 上述 ext4 分区封装在 MBR / GPT 分区表中

## 目录结构

```
src/
  main.rs              # CLI
  lib.rs
  bin/ext4gui.rs       # GUI (egui)
  ext4/
    superblock.rs      # 超级块
    group.rs           # 块组描述符
    inode.rs           # inode、extent 树、间接块映射
    dir.rs             # 目录遍历（线性 + htree）
    partitions.rs      # MBR/GPT 分区表 + fs 探测
    device.rs          # Windows 原始磁盘访问（IOCTL）
    copy.rs            # 复制提取（进度/取消/长路径）
examples/
  mkimg.rs             # 测试镜像生成器
```

## 许可

[MIT](LICENSE)
