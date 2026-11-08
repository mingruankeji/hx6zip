# Hx6Zip

**中文** | [English](#english)

Hx6Zip 是一款面向开发者的 Windows 桌面压缩软件。核心压缩与解压逻辑使用 Rust 实现，界面基于 GPUI Kit；无需安装 7-Zip，也不依赖 7-Zip 动态库或外部 `7z.exe`。

它特别适合日常处理代码仓库和项目目录：压缩时可排除缓存、前端依赖、版本控制目录及占用中的文件；从压缩包拖出文件时，优先直接解压到目标位置，减少临时文件和重复复制的等待。并行处理与小文件优化也让常见项目归档更高效。性能取决于压缩格式、文件特征和硬件，不同场景的表现会有差异。

## 下载

从 [GitHub Releases](https://github.com/mingruankeji/hx6zip/releases) 下载：

- [Windows x64 安装版（MSI）](https://github.com/mingruankeji/hx6zip/releases/download/v0.1.2/Hx6Zip-Setup-0.1.2-x64.msi)
- [Windows x64 便携版（ZIP）](https://github.com/mingruankeji/hx6zip/releases/download/v0.1.2/Hx6Zip-Portable-0.1.2-x64.zip)

安装版可集成资源管理器右键菜单。当前安装包使用开发证书，Windows 可能提示“未知发布者”或请求管理员权限以信任证书；请仅安装你信任的软件。便携版不注册系统菜单。

## 主要功能

- **Rust 驱动，轻量独立**：桌面界面使用 GPUI Kit；支持多种常见归档格式，不需要系统安装 7-Zip 或调用其动态库。
- **为开发工作流优化**：创建归档时可按需排除常见缓存目录、`node_modules`、`.git` / `.svn` 版本控制目录，以及因文件共享冲突而无法读取的文件，避免无关文件拖慢项目打包。
- **快速拖出压缩包**：将选中的文件或文件夹拖到资源管理器，优先由 Hx6Zip 直接解压到目标位置，避免先完整解压到临时目录再复制；固实压缩格式和特殊目标可能需要更多处理。
- **链接不再悄悄丢失**：扫描到可记录的失效链接等特殊情况时，会在归档中保留说明清单；解压时可按清单尝试恢复受支持的链接，降低项目目录因链接无法还原而缺文件的风险。
- 浏览文件夹和压缩包；创建 ZIP、7z、TAR，查看、解压 ZIP、7z、RAR、CAB、ISO、TAR 及常见压缩流格式。
- 修改 ZIP 和 7z，支持添加、替换和删除内容；可处理加密 ZIP、7z 和 RAR。
- 在压缩包与文件夹之间拖放文件；支持选择性解压、Windows 右键快速压缩和解压。
- 分割文件并合并连续分片。
- 后台运行压缩任务，显示进度和速度，支持暂停与取消。
- 提供高速和安全两种完整性模式。安全模式会在发布压缩结果前进行额外校验，并在受支持的失败或取消场景下回滚文件操作。
- 内置 HTTP API 测试工具。

## 快速开始

1. 启动 Hx6Zip，在主窗口浏览文件，或直接打开压缩包。
2. 选择文件后使用工具栏、右键菜单或拖放执行压缩和解压。
3. 要添加或删除压缩包内的内容，请打开 ZIP 或 7z 后使用“添加”或“删除”。

## 安全与限制

- 仅创建 ZIP、7z、TAR；仅修改 ZIP、7z。暂不创建分卷压缩包或自解压文件。
- 高速模式为默认设置。安全模式提供额外校验和失败回滚，但不保证抵御断电、系统崩溃或强制结束进程。
- 外部程序接管的系统拖放由接收程序管理目标文件，不具备与“解压所选到”相同的事务回滚保证。
- 压缩包可能包含恶意内容。解压不可信文件时请谨慎；只有在信任压缩包时才恢复其中记录的特殊链接，因为链接目标可能位于解压目录之外。
- 文件格式自身的校验能力不同；对重要数据建议另行保存校验和并验证备份。

## 构建

需要 Rust 1.93 或更高版本。构建并运行 GUI：

```powershell
cargo build --release
.\target\release\hx6zip.exe
```

构建 Windows 安装包需要 WiX Toolset 6 和项目的 UI 扩展：

```powershell
wix extension add WixToolset.UI.wixext/6.0.2
.\installer\build.ps1
```

## English

Hx6Zip is a Windows desktop archive manager built for developers. Its core archive logic is written in Rust, and its UI uses GPUI Kit. It does not require 7-Zip to be installed and does not rely on 7-Zip DLLs or an external `7z.exe`.

Hx6Zip is designed for everyday project archives: optionally skip caches, frontend dependencies, version-control directories, and files locked by other processes. When dragging entries out of an archive, Hx6Zip can extract them directly to the target, reducing temporary-file and copy overhead. Parallel processing and small-file optimizations help keep common archive tasks responsive. Actual performance varies with archive format, data, and hardware.

### Download

Get the Windows x64 builds from [GitHub Releases](https://github.com/mingruankeji/hx6zip/releases):

- [Installer (MSI)](https://github.com/mingruankeji/hx6zip/releases/download/v0.1.2/Hx6Zip-Setup-0.1.2-x64.msi)
- [Portable version (ZIP)](https://github.com/mingruankeji/hx6zip/releases/download/v0.1.2/Hx6Zip-Portable-0.1.2-x64.zip)

The installer integrates with Explorer. The current MSI uses a development certificate, so Windows may show an unknown-publisher warning or request administrator approval to trust the certificate. Install only software you trust. The portable version does not register system context menus.

### Features

- **Developer-friendly filtering:** Optionally exclude common caches, `node_modules`, `.git` / `.svn` directories, and files that cannot be read because they are in use.
- **Fast drag-out:** Selected entries can be extracted directly to the drop target instead of being fully unpacked to a temporary directory and copied again. Solid archives and some targets may require additional work.
- **Link-aware archives:** When supported broken links are encountered, Hx6Zip can record them in an archive manifest and attempt to restore supported links during extraction, helping avoid missing project files.
- **Rust-based and independent:** The UI uses GPUI Kit, and archive operations do not require an installed 7-Zip application, its DLLs, or an external `7z.exe`.
- **Performance-focused:** Parallel processing and small-file optimizations improve common project-archive workflows. Results depend on the format, data, and hardware; no single speed claim applies to every workload.
- Browse files and archives; create ZIP, 7z, and TAR archives; open and extract ZIP, 7z, RAR, CAB, ISO, TAR, and common compressed-stream formats.
- Add, replace, and delete entries in ZIP and 7z archives. Encrypted ZIP, 7z, and RAR archives are supported.
- Drag and drop files between archives and folders, selectively extract entries, and use Explorer context-menu actions for quick compression and extraction.
- Split files and join numbered parts.
- Run background jobs with progress reporting, pause, and cancellation.
- Choose between high-speed and safe integrity modes. Safe mode performs additional verification and rolls back supported file operations on failure or cancellation.
- Use the built-in HTTP API testing tool.

### Safety notes

Hx6Zip creates ZIP, 7z, and TAR archives, and edits ZIP and 7z archives. It does not currently create split archives or self-extracting archives.

Safe mode does not guarantee recovery from power loss, system crashes, or forced process termination. When another application handles a system drag-and-drop operation, destination files are managed by that application and do not have the same transaction guarantees as **Extract to**.

Treat untrusted archives with caution. Restoring special links recorded in an archive may create links whose targets are outside the extraction directory. Archive formats also differ in their ability to detect corruption; keep separate checksums for important backups.

### Build from source

Rust 1.93 or later is required. Build and run the GUI with:

```powershell
cargo build --release
.\target\release\hx6zip.exe
```

Building the Windows installer also requires WiX Toolset 6 and the project's UI extension:

```powershell
wix extension add WixToolset.UI.wixext/6.0.2
.\installer\build.ps1
```
