# Hx6Zip

**中文** | [English](#english)

Hx6Zip 是一款适用于 Windows 的桌面压缩软件，提供文件浏览、压缩包管理、Windows 资源管理器右键菜单和拖放操作。

## 下载

从 [GitHub Releases](https://github.com/mingruankeji/hx6zip/releases) 下载：

- [Windows x64 安装版（MSI）](https://github.com/mingruankeji/hx6zip/releases/download/v0.1.2/Hx6Zip-Setup-0.1.2-x64.msi)
- [Windows x64 便携版（ZIP）](https://github.com/mingruankeji/hx6zip/releases/download/v0.1.2/Hx6Zip-Portable-0.1.2-x64.zip)

安装版可集成资源管理器右键菜单。当前安装包使用开发证书，Windows 可能提示“未知发布者”或请求管理员权限以信任证书；请仅安装你信任的软件。便携版不注册系统菜单。

## 主要功能

- 浏览文件夹和压缩包；创建 ZIP、7z、TAR，查看、解压 ZIP、7z、RAR、CAB、ISO、TAR 及常见压缩流格式。
- 修改 ZIP 和 7z，支持添加、替换和删除内容；可处理加密 ZIP、7z 和 RAR。
- 在压缩包与文件夹之间拖放文件；支持选择性解压、Windows 右键快速压缩和解压。
- 分割文件并合并连续分片。
- 后台运行压缩任务，显示进度和速度，支持暂停与取消。
- 可选择忽略常见缓存目录、`node_modules`、版本控制目录或被占用的文件；必要时在压缩包中生成说明报告。
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

Hx6Zip is a desktop archive manager for Windows. It combines file browsing with archive management, drag-and-drop operations, and Windows Explorer context-menu integration.

### Download

Get the Windows x64 builds from [GitHub Releases](https://github.com/mingruankeji/hx6zip/releases):

- [Installer (MSI)](https://github.com/mingruankeji/hx6zip/releases/download/v0.1.2/Hx6Zip-Setup-0.1.2-x64.msi)
- [Portable version (ZIP)](https://github.com/mingruankeji/hx6zip/releases/download/v0.1.2/Hx6Zip-Portable-0.1.2-x64.zip)

The installer integrates with Explorer. The current MSI uses a development certificate, so Windows may show an unknown-publisher warning or request administrator approval to trust the certificate. Install only software you trust. The portable version does not register system context menus.

### Features

- Browse files and archives; create ZIP, 7z, and TAR archives; open and extract ZIP, 7z, RAR, CAB, ISO, TAR, and common compressed-stream formats.
- Add, replace, and delete entries in ZIP and 7z archives. Encrypted ZIP, 7z, and RAR archives are supported.
- Drag and drop files between archives and folders, selectively extract entries, and use Explorer context-menu actions for quick compression and extraction.
- Split files and join numbered parts.
- Run background jobs with progress reporting, pause, and cancellation.
- Optionally skip common caches, `node_modules`, version-control directories, or locked files when creating archives.
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
