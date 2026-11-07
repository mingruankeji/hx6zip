use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs::{self, File},
    io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{SyncSender, TrySendError, sync_channel},
    },
    time::SystemTime,
};

use anyhow::{Context, Result, bail};
use sevenz_rust2::{Archive, ArchiveReader, Block, BlockDecoder, EncoderMethod, Password};
use tempfile::{NamedTempFile, TempDir, TempPath};
use zip::{CompressionMethod, ZipArchive};

use super::{
    ArchiveEntry, ArchiveInfo, ArchiveKind, Cancelled, ExtractOptions, IO_BUFFER_SIZE,
    IntegrityMode, JobControl, Progress, SEVENZIP_PARALLEL_THRESHOLD,
    common::{
        Tracker, check_target, checked_metadata, collision_key, copy_stream, ensure_directory,
        ensure_root, extraction_metadata, persist_temp, safe_relative_path,
    },
    inspect::{inspect_sevenzip, inspect_zip},
    iso::{IsoExtent, read_iso},
    other::{inspect_other, open_stream, read_tar_entries},
    rar,
    sfx::{OffsetReader, sevenzip_offset},
    timestamps::{set_directory_modified, set_file_modified},
};

#[derive(Clone)]
struct PlannedEntry {
    entry: ArchiveEntry,
    relative: PathBuf,
}

#[derive(Clone)]
struct SharedCursor {
    #[cfg(any(windows, unix))]
    file: Arc<File>,
    #[cfg(not(any(windows, unix)))]
    file: Arc<Mutex<File>>,
    position: u64,
    length: u64,
}

impl Read for SharedCursor {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = self.read_at(buffer, self.position)?;
        self.position += count as u64;
        Ok(count)
    }
}

impl SharedCursor {
    fn read_at(&self, buffer: &mut [u8], position: u64) -> io::Result<usize> {
        #[cfg(windows)]
        let count = {
            use std::os::windows::fs::FileExt;
            self.file.seek_read(buffer, position)?
        };
        #[cfg(unix)]
        let count = {
            use std::os::unix::fs::FileExt;
            self.file.read_at(buffer, position)?
        };
        #[cfg(not(any(windows, unix)))]
        let count = {
            let mut file = self
                .file
                .lock()
                .map_err(|_| io::Error::other("压缩包读取状态异常"))?;
            file.seek(SeekFrom::Start(position))?;
            file.read(buffer)?
        };
        Ok(count)
    }
}

impl Seek for SharedCursor {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        self.position = match from {
            SeekFrom::Start(position) => Some(position),
            SeekFrom::Current(offset) => self.position.checked_add_signed(offset),
            SeekFrom::End(offset) => self.length.checked_add_signed(offset),
        }
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "无效的压缩包读取位置"))?;
        Ok(self.position)
    }
}

pub(super) struct SharedInput(BufReader<SharedCursor>);

impl SharedInput {
    pub(super) fn new(file: File, length: u64) -> Self {
        Self(BufReader::with_capacity(
            IO_BUFFER_SIZE,
            SharedCursor {
                #[cfg(any(windows, unix))]
                file: Arc::new(file),
                #[cfg(not(any(windows, unix)))]
                file: Arc::new(Mutex::new(file)),
                position: 0,
                length,
            },
        ))
    }

    fn read_exact_at(&self, mut position: u64, mut buffer: &mut [u8]) -> io::Result<()> {
        let cursor = self.0.get_ref();
        while !buffer.is_empty() {
            match cursor.read_at(buffer, position) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(count) => {
                    position = position
                        .checked_add(count as u64)
                        .ok_or_else(|| io::Error::other("压缩包读取位置溢出"))?;
                    buffer = &mut buffer[count..];
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn direct_cursor(&self) -> SharedCursor {
        self.0.get_ref().clone()
    }
}

impl Clone for SharedInput {
    fn clone(&self) -> Self {
        let mut cursor = self.0.get_ref().clone();
        cursor.position -= self.0.buffer().len() as u64;
        Self(BufReader::with_capacity(IO_BUFFER_SIZE, cursor))
    }
}

impl Read for SharedInput {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.0.read(buffer)
    }
}

impl Seek for SharedInput {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        self.0.seek(from)
    }
}

pub fn smart_destination(info: &ArchiveInfo, base: &Path) -> Result<PathBuf> {
    let mut roots = HashSet::new();
    for entry in &info.entries {
        if matches!(info.format, ArchiveKind::Zip | ArchiveKind::SevenZip)
            && entry
                .name
                .eq_ignore_ascii_case(super::special::MANIFEST_NAME)
        {
            continue;
        }
        let path = safe_relative_path(&entry.name)?;
        if let Some(root) = path.components().next() {
            roots.insert(root.as_os_str().to_string_lossy().to_lowercase());
        }
    }
    if roots.len() <= 1 {
        Ok(base.to_path_buf())
    } else {
        named_destination(&info.path, base)
    }
}

pub fn named_destination(path: &Path, base: &Path) -> Result<PathBuf> {
    let stem = hx6zip_shell_protocol::archive_stem(path)
        .and_then(|value| value.to_str())
        .context("无法确定解压文件夹名称")?;
    let safe_stem = safe_relative_path(stem)?;
    Ok(base.join(safe_stem))
}

pub fn extract_archive(
    info: &ArchiveInfo,
    options: &ExtractOptions,
    control: &JobControl,
) -> Result<()> {
    control.with_archive_password(
        &info.path,
        options.password.as_deref(),
        info.entries.iter().any(|entry| {
            entry.is_encrypted == Some(true)
                && options
                    .selected
                    .as_ref()
                    .is_none_or(|selected| selected.contains(&entry.index))
        }),
        control.integrity() == IntegrityMode::Safe,
        |password| {
            let options = ExtractOptions {
                password: password.map(str::to_owned),
                ..options.clone()
            };
            extract_archive_with_password(info, &options, control)
        },
    )
}

fn extract_archive_with_password(
    info: &ArchiveInfo,
    options: &ExtractOptions,
    control: &JobControl,
) -> Result<()> {
    extract_archive_with_password_mode(info, options, control, false)
}

pub(super) fn extract_archive_for_export(
    info: &ArchiveInfo,
    options: &ExtractOptions,
    control: &JobControl,
) -> Result<()> {
    control.with_archive_password(
        &info.path,
        options.password.as_deref(),
        info.entries.iter().any(|entry| {
            entry.is_encrypted == Some(true)
                && options
                    .selected
                    .as_ref()
                    .is_none_or(|selected| selected.contains(&entry.index))
        }),
        control.integrity() == IntegrityMode::Safe,
        |password| {
            let options = ExtractOptions {
                password: password.map(str::to_owned),
                ..options.clone()
            };
            extract_archive_with_password_mode(info, &options, control, true)
        },
    )
}

fn extract_archive_with_password_mode(
    info: &ArchiveInfo,
    options: &ExtractOptions,
    control: &JobControl,
    isolated_export: bool,
) -> Result<()> {
    if isolated_export {
        control.check_cancelled()?;
        let plan = plan_entries(info, options.selected.as_ref(), control)?;
        extract_archive_staged(info, options, &plan, control)?;
        if control.integrity() == IntegrityMode::Safe {
            verify_export_entries(&options.destination, &plan, control)?;
        }
    } else {
        extract_archive_without_special(info, options, control)?;
    }
    let scopes = match &options.selected {
        None => vec![(PathBuf::new(), PathBuf::new())],
        Some(selected) => info
            .entries
            .iter()
            .filter(|entry| entry.is_directory && selected.contains(&entry.index))
            .map(|entry| {
                let path = safe_relative_path(&entry.name)?;
                Ok((path.clone(), path))
            })
            .collect::<Result<Vec<_>>>()?,
    };
    let extracted_manifest = info.entries.iter().find(|entry| {
        !entry.is_directory
            && entry
                .name
                .eq_ignore_ascii_case(super::special::MANIFEST_NAME)
            && options
                .selected
                .as_ref()
                .is_none_or(|selected| selected.contains(&entry.index))
    });
    restore_special_entries(info, options, &scopes, extracted_manifest, control)
}

fn verify_export_entries(root: &Path, plan: &[PlannedEntry], control: &JobControl) -> Result<()> {
    for item in plan {
        control.check_cancelled()?;
        let path = root.join(&item.relative);
        let metadata = checked_metadata(&path)?
            .with_context(|| format!("解压结果缺少条目：{}", item.entry.name))?;
        if metadata.is_dir() != item.entry.is_directory
            || (!item.entry.is_directory
                && (!metadata.is_file() || metadata.len() != item.entry.size))
        {
            bail!("解压结果不完整：{}", item.entry.name);
        }
    }
    Ok(())
}

pub(super) fn extract_archive_without_special(
    info: &ArchiveInfo,
    options: &ExtractOptions,
    control: &JobControl,
) -> Result<()> {
    control.check_cancelled()?;
    let plan = plan_entries(info, options.selected.as_ref(), control)?;
    extract_planned(info, options, &plan, control, |staged| {
        extract_archive_staged(info, staged, &plan, control)
    })
}

pub(super) fn read_special_manifest(
    info: &ArchiveInfo,
    password: Option<&str>,
    control: &JobControl,
) -> Result<Option<super::special::Manifest>> {
    use super::special;

    if !matches!(info.format, ArchiveKind::Zip | ArchiveKind::SevenZip) {
        return Ok(None);
    }
    let Some(entry) = info.entries.iter().find(|entry| {
        !entry.is_directory && entry.name.eq_ignore_ascii_case(special::MANIFEST_NAME)
    }) else {
        return Ok(None);
    };
    anyhow::ensure!(
        entry.size <= special::MAX_MANIFEST_SIZE as u64,
        "特殊情况清单超过 8 MiB"
    );
    let temporary = tempfile::Builder::new()
        .prefix("hx6zip-special-")
        .tempdir()?;
    let relative = safe_relative_path(&entry.name)?;
    extract_archive_staged(
        info,
        &ExtractOptions {
            destination: temporary.path().to_path_buf(),
            password: password.map(str::to_owned),
            selected: Some(BTreeSet::from([entry.index])),
            overwrite: false,
        },
        &[PlannedEntry {
            entry: entry.clone(),
            relative: relative.clone(),
        }],
        control,
    )?;
    let bytes = fs::read(temporary.path().join(relative))?;
    special::parse(&bytes).map(Some)
}

fn restore_special_entries(
    info: &ArchiveInfo,
    options: &ExtractOptions,
    scopes: &[(PathBuf, PathBuf)],
    extracted_manifest: Option<&ArchiveEntry>,
    control: &JobControl,
) -> Result<()> {
    use super::special::{self, SpecialKind};

    if scopes.is_empty() || !matches!(info.format, ArchiveKind::Zip | ArchiveKind::SevenZip) {
        return Ok(());
    }
    control.check_cancelled()?;
    let manifest = match read_special_manifest(info, options.password.as_deref(), control) {
        Ok(Some(manifest)) => manifest,
        Ok(None) => return Ok(()),
        Err(error) => {
            control.check_cancelled()?;
            control.warn(format!("特殊情况清单未处理：{error:#}"));
            return Ok(());
        }
    };
    let root = match ensure_root(&options.destination) {
        Ok(root) => root,
        Err(error) => {
            control.warn(format!("特殊条目未恢复，无法访问解压目录：{error:#}"));
            return Ok(());
        }
    };
    let mut names = HashSet::new();
    let mut complete = true;
    for entry in &manifest.entries {
        control.check_cancelled()?;
        let mut restore = || -> Result<()> {
            let relative = safe_relative_path(&entry.path)?;
            let key = collision_key(&relative);
            anyhow::ensure!(names.insert(key.clone()), "清单包含重复路径");
            let Some((source, destination)) = scopes.iter().find(|(source, _)| {
                relative
                    .ancestors()
                    .any(|ancestor| collision_key(ancestor) == collision_key(source))
            }) else {
                complete = false;
                return Ok(());
            };
            if matches!(entry.kind, SpecialKind::Skipped) {
                bail!("原始内容未备份：{}", entry.reason);
            }
            anyhow::ensure!(
                !info.entries.iter().any(|ordinary| {
                    safe_relative_path(&ordinary.name).is_ok_and(|path| {
                        path.ancestors()
                            .any(|ancestor| collision_key(ancestor) == key)
                    })
                }),
                "特殊条目与压缩包普通条目冲突"
            );
            let suffix: PathBuf = relative
                .components()
                .skip(source.components().count())
                .collect();
            let target_relative = destination.join(suffix);
            let target_relative = safe_relative_path(&target_relative.to_string_lossy())?;
            let mut parent = root.clone();
            if let Some(parents) = target_relative.parent() {
                for component in parents.components() {
                    parent.push(component);
                    match checked_metadata(&parent)? {
                        Some(metadata) => anyhow::ensure!(metadata.is_dir(), "链接父路径不是目录"),
                        None => fs::create_dir(&parent)?,
                    }
                }
            }
            let target = root.join(target_relative);
            anyhow::ensure!(
                fs::symlink_metadata(&target).is_err_and(|e| e.kind() == io::ErrorKind::NotFound),
                "恢复位置已存在或无法访问，不覆盖现有项目"
            );
            special::restore_link(entry, &target)
        };
        if let Err(error) = restore() {
            complete = false;
            control.warn(format!("特殊条目未恢复：{}：{error:#}", entry.path));
        }
    }
    control.check_cancelled()?;
    if complete && let Some(entry) = extracted_manifest {
        let cleanup = || -> Result<()> {
            let path = root.join(safe_relative_path(&entry.name)?);
            let metadata = checked_metadata(&path)?.context("解压后的特殊情况清单已不存在")?;
            anyhow::ensure!(
                metadata.is_file() && metadata.len() <= special::MAX_MANIFEST_SIZE as u64,
                "解压后的特殊情况清单类型或大小已变化"
            );
            let extracted = special::parse(&fs::read(&path)?)?;
            anyhow::ensure!(
                extracted.entries == manifest.entries,
                "解压后的特殊情况清单内容已变化"
            );
            fs::remove_file(&path).context("无法删除已处理的特殊情况清单")
        };
        if let Err(error) = cleanup() {
            control.warn(format!("特殊链接已处理，但清单未删除：{error:#}"));
        }
    }
    Ok(())
}

fn extract_planned(
    info: &ArchiveInfo,
    options: &ExtractOptions,
    plan: &[PlannedEntry],
    control: &JobControl,
    extract: impl FnOnce(&ExtractOptions) -> Result<()>,
) -> Result<()> {
    if control.integrity() == IntegrityMode::Normal {
        return extract(options);
    }
    let mut transaction = ExtractionTransaction::new(&options.destination)?;
    let result = (|| {
        check_destination(info, options, plan, &transaction.root)?;
        let staged_options = ExtractOptions {
            destination: transaction.work.path().join("data"),
            overwrite: false,
            ..options.clone()
        };
        extract(&staged_options)?;
        let total = total_selected(plan)?;
        let mut verified = 0u64;
        // A decoder returning success is insufficient if it omitted a planned entry.
        for item in plan {
            control.report(Progress {
                operation: "正在校验解压结果",
                current_file: item.entry.name.clone(),
                completed_bytes: verified,
                total_bytes: total,
                archive_size: None,
            });
            control.check_cancelled()?;
            let path = staged_options.destination.join(&item.relative);
            let metadata = checked_metadata(&path)?
                .with_context(|| format!("解压结果缺少条目：{}", item.entry.name))?;
            if metadata.is_dir() != item.entry.is_directory
                || (!item.entry.is_directory
                    && (!metadata.is_file() || metadata.len() != item.entry.size))
            {
                bail!("解压结果不完整：{}", item.entry.name);
            }
            verified = verified.saturating_add(item.entry.size).min(total);
        }
        check_destination(info, options, plan, &transaction.root)?;
        let mut published = 0u64;
        for item in plan {
            control.report(Progress {
                operation: "正在提交解压结果",
                current_file: item.entry.name.clone(),
                completed_bytes: published,
                total_bytes: total,
                archive_size: None,
            });
            control.check_cancelled()?;
            transaction.publish(
                &staged_options.destination,
                item,
                options.overwrite,
                control,
            )?;
            published = published.saturating_add(item.entry.size).min(total);
        }
        restore_directory_times(&transaction.root, plan, control)?;
        control.check_cancelled()
    })();
    transaction.finish(result)
}

fn plan_drag_entries(
    info: &ArchiveInfo,
    paths: &[String],
    control: &JobControl,
) -> Result<Vec<PlannedEntry>> {
    control.check_cancelled()?;
    let selected = super::archive_selection(info, paths)?;
    let mut plan = plan_entries(info, Some(&selected), control)?;
    let mut roots = paths
        .iter()
        .map(|name| safe_relative_path(name))
        .collect::<Result<Vec<_>>>()?;
    roots.sort_by_key(|path| path.components().count());
    for root in &roots {
        let key = collision_key(root);
        if info.entries.iter().any(|entry| {
            info.skips_iso_symlink(entry) && collision_key(Path::new(&entry.name)) == key
        }) {
            continue;
        }
        anyhow::ensure!(
            plan.iter()
                .any(|item| item.relative.ancestors().any(|p| collision_key(p) == key)),
            "拖放项目已不存在：{}",
            root.display()
        );
    }
    let mut names = HashMap::new();
    for item in &mut plan {
        control.check_cancelled()?;
        let root = roots
            .iter()
            .find(|root| {
                let key = collision_key(root);
                item.relative.ancestors().any(|p| collision_key(p) == key)
            })
            .context("无法确定拖放条目的目标路径")?;
        item.relative = item
            .relative
            .components()
            .skip(root.components().count() - 1)
            .collect();
        anyhow::ensure!(
            names
                .insert(collision_key(&item.relative), item.entry.is_directory)
                .is_none(),
            "拖放项目存在同名目标，请分开解压：{}",
            item.relative.display()
        );
    }
    for item in &plan {
        for parent in item.relative.ancestors().skip(1) {
            anyhow::ensure!(
                names.get(&collision_key(parent)) != Some(&false),
                "拖放项目存在文件与文件夹冲突：{}",
                item.relative.display()
            );
        }
    }
    Ok(plan)
}

pub fn drag_conflicts(
    info: &ArchiveInfo,
    paths: &[String],
    destination: &Path,
    control: &JobControl,
) -> Result<Vec<PathBuf>> {
    let plan = plan_drag_entries(info, paths, control)?;
    planned_conflicts(info, destination, &plan, control)
}

pub fn extract_conflicts(
    info: &ArchiveInfo,
    options: &ExtractOptions,
    control: &JobControl,
) -> Result<Vec<PathBuf>> {
    control.check_cancelled()?;
    let plan = plan_entries(info, options.selected.as_ref(), control)?;
    planned_conflicts(info, &options.destination, &plan, control)
}

fn planned_conflicts(
    info: &ArchiveInfo,
    destination: &Path,
    plan: &[PlannedEntry],
    control: &JobControl,
) -> Result<Vec<PathBuf>> {
    let root = std::path::absolute(destination).context("无法确定解压目标路径")?;
    for ancestor in root.ancestors() {
        control.check_cancelled()?;
        if let Some(metadata) = extraction_metadata(ancestor)? {
            anyhow::ensure!(
                metadata.is_dir(),
                "解压目标路径不是文件夹：{}",
                ancestor.display()
            );
        }
    }
    let options = ExtractOptions {
        destination: root.clone(),
        password: None,
        selected: None,
        overwrite: true,
    };
    check_destination(info, &options, plan, &root)?;
    let mut conflicts = Vec::new();
    for item in plan {
        control.check_cancelled()?;
        let path = root.join(&item.relative);
        if !item.entry.is_directory && checked_metadata(&path)?.is_some() {
            conflicts.push(path);
        }
    }
    Ok(conflicts)
}

pub fn extract_drag_entries(
    info: &ArchiveInfo,
    paths: &[String],
    destination: &Path,
    password: Option<&str>,
    overwrite: bool,
    control: &JobControl,
) -> Result<()> {
    let selected = super::archive_selection(info, paths)?;
    control.with_archive_password(
        &info.path,
        password,
        info.entries
            .iter()
            .any(|entry| selected.contains(&entry.index) && entry.is_encrypted == Some(true)),
        control.integrity() == IntegrityMode::Safe,
        |password| {
            extract_drag_with_password(info, paths, destination, password, overwrite, control)
        },
    )
}

fn extract_drag_with_password(
    info: &ArchiveInfo,
    paths: &[String],
    destination: &Path,
    password: Option<&str>,
    overwrite: bool,
    control: &JobControl,
) -> Result<()> {
    let plan = plan_drag_entries(info, paths, control)?;
    let mut open = fs::OpenOptions::new();
    open.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        open.share_mode(windows::Win32::Storage::FileSystem::FILE_SHARE_READ.0);
    }
    // Keep the source locked through extraction and transaction commit.
    let _source = open.open(&info.path).context("无法打开拖放压缩包")?;
    let options = ExtractOptions {
        destination: destination.to_path_buf(),
        password: password.map(str::to_owned),
        selected: None,
        overwrite,
    };
    extract_planned(info, &options, &plan, control, |staged| {
        extract_archive_staged(info, staged, &plan, control)
    })?;
    let scopes = paths
        .iter()
        .map(|name| {
            let source = safe_relative_path(name)?;
            let destination = PathBuf::from(source.file_name().context("拖放路径无效")?);
            Ok((source, destination))
        })
        .collect::<Result<Vec<_>>>()?;
    restore_special_entries(info, &options, &scopes, None, control)
}

struct PublishedFile {
    target: PathBuf,
    backup: Option<PathBuf>,
    installed: bool,
}

fn move_extracted_file(source: &Path, target: &Path, control: &JobControl) -> Result<()> {
    let source_path = TempPath::try_from_path(source)?;
    match source_path.persist_noclobber(target) {
        Ok(()) => return Ok(()),
        Err(error) => {
            let io_error = error.error;
            error.path.keep().context("无法保留未提交的事务源文件")?;
            if io_error.kind() != io::ErrorKind::CrossesDevices {
                return Err(io_error).context("无法移动解压事务文件");
            }
        }
    }
    let mut input = File::open(source)?;
    let metadata = input.metadata()?;
    let mut temporary = NamedTempFile::new_in(target.parent().context("无效的事务目标")?)?;
    copy_stream(
        &mut input,
        temporary.as_file_mut(),
        metadata.len(),
        control,
        |_| Ok(()),
    )?;
    set_file_modified(temporary.as_file(), Some(metadata.modified()?))?;
    temporary.as_file().sync_all()?;
    control.check_cancelled()?;
    persist_temp(temporary, target, false)?;
    if let Err(error) = fs::remove_file(source) {
        fs::remove_file(target).context("移动事务文件失败且无法撤销目标副本")?;
        return Err(error).context("无法删除已复制的事务源文件");
    }
    Ok(())
}

struct ExtractionTransaction {
    root: PathBuf,
    work: TempDir,
    files: Vec<PublishedFile>,
    created_directories: BTreeMap<String, PathBuf>,
    directory_times: BTreeMap<String, (PathBuf, SystemTime)>,
}

impl ExtractionTransaction {
    fn new(destination: &Path) -> Result<Self> {
        let root = std::path::absolute(destination).context("无法确定解压目标路径")?;
        let mut parent = root.parent().unwrap_or(&root);
        while extraction_metadata(parent)?.is_none() {
            parent = parent.parent().context("找不到可用的目标文件夹")?;
        }
        // Validate every existing ancestor, not just the destination itself.
        for ancestor in root.ancestors() {
            if let Some(metadata) = extraction_metadata(ancestor)?
                && !metadata.is_dir()
            {
                bail!("目标路径不是文件夹：{}", ancestor.display());
            }
        }
        let modified = fs::metadata(parent)?.modified()?;
        let work = tempfile::Builder::new()
            .prefix(".hx6zip-extract-")
            .tempdir_in(parent)
            .context("无法创建解压事务文件夹")?;
        let parent = fs::canonicalize(parent).context("无法解析事务文件夹路径")?;
        let directory_times = BTreeMap::from([(collision_key(&parent), (parent, modified))]);
        Ok(Self {
            root,
            work,
            files: Vec::new(),
            created_directories: BTreeMap::new(),
            directory_times,
        })
    }

    fn directory(&mut self, path: &Path) -> Result<()> {
        if let Some(metadata) = extraction_metadata(path)? {
            if !metadata.is_dir() {
                bail!("目标路径不是文件夹：{}", path.display());
            }
            let path = fs::canonicalize(path)
                .with_context(|| format!("无法解析目标文件夹 {}", path.display()))?;
            if !self.created_directories.contains_key(&collision_key(&path)) {
                self.directory_times
                    .entry(collision_key(&path))
                    .or_insert((path, metadata.modified()?));
            }
        } else {
            self.directory(path.parent().context("无效的目标文件夹")?)?;
            fs::create_dir(path).with_context(|| format!("无法创建 {}", path.display()))?;
            let path = fs::canonicalize(path)
                .with_context(|| format!("无法解析新建文件夹 {}", path.display()))?;
            self.created_directories.insert(collision_key(&path), path);
        }
        Ok(())
    }

    fn publish(
        &mut self,
        stage: &Path,
        item: &PlannedEntry,
        overwrite: bool,
        control: &JobControl,
    ) -> Result<()> {
        let target = self.root.join(&item.relative);
        if item.entry.is_directory {
            return self.directory(&target);
        }
        self.directory(target.parent().context("无效的解压目标")?)?;
        let exists = check_target(&self.root, &item.relative, false, overwrite)?;
        let target = fs::canonicalize(target.parent().context("无效的解压目标")?)?
            .join(target.file_name().context("无效的解压文件名")?);
        let backup = if exists {
            let backup_root = ensure_directory(self.work.path(), Path::new("backup"))?;
            ensure_directory(
                &backup_root,
                item.relative.parent().unwrap_or_else(|| Path::new("")),
            )?;
            let backup = backup_root.join(&item.relative);
            move_extracted_file(&target, &backup, control)
                .with_context(|| format!("无法备份待覆盖文件 {}", target.display()))?;
            Some(backup)
        } else {
            None
        };
        self.files.push(PublishedFile {
            target: target.clone(),
            backup,
            installed: false,
        });
        // A linked directory may reside on another volume; publish from a local temporary file.
        move_extracted_file(&stage.join(&item.relative), &target, control)
            .with_context(|| format!("无法提交解压文件 {}", target.display()))?;
        self.files
            .last_mut()
            .expect("recorded publication")
            .installed = true;
        Ok(())
    }

    fn finish(mut self, result: Result<()>) -> Result<()> {
        let mut errors = Vec::new();
        if result.is_err() {
            for file in self.files.iter().rev() {
                if file.installed
                    && let Err(error) = fs::remove_file(&file.target)
                {
                    errors.push(format!("无法撤销 {}：{error}", file.target.display()));
                    continue;
                }
                if let Some(backup) = &file.backup
                    && let Err(error) =
                        move_extracted_file(backup, &file.target, &JobControl::default())
                {
                    errors.push(format!("无法恢复 {}：{error}", file.target.display()));
                }
            }
            let mut directories = self.created_directories.values().collect::<Vec<_>>();
            directories.sort_by_key(|path| Reverse(path.components().count()));
            for directory in directories {
                if let Err(error) = fs::remove_dir(directory) {
                    errors.push(format!("无法撤销文件夹 {}：{error}", directory.display()));
                }
            }
        } else {
            self.directory_times.clear();
        }
        let work = self.work.keep();
        if errors.is_empty()
            && let Err(error) = fs::remove_dir_all(&work)
        {
            errors.push(format!("无法清理解压事务文件夹：{error}"));
        }
        let mut times = self.directory_times.into_values().collect::<Vec<_>>();
        times.sort_by_key(|(path, _)| Reverse(path.components().count()));
        for (path, modified) in times {
            if let Err(error) = set_directory_modified(&path, Some(modified)) {
                errors.push(format!("无法恢复文件夹时间 {}：{error:#}", path.display()));
            }
        }
        if !errors.is_empty() {
            // Do not retain Cancelled in the error chain: the UI must show recovery failures.
            bail!(
                "{}；回滚或清理未完成：{}。请保留恢复目录并人工检查：{}",
                result.as_ref().err().map_or_else(
                    || "解压结果已完整提交".to_owned(),
                    |error| format!("{error:#}")
                ),
                errors.join("；"),
                work.display()
            );
        }
        result
    }
}

fn extract_archive_staged(
    info: &ArchiveInfo,
    options: &ExtractOptions,
    plan: &[PlannedEntry],
    control: &JobControl,
) -> Result<()> {
    control.check_cancelled()?;
    let mut file = File::open(&info.path)
        .with_context(|| format!("无法打开压缩包 {}", info.path.display()))?;
    let metadata = file.metadata().context("无法读取压缩包信息")?;
    match info.format {
        ArchiveKind::Zip => {
            let input = SharedInput::new(file, metadata.len());
            let mut reader = ZipArchive::new(input.clone()).context("无法读取 ZIP 压缩包")?;
            let fresh = inspect_zip(&info.path, &metadata, &mut reader, control)?;
            verify_unchanged(info, &fresh)?;
            let root = prepare_destination(&fresh, options, plan)?;
            extract_zip(&mut reader, &input, &root, options, plan, control)
        }
        ArchiveKind::SevenZip => {
            let password = Password::from(options.password.as_deref().unwrap_or_default());
            let offset = sevenzip_offset(&mut file, control)?;
            let input = OffsetReader::new(SharedInput::new(file, metadata.len()), offset)?;
            let reader = ArchiveReader::new(input.clone(), password.clone())
                .context("无法读取 7z 压缩包，密码可能不正确或格式不受支持")?;
            let fresh = inspect_sevenzip(&info.path, &metadata, &reader, control)?;
            verify_unchanged(info, &fresh)?;
            let root = prepare_destination(&fresh, options, plan)?;
            let workers = sevenzip_parallel_workers(
                reader.archive(),
                std::thread::available_parallelism()
                    .context("无法获取可用处理器数量")?
                    .get(),
            );
            extract_sevenzip_parallel(
                input,
                reader.archive(),
                &password,
                &root,
                options,
                plan,
                control,
                workers,
            )
        }
        ArchiveKind::Iso => {
            let mut file = file;
            let layout = read_iso(&info.path, &metadata, &mut file, control)?;
            verify_unchanged(info, &layout.info)?;
            let root = prepare_destination(&layout.info, options, plan)?;
            extract_iso(
                SharedInput::new(file, metadata.len()),
                &layout.extents,
                &root,
                options,
                plan,
                control,
            )
        }
        format => {
            let mut file = file;
            let fresh = inspect_other(
                &info.path,
                &metadata,
                file.try_clone().context("无法重新读取压缩包")?,
                format,
                options.password.as_deref(),
                control,
            )?;
            verify_unchanged(info, &fresh)?;
            let root = prepare_destination(&fresh, options, plan)?;
            file.rewind().context("无法重新读取压缩包")?;
            extract_other(file, &fresh, &root, options, plan, control)
        }
    }
}

pub(super) fn verify_unchanged(previous: &ArchiveInfo, fresh: &ArchiveInfo) -> Result<()> {
    if previous.archive_size != fresh.archive_size
        || previous.modified != fresh.modified
        || previous.entries != fresh.entries
    {
        bail!("压缩包在打开后发生了变化，请重新打开后再解压");
    }
    Ok(())
}

pub(super) fn validate_edit_entries(info: &ArchiveInfo) -> Result<()> {
    if info.entries.is_empty() {
        return Ok(());
    }
    let plan = plan_entries(info, None, &JobControl::default())?;
    let kinds = plan
        .iter()
        .map(|item| (collision_key(&item.relative), item.entry.is_directory))
        .collect::<HashMap<_, _>>();
    for item in &plan {
        for parent in item.relative.ancestors().skip(1) {
            if kinds.get(&collision_key(parent)) == Some(&false) {
                bail!("压缩包包含文件与文件夹冲突：{}", item.entry.name);
            }
        }
    }
    Ok(())
}

fn plan_entries(
    info: &ArchiveInfo,
    selected: Option<&BTreeSet<usize>>,
    control: &JobControl,
) -> Result<Vec<PlannedEntry>> {
    if let Some(selected) = selected {
        if selected.is_empty() {
            bail!("请先勾选要解压的条目");
        }
        if selected.iter().any(|&index| index >= info.entries.len()) {
            bail!("选中的压缩包条目已失效，请重新打开压缩包");
        }
    }

    let mut all = Vec::with_capacity(info.entries.len());
    let mut names = HashSet::new();
    for entry in &info.entries {
        if entry.unsupported_kind && !info.skips_iso_symlink(entry) {
            bail!("不支持解压符号链接或特殊条目：{}", entry.name);
        }
        let relative = safe_relative_path(&entry.name)?;
        if !names.insert(collision_key(&relative)) {
            bail!("压缩包包含重复或冲突的路径：{}", entry.name);
        }
        all.push(PlannedEntry {
            entry: entry.clone(),
            relative,
        });
    }

    let selected_directories: HashSet<_> = all
        .iter()
        .filter(|item| {
            item.entry.is_directory
                && selected.is_some_and(|indices| indices.contains(&item.entry.index))
        })
        .map(|item| collision_key(&item.relative))
        .collect();
    let selected_entries = all.into_iter().filter(|item| {
        selected.is_none_or(|indices| indices.contains(&item.entry.index))
            || item
                .relative
                .ancestors()
                .skip(1)
                .any(|ancestor| selected_directories.contains(&collision_key(ancestor)))
    });
    let mut plan = Vec::new();
    for item in selected_entries {
        if info.skips_iso_symlink(&item.entry) {
            control.warn(format!("已跳过 ISO 符号链接：{}", item.entry.name));
        } else {
            plan.push(item);
        }
    }
    if plan.is_empty() {
        if control.warnings().is_empty() {
            bail!("没有可解压的条目");
        }
        bail!("没有可解压的条目；{}", control.warnings().join("；"));
    }
    Ok(plan)
}

fn prepare_destination(
    info: &ArchiveInfo,
    options: &ExtractOptions,
    plan: &[PlannedEntry],
) -> Result<PathBuf> {
    let root = ensure_root(&options.destination)?;
    check_destination(info, options, plan, &root)?;
    Ok(root)
}

fn check_destination(
    info: &ArchiveInfo,
    options: &ExtractOptions,
    plan: &[PlannedEntry],
    root: &Path,
) -> Result<()> {
    let archive_path = fs::canonicalize(&info.path).context("无法检查压缩包路径")?;
    for item in plan {
        let exists = check_target(
            root,
            &item.relative,
            item.entry.is_directory,
            options.overwrite,
        )?;
        let target = root.join(&item.relative);
        if exists && fs::canonicalize(&target).context("无法检查目标文件")? == archive_path
        {
            bail!("不能用解压结果覆盖正在读取的压缩包");
        }
    }
    Ok(())
}

fn total_selected(plan: &[PlannedEntry]) -> Result<u64> {
    plan.iter().try_fold(0u64, |sum, item| {
        sum.checked_add(item.entry.size).context("所选文件大小溢出")
    })
}

fn prepare_directories(root: &Path, plan: &[PlannedEntry], control: &JobControl) -> Result<()> {
    let mut directories = BTreeSet::new();
    for item in plan {
        control.check_cancelled()?;
        let directory = if item.entry.is_directory {
            item.relative.as_path()
        } else {
            item.relative.parent().unwrap_or_else(|| Path::new(""))
        };
        directories.insert(directory);
    }
    for directory in directories {
        control.check_cancelled()?;
        ensure_directory(root, directory)?;
    }
    Ok(())
}

fn parallel_jobs<R, T, F>(
    reader: &R,
    jobs: &[T],
    workers: usize,
    control: &JobControl,
    extract: F,
) -> Result<()>
where
    R: Clone + Send,
    T: Sync,
    F: Fn(&mut R, &T) -> Result<()> + Sync,
{
    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    std::thread::scope(|scope| -> Result<()> {
        let mut handles = Vec::new();
        for _ in 0..workers {
            let next = &next;
            let failed = &failed;
            let extract = &extract;
            let mut reader = reader.clone();
            let handle = std::thread::Builder::new()
                .name("hx6zip-extract".to_owned())
                .spawn_scoped(scope, move || {
                    let result = (|| -> Result<()> {
                        while !failed.load(Ordering::Relaxed) {
                            control.check_cancelled()?;
                            let index = next.fetch_add(1, Ordering::Relaxed);
                            let Some(job) = jobs.get(index) else { break };
                            extract(&mut reader, job)?;
                        }
                        Ok(())
                    })();
                    if result.is_err() {
                        failed.store(true, Ordering::Relaxed);
                    }
                    result
                });
            match handle {
                Ok(handle) => handles.push(handle),
                Err(error) => {
                    failed.store(true, Ordering::Relaxed);
                    return Err(error).context("无法启动解压线程");
                }
            }
        }
        let mut error = None;
        for handle in handles {
            let result = handle
                .join()
                .unwrap_or_else(|_| Err(anyhow::anyhow!("解压线程异常退出")));
            if error.is_none() {
                error = result.err();
            }
        }
        if let Some(error) = error {
            return Err(error);
        }
        Ok(())
    })
}

fn parallel_files<'a, R, F>(
    reader: &R,
    files: &[&PlannedEntry],
    workers: usize,
    control: &JobControl,
    tracker: &mut Tracker<'a>,
    extract: F,
) -> Result<()>
where
    R: Clone + Send,
    F: Fn(&mut R, &PlannedEntry, &Mutex<&mut Tracker<'a>>) -> Result<()> + Sync,
{
    let tracker = Mutex::new(tracker);
    parallel_jobs(reader, files, workers, control, |reader, item| {
        tracker
            .lock()
            .map_err(|_| anyhow::anyhow!("解压进度状态异常"))?
            .start_file(&item.entry.name)?;
        extract(reader, item, &tracker)
    })
}

const STREAM_WRITE_MAX: u64 = 1024 * 1024;

struct FileWriters<'a> {
    sender: SyncSender<(PlannedEntry, Vec<u8>)>,
    failed: &'a AtomicBool,
    control: &'a JobControl,
}

struct WriteWorkerStatus<'a> {
    failed: &'a AtomicBool,
    succeeded: bool,
}

impl Drop for WriteWorkerStatus<'_> {
    fn drop(&mut self) {
        if !self.succeeded {
            self.failed.store(true, Ordering::Release);
        }
    }
}

impl FileWriters<'_> {
    fn submit(&self, item: &PlannedEntry, data: Vec<u8>) -> Result<()> {
        let mut job = (item.clone(), data);
        loop {
            self.control.check_cancelled()?;
            if self.failed.load(Ordering::Acquire) {
                bail!("解压写入线程失败");
            }
            match self.sender.try_send(job) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Full(returned)) => {
                    job = returned;
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                Err(TrySendError::Disconnected(_)) => bail!("解压写入线程已退出"),
            }
        }
    }
}

fn with_file_writers(
    root: &Path,
    overwrite: bool,
    control: &JobControl,
    enabled: bool,
    decode: impl FnOnce(Option<&FileWriters<'_>>) -> Result<()>,
) -> Result<()> {
    let workers = std::thread::available_parallelism()
        .context("无法获取可用处理器数量")?
        .get()
        .min(4);
    if !enabled || workers < 2 {
        return decode(None);
    }
    // The queue and worker buffers hold at most 12 MiB of decoded file data.
    let (sender, receiver) = sync_channel::<(PlannedEntry, Vec<u8>)>(workers * 2);
    let receiver = Mutex::new(receiver);
    let failed = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for _ in 0..workers {
            let receiver = &receiver;
            let failed = &failed;
            handles.push(
                std::thread::Builder::new()
                    .name("hx6zip-extract-write".into())
                    .spawn_scoped(scope, move || {
                        let mut status = WriteWorkerStatus {
                            failed,
                            succeeded: false,
                        };
                        let result = (|| -> Result<()> {
                            loop {
                                let job = receiver
                                    .lock()
                                    .map_err(|_| anyhow::anyhow!("解压写入队列状态异常"))?
                                    .recv();
                                let Ok((item, data)) = job else { break };
                                if failed.load(Ordering::Acquire) {
                                    break;
                                }
                                write_temporary(root, &item, overwrite, control, |output| {
                                    copy_stream(
                                        &mut io::Cursor::new(data),
                                        output,
                                        item.entry.size,
                                        control,
                                        |_| Ok(()),
                                    )
                                })
                                .with_context(|| format!("无法解压 {}", item.entry.name))?;
                            }
                            Ok(())
                        })();
                        status.succeeded = result.is_ok();
                        result
                    })
                    .context("无法启动解压写入线程")?,
            );
        }
        let writers = FileWriters {
            sender,
            failed: &failed,
            control,
        };
        let decoded = decode(Some(&writers));
        drop(writers);
        let mut write_error = None;
        for handle in handles {
            let result = handle
                .join()
                .unwrap_or_else(|_| Err(anyhow::anyhow!("解压写入线程异常退出")));
            if write_error.is_none() {
                write_error = result.err();
            }
        }
        if let Some(error) = write_error {
            return Err(error);
        }
        decoded
    })
}

fn extract_zip<R: Read + Seek + Clone + Send>(
    reader: &mut ZipArchive<R>,
    input: &SharedInput,
    root: &Path,
    options: &ExtractOptions,
    plan: &[PlannedEntry],
    control: &JobControl,
) -> Result<()> {
    let mut tracker = Tracker::new(control, "正在解压", total_selected(plan)?);
    let mut state = ZipExtraction::new(reader.clone(), input.clone());
    let files = plan
        .iter()
        .filter(|item| !item.entry.is_directory)
        .collect::<Vec<_>>();
    let workers = std::thread::available_parallelism()
        .context("无法获取可用处理器数量")?
        .get()
        .min(8)
        .min(files.len());
    if workers > 1 {
        prepare_directories(root, plan, control)?;
        parallel_files(
            &state,
            &files,
            workers,
            control,
            &mut tracker,
            |state, item, tracker| {
                write_temporary(root, item, options.overwrite, control, |output| {
                    state
                        .copy_file(
                            item,
                            options.password.as_deref(),
                            output,
                            control,
                            |bytes| {
                                tracker
                                    .lock()
                                    .map_err(|_| anyhow::anyhow!("解压进度状态异常"))?
                                    .advance(bytes)
                            },
                        )
                        .with_context(|| format!("无法解压 {}", item.entry.name))
                })
            },
        )?;
        restore_directory_times(root, plan, control)?;
        return tracker.finish();
    }
    for item in plan {
        tracker.start_file(&item.entry.name)?;
        if item.entry.is_directory {
            ensure_directory(root, &item.relative)?;
            continue;
        }
        write_temporary(root, item, options.overwrite, control, |output| {
            state
                .copy_file(
                    item,
                    options.password.as_deref(),
                    output,
                    control,
                    |bytes| tracker.advance(bytes),
                )
                .with_context(|| format!("无法解压 {}", item.entry.name))
        })?;
    }
    restore_directory_times(root, plan, control)?;
    tracker.finish()
}

struct ZipExtraction<R> {
    archive: ZipArchive<R>,
    input: SharedInput,
    decoder: Option<libdeflater::Decompressor>,
    compressed: Vec<u8>,
    decoded: Vec<u8>,
}

impl<R> ZipExtraction<R> {
    fn new(archive: ZipArchive<R>, input: SharedInput) -> Self {
        Self {
            archive,
            input,
            decoder: None,
            compressed: Vec::new(),
            decoded: Vec::new(),
        }
    }
}

impl<R: Clone> Clone for ZipExtraction<R> {
    fn clone(&self) -> Self {
        Self::new(self.archive.clone(), self.input.clone())
    }
}

impl<R: Read + Seek> ZipExtraction<R> {
    fn copy_file(
        &mut self,
        item: &PlannedEntry,
        password: Option<&str>,
        output: &mut File,
        control: &JobControl,
        advance: impl FnMut(usize) -> Result<()>,
    ) -> Result<()> {
        // Bound both buffers: at most 64 MiB per worker, without mapping mutable files.
        const MAX_BUFFER: u64 = 32 * 1024 * 1024;
        let data_start = {
            let metadata = self
                .archive
                .file_metadata(item.entry.index)
                .with_context(|| format!("无法读取 {}", item.entry.name))?;
            if !metadata.encrypted
                && metadata.compression_method == CompressionMethod::Deflated
                && metadata.uncompressed_size <= MAX_BUFFER
                && metadata.compressed_size <= MAX_BUFFER
            {
                let mut cursor = self.input.direct_cursor();
                Some(
                    metadata
                        .data_start(&mut cursor)
                        .with_context(|| format!("无法读取 ZIP 条目头：{}", item.entry.name))?,
                )
            } else {
                None
            }
        };
        if let Some(data_start) = data_start {
            let metadata = self.archive.file_metadata(item.entry.index)?;
            let expected = metadata.uncompressed_size as usize;
            let compressed_size = metadata.compressed_size;
            let crc = metadata.crc32;
            self.compressed.clear();
            self.compressed.reserve_exact(compressed_size as usize);
            self.compressed.resize(compressed_size as usize, 0);
            for (index, chunk) in self.compressed.chunks_mut(1024 * 1024).enumerate() {
                control.check_cancelled()?;
                let offset = index as u64 * 1024 * 1024;
                let position = data_start.checked_add(offset).context("ZIP 条目偏移溢出")?;
                self.input
                    .read_exact_at(position, chunk)
                    .with_context(|| format!("无法读取 {}", item.entry.name))?;
            }
            self.decoded
                .reserve_exact(expected.saturating_sub(self.decoded.len()));
            self.decoded.resize(expected, 0);
            control.check_cancelled()?;
            let size = self
                .decoder
                .get_or_insert_with(libdeflater::Decompressor::new)
                .deflate_decompress(&self.compressed, &mut self.decoded)
                .context("ZIP 解压数据失败")?;
            anyhow::ensure!(size == expected, "文件内容大小与预期不符");
            anyhow::ensure!(crc32fast::hash(&self.decoded) == crc, "ZIP CRC 校验失败");
            let mut advance = advance;
            for chunk in self.decoded.chunks(1024 * 1024) {
                control.check_cancelled()?;
                output.write_all(chunk).context("写入解压文件失败")?;
                advance(chunk.len())?;
            }
            return Ok(());
        }
        let mut input = match password.filter(|p| !p.is_empty()) {
            Some(password) => self
                .archive
                .by_index_decrypt(item.entry.index, password.as_bytes()),
            None => self.archive.by_index(item.entry.index),
        }
        .with_context(|| format!("无法读取 {}，密码可能不正确", item.entry.name))?;
        copy_stream(&mut input, output, item.entry.size, control, advance)
    }
}

fn extract_iso(
    mut reader: SharedInput,
    extents: &[Vec<IsoExtent>],
    root: &Path,
    options: &ExtractOptions,
    plan: &[PlannedEntry],
    control: &JobControl,
) -> Result<()> {
    let mut tracker = Tracker::new(control, "正在解压", total_selected(plan)?);
    let files = plan
        .iter()
        .filter(|item| !item.entry.is_directory)
        .collect::<Vec<_>>();
    let workers = std::thread::available_parallelism()
        .context("无法获取可用处理器数量")?
        .get()
        .min(8)
        .min(files.len());
    if workers > 1 {
        prepare_directories(root, plan, control)?;
        parallel_files(
            &reader,
            &files,
            workers,
            control,
            &mut tracker,
            |reader, item, tracker| {
                write_temporary(root, item, options.overwrite, control, |output| {
                    copy_iso_extents(
                        reader,
                        &extents[item.entry.index],
                        output,
                        control,
                        |bytes| {
                            tracker
                                .lock()
                                .map_err(|_| anyhow::anyhow!("解压进度状态异常"))?
                                .advance(bytes)
                        },
                    )
                })
            },
        )?;
    } else {
        for item in plan {
            tracker.start_file(&item.entry.name)?;
            if item.entry.is_directory {
                ensure_directory(root, &item.relative)?;
            } else {
                write_temporary(root, item, options.overwrite, control, |output| {
                    copy_iso_extents(
                        &mut reader,
                        &extents[item.entry.index],
                        output,
                        control,
                        |bytes| tracker.advance(bytes),
                    )
                })?;
            }
        }
    }
    restore_directory_times(root, plan, control)?;
    tracker.finish()
}

fn copy_iso_extents(
    reader: &mut SharedInput,
    extents: &[IsoExtent],
    output: &mut File,
    control: &JobControl,
    mut advance: impl FnMut(usize) -> Result<()>,
) -> Result<()> {
    for extent in extents {
        control.check_cancelled()?;
        reader
            .seek(SeekFrom::Start(extent.offset))
            .context("无法定位 ISO 文件数据")?;
        copy_stream(
            &mut reader.take(extent.length),
            output,
            extent.length,
            control,
            &mut advance,
        )?;
    }
    Ok(())
}

const SEVENZIP_PARALLEL_MEMORY_BUDGET: u64 = 512 * 1024 * 1024;
const SEVENZIP_MAX_PARALLEL_BLOCK_MEMORY: u64 = 256 * 1024 * 1024;

fn sevenzip_block_memory(block: &Block) -> Option<u64> {
    let mut memory = 8 * 1024 * 1024u64;
    for coder in &block.coders {
        memory = memory.checked_add(sevenzip_coder_memory(
            coder.encoder_method_id(),
            coder.properties(),
        )?)?;
    }
    (memory <= SEVENZIP_MAX_PARALLEL_BLOCK_MEMORY).then_some(memory)
}

fn sevenzip_coder_memory(method: &[u8], properties: &[u8]) -> Option<u64> {
    Some(match method {
        EncoderMethod::ID_LZMA2 => {
            let bits = u32::from(*properties.first()?);
            if bits >= 40 {
                return None;
            }
            (2 | u64::from(bits & 1)) << (bits / 2 + 11)
        }
        EncoderMethod::ID_LZMA | EncoderMethod::ID_PPMD => {
            u64::from(u32::from_le_bytes(properties.get(1..5)?.try_into().ok()?)).max(4096)
        }
        EncoderMethod::ID_BZIP2 => 16 * 1024 * 1024,
        EncoderMethod::ID_DEFLATE => 4 * 1024 * 1024,
        EncoderMethod::ID_COPY
        | EncoderMethod::ID_AES256_SHA256
        | EncoderMethod::ID_DELTA
        | EncoderMethod::ID_BCJ2
        | EncoderMethod::ID_BCJ_X86
        | EncoderMethod::ID_BCJ_ARM
        | EncoderMethod::ID_BCJ_ARM64
        | EncoderMethod::ID_BCJ_ARM_THUMB
        | EncoderMethod::ID_BCJ_PPC
        | EncoderMethod::ID_BCJ_IA64
        | EncoderMethod::ID_BCJ_SPARC
        | EncoderMethod::ID_BCJ_RISCV => 1024 * 1024,
        _ => return None,
    })
}

fn sevenzip_parallel_workers(archive: &Archive, available_threads: usize) -> usize {
    let memory = archive
        .blocks
        .iter()
        .map(sevenzip_block_memory)
        .try_fold(0u64, |maximum, memory| Some(maximum.max(memory?)));
    let Some(memory) = memory.filter(|memory| *memory > 0) else {
        return 1;
    };
    available_threads
        .min(archive.blocks.len())
        .min(8)
        .min((SEVENZIP_PARALLEL_MEMORY_BUDGET / memory) as usize)
        .max(1)
}

fn extract_sevenzip_parallel<R: Read + Seek + Clone + Send>(
    input: R,
    archive: &Archive,
    password: &Password,
    root: &Path,
    options: &ExtractOptions,
    plan: &[PlannedEntry],
    control: &JobControl,
    workers: usize,
) -> Result<()> {
    let mut tracker = Tracker::new(control, "正在解压", total_selected(plan)?);
    prepare_directories(root, plan, control)?;
    let by_index: HashMap<_, _> = plan.iter().map(|item| (item.entry.index, item)).collect();
    let mut last_selected = BTreeMap::<usize, usize>::new();
    for item in plan {
        let index = item.entry.index;
        if archive.files[index].has_stream {
            let block =
                archive.stream_map.file_block_index[index].context("7z 条目缺少数据块索引")?;
            last_selected
                .entry(block)
                .and_modify(|last| *last = (*last).max(index))
                .or_insert(index);
        }
    }
    let blocks = last_selected.keys().copied().collect::<Vec<_>>();
    let workers = workers.min(blocks.len()).max(1);
    let decoder_threads = if workers == 1 {
        std::thread::available_parallelism()
            .context("无法获取可用处理器数量")?
            .get() as u32
    } else {
        1
    };
    let tracker = Mutex::new(&mut tracker);
    with_file_writers(
        root,
        options.overwrite,
        control,
        workers == 1 && plan.len() >= 16,
        |writers| {
            parallel_jobs(&input, &blocks, workers, control, |source, &block_index| {
                let threads = if archive.blocks[block_index].get_unpack_size()
                    < SEVENZIP_PARALLEL_THRESHOLD
                {
                    1
                } else {
                    decoder_threads
                };
                let decoder = BlockDecoder::new(threads, block_index, archive, password, source);
                let mut file_index = archive.stream_map.block_first_file_index[block_index];
                let mut inner_error = None;
                let result = decoder.for_each_entries(&mut |entry, input| {
                    let index = file_index;
                    file_index += 1;
                    let extracted = (|| -> Result<()> {
                        control.check_cancelled()?;
                        if !entry.has_stream {
                            return Ok(());
                        }
                        if let Some(item) =
                            by_index.get(&index).filter(|item| !item.entry.is_directory)
                        {
                            tracker
                                .lock()
                                .map_err(|_| anyhow::anyhow!("解压进度状态异常"))?
                                .start_file(&item.entry.name)?;
                            let mut advance = |bytes| {
                                tracker
                                    .lock()
                                    .map_err(|_| anyhow::anyhow!("解压进度状态异常"))?
                                    .advance(bytes)
                            };
                            if let Some(writers) =
                                writers.filter(|_| item.entry.size <= STREAM_WRITE_MAX)
                            {
                                let mut data = Vec::with_capacity(item.entry.size as usize);
                                copy_stream(
                                    input,
                                    &mut data,
                                    item.entry.size,
                                    control,
                                    &mut advance,
                                )
                                .with_context(|| format!("无法解压 {}", item.entry.name))?;
                                writers.submit(item, data)?;
                            } else {
                                write_temporary(
                                    root,
                                    item,
                                    options.overwrite,
                                    control,
                                    |output| {
                                        copy_stream(
                                            input,
                                            output,
                                            item.entry.size,
                                            control,
                                            &mut advance,
                                        )
                                        .with_context(|| format!("无法解压 {}", item.entry.name))
                                    },
                                )?;
                            }
                        } else {
                            copy_stream(input, &mut io::sink(), entry.size, control, |_| Ok(()))?;
                        }
                        Ok(())
                    })();
                    match extracted {
                        Ok(()) => Ok(index < last_selected[&block_index]),
                        Err(error) => {
                            inner_error = Some(error);
                            Err(sevenz_rust2::Error::Other("解压失败".into()))
                        }
                    }
                });
                if let Some(error) = inner_error {
                    return Err(error);
                }
                result.context("解压 7z 失败，密码可能不正确或文件已损坏")?;
                Ok(())
            })
        },
    )?;
    let tracker = tracker
        .into_inner()
        .map_err(|_| anyhow::anyhow!("解压进度状态异常"))?;
    for item in plan {
        if !item.entry.is_directory && !archive.files[item.entry.index].has_stream {
            tracker.start_file(&item.entry.name)?;
            extract_file(
                &mut io::empty(),
                root,
                item,
                options.overwrite,
                control,
                tracker,
            )?;
        }
    }
    restore_directory_times(root, plan, control)?;
    tracker.finish()
}

fn extract_other(
    mut file: File,
    info: &ArchiveInfo,
    root: &Path,
    options: &ExtractOptions,
    plan: &[PlannedEntry],
    control: &JobControl,
) -> Result<()> {
    let mut tracker = Tracker::new(control, "正在解压", total_selected(plan)?);
    match info.format {
        ArchiveKind::Cab => {
            let mut cabinet = cab::Cabinet::new(file).context("无法读取 CAB 压缩包")?;
            for item in plan {
                tracker.start_file(&item.entry.name)?;
                let mut reader = cabinet
                    .read_file(&item.entry.name)
                    .with_context(|| format!("无法读取 CAB 文件 {}", item.entry.name))?;
                extract_file(
                    &mut reader,
                    root,
                    item,
                    options.overwrite,
                    control,
                    &mut tracker,
                )?;
            }
        }
        ArchiveKind::Rar => {
            let mut reader =
                rar::Reader::open(&info.path, options.password.as_deref(), control, true)?;
            let by_index: HashMap<_, _> =
                plan.iter().map(|item| (item.entry.index, item)).collect();
            let mut index = 0usize;
            while let Some(entry) = reader.next(index)? {
                if info.entries.get(index) != Some(&entry) {
                    bail!("压缩包在打开后发生了变化，请重新打开后再解压");
                }
                if let Some(item) = by_index.get(&index) {
                    tracker.start_file(&item.entry.name)?;
                    if item.entry.is_directory {
                        ensure_directory(root, &item.relative)?;
                        reader.skip()?;
                    } else {
                        write_temporary(root, item, options.overwrite, control, |output| {
                            let mut output = BufWriter::with_capacity(512 * 1024, output);
                            let mut writer = ProgressWriter {
                                output: &mut output,
                                control,
                                tracker: &mut tracker,
                            };
                            reader.process(item.entry.size, Some(&mut writer))?;
                            writer.flush().context("无法写入 RAR 解压内容")
                        })?;
                    }
                } else if reader.is_solid() && !entry.is_directory {
                    reader.process(entry.size, None)?;
                } else {
                    reader.skip()?;
                }
                index += 1;
            }
            if index != info.entries.len() {
                bail!("压缩包在打开后发生了变化，请重新打开后再解压");
            }
        }
        ArchiveKind::Tar(compression) => {
            prepare_directories(root, plan, control)?;
            let by_index: HashMap<_, _> =
                plan.iter().map(|item| (item.entry.index, item)).collect();
            let mut count = 0usize;
            with_file_writers(
                root,
                options.overwrite,
                control,
                plan.len() >= 16,
                |writers| {
                    read_tar_entries(&mut file, compression, control, |entry, input| {
                        if info.entries.get(entry.index) != Some(entry) {
                            bail!("压缩包在打开后发生了变化，请重新打开后再解压");
                        }
                        count += 1;
                        if let Some(item) = by_index.get(&entry.index) {
                            tracker.start_file(&entry.name)?;
                            if entry.is_directory {
                                ensure_directory(root, &item.relative)?;
                            } else if let Some(writers) =
                                writers.filter(|_| item.entry.size <= STREAM_WRITE_MAX)
                            {
                                let mut data = Vec::with_capacity(item.entry.size as usize);
                                copy_stream(input, &mut data, item.entry.size, control, |bytes| {
                                    tracker.advance(bytes)
                                })
                                .with_context(|| format!("无法解压 {}", item.entry.name))?;
                                writers.submit(item, data)?;
                            } else {
                                extract_file(
                                    input,
                                    root,
                                    item,
                                    options.overwrite,
                                    control,
                                    &mut tracker,
                                )?;
                            }
                        }
                        Ok(())
                    })
                },
            )?;
            if count != info.entries.len() {
                bail!("压缩包在打开后发生了变化，请重新打开后再解压");
            }
        }
        ArchiveKind::Stream(compression) => {
            let item = &plan[0];
            tracker.start_file(&item.entry.name)?;
            let mut reader = open_stream(file, compression, control)?;
            extract_file(
                &mut reader,
                root,
                item,
                options.overwrite,
                control,
                &mut tracker,
            )?;
        }
        ArchiveKind::Zip | ArchiveKind::SevenZip | ArchiveKind::Iso => {
            unreachable!("ZIP, 7z and ISO use their own extractors")
        }
    }
    restore_directory_times(root, plan, control)?;
    tracker.finish()
}

struct ProgressWriter<'a, 'b> {
    output: &'a mut dyn Write,
    control: &'a JobControl,
    tracker: &'a mut Tracker<'b>,
}

impl Write for ProgressWriter<'_, '_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.control
            .check_cancelled()
            .map_err(|_| io::Error::other(Cancelled))?;
        let count = self.output.write(buffer)?;
        self.tracker
            .advance(count)
            .map_err(|error| io::Error::other(error.to_string()))?;
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}

fn extract_file(
    reader: &mut dyn io::Read,
    root: &Path,
    item: &PlannedEntry,
    overwrite: bool,
    control: &JobControl,
    tracker: &mut Tracker<'_>,
) -> Result<()> {
    write_temporary(root, item, overwrite, control, |output| {
        copy_stream(reader, output, item.entry.size, control, |bytes| {
            tracker.advance(bytes)
        })
        .with_context(|| format!("无法解压 {}", item.entry.name))
    })
}

fn write_temporary(
    root: &Path,
    item: &PlannedEntry,
    overwrite: bool,
    control: &JobControl,
    write: impl FnOnce(&mut File) -> Result<()>,
) -> Result<()> {
    let relative_parent = item.relative.parent().unwrap_or_else(|| Path::new(""));
    let parent = ensure_directory(root, relative_parent)?;
    check_target(root, &item.relative, false, overwrite)?;
    let destination = root.join(&item.relative);
    let mut temporary = NamedTempFile::new_in(&parent).context("无法创建解压临时文件")?;
    let prepared = (|| -> Result<()> {
        write(temporary.as_file_mut())?;
        temporary
            .as_file_mut()
            .flush()
            .context("无法写入解压文件")?;
        set_file_modified(temporary.as_file(), item.entry.modified)?;
        if control.integrity() == IntegrityMode::Safe {
            temporary
                .as_file()
                .sync_all()
                .context("无法保存解压文件数据")?;
        }
        control.check_cancelled()?;
        check_target(root, &item.relative, false, overwrite)?;
        Ok(())
    })();
    if let Err(error) = prepared {
        // Close before unlinking to avoid Windows delete-pending temporary names.
        let (file, path) = temporary.into_parts();
        drop(file);
        if let Err(cleanup) = path.close() {
            bail!("解压失败：{error:#}；无法清理解压临时文件：{cleanup}");
        }
        return Err(error);
    }
    persist_temp(temporary, &destination, overwrite)
}

fn restore_directory_times(root: &Path, plan: &[PlannedEntry], control: &JobControl) -> Result<()> {
    let mut directories = plan
        .iter()
        .filter(|item| item.entry.is_directory && item.entry.modified.is_some())
        .collect::<Vec<_>>();
    directories.sort_by_key(|item| Reverse(item.relative.components().count()));
    for item in directories {
        control.check_cancelled()?;
        check_target(root, &item.relative, true, true)?;
        let target = fs::canonicalize(root.join(&item.relative))
            .with_context(|| format!("无法解析目标文件夹：{}", item.entry.name))?;
        set_directory_modified(&target, item.entry.modified)
            .with_context(|| format!("无法恢复 {} 的修改时间", item.entry.name))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        io::Cursor,
        sync::{Arc, Barrier, mpsc},
        time::{Duration, SystemTime},
    };

    use super::*;
    use crate::archive::CancellationToken;
    use sevenz_rust2::{ArchiveWriter, encoder_options::AesEncoderOptions};
    use tempfile::TempDir;

    #[test]
    fn cloned_shared_inputs_keep_independent_positions_and_unread_buffers() {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(b"abcdef").unwrap();
        let mut original = SharedInput::new(file.reopen().unwrap(), 6);
        let mut byte = [0];
        original.read_exact(&mut byte).unwrap();
        assert_eq!(&byte, b"a");
        let mut cloned = original.clone();
        cloned.read_exact(&mut byte).unwrap();
        assert_eq!(&byte, b"b");
        original.read_exact(&mut byte).unwrap();
        assert_eq!(&byte, b"b");
        cloned.seek(SeekFrom::End(-1)).unwrap();
        cloned.read_exact(&mut byte).unwrap();
        assert_eq!(&byte, b"f");
        original.read_exact(&mut byte).unwrap();
        assert_eq!(&byte, b"c");
        assert!(cloned.seek(SeekFrom::Start(0)).is_ok());
        assert!(cloned.seek(SeekFrom::Current(-1)).is_err());
        cloned.read_exact(&mut byte).unwrap();
        assert_eq!(&byte, b"a");
    }

    #[test]
    fn queued_file_write_errors_are_returned_and_temporaries_are_removed() {
        let temp = TempDir::new().unwrap();
        let item = PlannedEntry {
            entry: ArchiveEntry {
                index: 0,
                name: "bad.bin".into(),
                size: 2,
                compressed_size: None,
                is_directory: false,
                is_encrypted: Some(false),
                unsupported_kind: false,
                is_symbolic_link: false,
                modified: None,
            },
            relative: PathBuf::from("bad.bin"),
        };
        let control = JobControl::default();
        let error = with_file_writers(temp.path(), false, &control, true, |writers| {
            if let Some(writers) = writers {
                for _ in 0..32 {
                    writers.submit(&item, vec![1])?;
                }
                Ok(())
            } else {
                write_temporary(temp.path(), &item, false, &control, |output| {
                    copy_stream(&mut io::Cursor::new([1]), output, 2, &control, |_| Ok(()))
                })
            }
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("大小与预期不符"), "{error:#}");
        let remaining = fs::read_dir(temp.path())
            .map(|entries| {
                entries
                    .map(|entry| entry.unwrap().path())
                    .collect::<Vec<_>>()
            })
            .unwrap();
        assert!(remaining.is_empty(), "{remaining:?}");
    }

    #[test]
    fn zip_batch_decoder_reuses_bounded_buffers_and_handles_empty_files() {
        let archive = NamedTempFile::new().unwrap();
        let mut writer = zip::ZipWriter::new(archive.reopen().unwrap());
        let sizes = [1, 3 * 1024 * 1024, 5 * 1024 * 1024, 0];
        for (index, size) in sizes.into_iter().enumerate() {
            writer
                .start_file(
                    format!("{index}.bin"),
                    zip::write::SimpleFileOptions::default()
                        .compression_method(CompressionMethod::Deflated),
                )
                .unwrap();
            writer.write_all(&vec![index as u8; size]).unwrap();
        }
        writer.finish().unwrap();
        let control = JobControl::default();
        let info = crate::archive::inspect_archive(archive.path(), None, &control).unwrap();
        let plan = plan_entries(&info, None, &control).unwrap();
        let input = SharedInput::new(
            archive.reopen().unwrap(),
            archive.as_file().metadata().unwrap().len(),
        );
        let mut state = ZipExtraction::new(ZipArchive::new(input.clone()).unwrap(), input);
        for (index, item) in plan.iter().enumerate() {
            let mut output = NamedTempFile::new().unwrap();
            let mut completed = 0;
            state
                .copy_file(item, None, output.as_file_mut(), &control, |count| {
                    completed += count;
                    Ok(())
                })
                .unwrap();
            assert_eq!(completed, sizes[index]);
            assert_eq!(
                fs::read(output.path()).unwrap(),
                vec![index as u8; sizes[index]]
            );
            assert!(state.decoder.is_some());
            assert!(state.compressed.capacity() <= 16 * 1024 * 1024);
            assert!(state.decoded.capacity() <= 16 * 1024 * 1024);
        }
    }

    fn sevenzip_sample(temp: &TempDir, password: Option<&str>, stored: bool) -> PathBuf {
        let path = temp.path().join("blocks.7z");
        let mut writer = ArchiveWriter::new(File::create(&path).unwrap()).unwrap();
        let mut methods = vec![if stored {
            EncoderMethod::COPY.into()
        } else {
            EncoderMethod::LZMA2.into()
        }];
        if let Some(password) = password {
            methods.push(AesEncoderOptions::new(Password::from(password)).into());
        }
        writer.set_content_methods(methods);
        for index in 0..4 {
            let contents = vec![index as u8; 128 * 1024];
            writer
                .push_archive_entry(
                    sevenz_rust2::ArchiveEntry::new_file(&format!("file-{index}.bin")),
                    Some(Cursor::new(contents)),
                )
                .unwrap();
        }
        writer
            .push_archive_entry(
                sevenz_rust2::ArchiveEntry::new_file("empty.txt"),
                None::<Cursor<Vec<u8>>>,
            )
            .unwrap();
        writer
            .push_archive_entry(
                sevenz_rust2::ArchiveEntry::new_directory("empty-dir"),
                None::<Cursor<Vec<u8>>>,
            )
            .unwrap();
        writer.finish().unwrap();
        path
    }

    fn force_parallel_sevenzip(
        path: &Path,
        options: &ExtractOptions,
        control: &JobControl,
    ) -> Result<()> {
        let mut file = File::open(path)?;
        let metadata = file.metadata()?;
        let offset = sevenzip_offset(&mut file, control)?;
        let input = OffsetReader::new(SharedInput::new(file, metadata.len()), offset)?;
        let password = Password::from(options.password.as_deref().unwrap_or_default());
        let reader = ArchiveReader::new(input.clone(), password.clone())?;
        let info = inspect_sevenzip(path, &metadata, &reader, control)?;
        assert_eq!(reader.archive().blocks.len(), 4);
        assert_eq!(sevenzip_parallel_workers(reader.archive(), 2), 2);
        let plan = plan_entries(&info, options.selected.as_ref(), control)?;
        let root = prepare_destination(&info, options, &plan)?;
        extract_sevenzip_parallel(
            input,
            reader.archive(),
            &password,
            &root,
            options,
            &plan,
            control,
            2,
        )
    }

    #[test]
    fn parallel_sevenzip_sfx_uses_archive_relative_offsets() {
        let temp = TempDir::new().unwrap();
        let source = sevenzip_sample(&temp, Some("secret"), false);
        let path = temp.path().join("parallel.exe");
        let mut data = b"MZ".to_vec();
        data.resize(65533, 0);
        data.extend(fs::read(source).unwrap());
        fs::write(&path, data).unwrap();
        let output = temp.path().join("output");
        let options = ExtractOptions {
            destination: output.clone(),
            password: Some("secret".to_owned()),
            selected: None,
            overwrite: false,
        };
        force_parallel_sevenzip(&path, &options, &JobControl::default()).unwrap();
        for index in 0..4 {
            assert_eq!(
                fs::read(output.join(format!("file-{index}.bin"))).unwrap(),
                vec![index as u8; 128 * 1024]
            );
        }
    }

    #[test]
    fn parallel_job_runner_runs_multiple_workers() {
        let barrier = Barrier::new(2);
        let threads = Mutex::new(HashSet::new());
        parallel_jobs(&(), &[0, 1], 2, &JobControl::default(), |_, _| {
            threads.lock().unwrap().insert(std::thread::current().id());
            barrier.wait();
            Ok(())
        })
        .unwrap();
        assert_eq!(threads.into_inner().unwrap().len(), 2);
    }

    #[test]
    fn sevenzip_parallel_budget_rejects_unknown_and_large_codecs() {
        let mut archive = Archive::default();
        archive.blocks = vec![Block::default(); 10];
        assert_eq!(sevenzip_parallel_workers(&archive, 32), 8);
        assert_eq!(sevenzip_parallel_workers(&archive, 1), 1);
        archive.blocks[0]
            .coders
            .push(sevenz_rust2::Coder::default());
        assert_eq!(sevenzip_parallel_workers(&archive, 8), 1);
        assert_eq!(
            sevenzip_coder_memory(EncoderMethod::ID_LZMA2, &[28]),
            Some(64 * 1024 * 1024)
        );
        assert_eq!(sevenzip_coder_memory(EncoderMethod::ID_LZMA2, &[40]), None);
        assert_eq!(sevenzip_coder_memory(EncoderMethod::ID_LZMA2, &[]), None);
        assert_eq!(sevenzip_coder_memory(EncoderMethod::ID_LZMA, &[0; 4]), None);
        assert_eq!(sevenzip_coder_memory(EncoderMethod::ID_ZSTD, &[]), None);
    }

    #[test]
    fn parallel_sevenzip_extracts_selected_and_encrypted_blocks() {
        for password in [None, Some("parallel-password")] {
            let temp = TempDir::new().unwrap();
            let path = sevenzip_sample(&temp, password, false);
            let info =
                super::super::inspect_archive(&path, password, &JobControl::default()).unwrap();
            let selected = info
                .entries
                .iter()
                .filter(|entry| {
                    matches!(
                        entry.name.as_str(),
                        "file-1.bin" | "file-3.bin" | "empty.txt" | "empty-dir"
                    )
                })
                .map(|entry| entry.index)
                .collect();
            let output = temp.path().join("selected");
            let options = ExtractOptions {
                destination: output.clone(),
                password: password.map(str::to_owned),
                selected: Some(selected),
                overwrite: false,
            };
            let reports = Arc::new(Mutex::new(Vec::new()));
            let captured = reports.clone();
            let control = JobControl::new(CancellationToken::default(), move |progress| {
                captured.lock().unwrap().push(progress);
            });
            force_parallel_sevenzip(&path, &options, &control).unwrap();
            assert_eq!(
                fs::read(output.join("file-1.bin")).unwrap(),
                vec![1; 128 * 1024]
            );
            assert_eq!(
                fs::read(output.join("file-3.bin")).unwrap(),
                vec![3; 128 * 1024]
            );
            assert_eq!(fs::read(output.join("empty.txt")).unwrap(), b"");
            assert!(output.join("empty-dir").is_dir());
            assert!(!output.join("file-0.bin").exists());
            let reports = reports.lock().unwrap();
            assert!(
                reports
                    .windows(2)
                    .all(|pair| pair[0].completed_bytes <= pair[1].completed_bytes)
            );
            assert_eq!(reports.last().unwrap().completed_bytes, 256 * 1024);
            if password.is_some() {
                let mut wrong = options.clone();
                wrong.destination = temp.path().join("wrong");
                wrong.password = Some("wrong".to_owned());
                assert!(force_parallel_sevenzip(&path, &wrong, &JobControl::default()).is_err());
                assert!(!wrong.destination.join("file-1.bin").exists());
            }
        }
    }

    #[test]
    fn parallel_sevenzip_cancellation_and_crc_preserve_targets() {
        let temp = TempDir::new().unwrap();
        let path = sevenzip_sample(&temp, None, true);
        let output = temp.path().join("cancel");
        fs::create_dir(&output).unwrap();
        fs::write(output.join("file-0.bin"), b"keep").unwrap();
        let options = ExtractOptions {
            destination: output.clone(),
            password: None,
            selected: None,
            overwrite: true,
        };
        let token = CancellationToken::default();
        let trigger = token.clone();
        let control = JobControl::new(token, move |_| trigger.cancel());
        let error = force_parallel_sevenzip(&path, &options, &control).unwrap_err();
        assert!(error.downcast_ref::<Cancelled>().is_some(), "{error:#}");
        assert_eq!(fs::read(output.join("file-0.bin")).unwrap(), b"keep");
        assert!(fs::read_dir(&output).unwrap().all(|entry| {
            let name = entry.unwrap().file_name();
            name == "file-0.bin" || name == "empty-dir"
        }));

        let reader = ArchiveReader::open(&path, Password::empty()).unwrap();
        let archive = reader.archive();
        let pack = archive.stream_map.block_first_pack_stream_index()[0];
        let offset = 32 + archive.pack_pos() + archive.stream_map.pack_stream_offsets()[pack];
        drop(reader);
        let mut file = fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(offset)).unwrap();
        file.write_all(b"X").unwrap();
        drop(file);
        let output = temp.path().join("crc");
        fs::create_dir(&output).unwrap();
        fs::write(output.join("file-0.bin"), b"keep").unwrap();
        let options = ExtractOptions {
            destination: output.clone(),
            ..options
        };
        assert!(force_parallel_sevenzip(&path, &options, &JobControl::default()).is_err());
        assert_eq!(fs::read(output.join("file-0.bin")).unwrap(), b"keep");
        assert!(fs::read_dir(&output).unwrap().all(|entry| {
            matches!(
                entry.unwrap().file_name().to_str().unwrap(),
                "file-0.bin"
                    | "file-1.bin"
                    | "file-2.bin"
                    | "file-3.bin"
                    | "empty.txt"
                    | "empty-dir"
            )
        }));
    }

    #[test]
    fn corrupt_last_sevenzip_block_never_publishes_other_blocks() {
        let temp = TempDir::new().unwrap();
        let path = sevenzip_sample(&temp, None, true);
        let reader = ArchiveReader::open(&path, Password::empty()).unwrap();
        let archive = reader.archive();
        let pack = archive.stream_map.block_first_pack_stream_index()[3];
        let offset = 32 + archive.pack_pos() + archive.stream_map.pack_stream_offsets()[pack];
        drop(reader);
        let mut file = fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(offset)).unwrap();
        file.write_all(b"X").unwrap();
        drop(file);
        let info = super::super::inspect_archive(&path, None, &JobControl::default()).unwrap();
        for existing in [false, true] {
            let output = temp.path().join(if existing { "existing" } else { "new" });
            if existing {
                fs::create_dir(&output).unwrap();
                fs::write(output.join("file-0.bin"), b"original").unwrap();
            }
            assert!(
                extract_archive(
                    &info,
                    &ExtractOptions {
                        destination: output.clone(),
                        password: None,
                        selected: None,
                        overwrite: existing,
                    },
                    &JobControl::default()
                )
                .is_err()
            );
            if existing {
                assert_eq!(fs::read(output.join("file-0.bin")).unwrap(), b"original");
                assert_eq!(fs::read_dir(&output).unwrap().count(), 1);
            } else {
                assert!(!output.exists());
            }
        }
    }

    #[test]
    fn failed_publication_after_moving_backup_restores_all_originals() {
        let temp = TempDir::new().unwrap();
        let path = sevenzip_sample(&temp, None, true);
        let info = super::super::inspect_archive(&path, None, &JobControl::default()).unwrap();
        let plan =
            plan_entries(&info, Some(&BTreeSet::from([0, 1])), &JobControl::default()).unwrap();
        let output = temp.path().join("output");
        fs::create_dir(&output).unwrap();
        for item in &plan {
            fs::write(output.join(&item.relative), b"original").unwrap();
        }
        let mut transaction = ExtractionTransaction::new(&output).unwrap();
        let stage = transaction.work.path().join("data");
        fs::create_dir(&stage).unwrap();
        fs::write(stage.join(&plan[0].relative), b"replacement").unwrap();
        transaction
            .publish(&stage, &plan[0], true, &JobControl::default())
            .unwrap();
        let error = transaction
            .publish(&stage, &plan[1], true, &JobControl::default())
            .unwrap_err();
        assert!(!output.join(&plan[1].relative).exists());
        assert!(transaction.finish(Err(error)).is_err());
        for item in &plan {
            assert_eq!(fs::read(output.join(&item.relative)).unwrap(), b"original");
        }
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 2);
    }

    #[cfg(windows)]
    #[test]
    fn rollback_failure_keeps_backups_and_is_not_reported_as_cancellation() {
        use std::os::windows::fs::OpenOptionsExt;
        let temp = TempDir::new().unwrap();
        let path = sevenzip_sample(&temp, None, true);
        let info = super::super::inspect_archive(&path, None, &JobControl::default()).unwrap();
        let output = temp.path().join("output");
        fs::create_dir(&output).unwrap();
        fs::write(output.join("file-0.bin"), b"original").unwrap();
        let target = output.join("file-0.bin");
        let held = Arc::new(Mutex::new(None));
        let lock = held.clone();
        let token = CancellationToken::default();
        let cancel = token.clone();
        let control = JobControl::new(token, move |progress| {
            if progress.operation == "正在提交解压结果" && progress.current_file == "file-1.bin"
            {
                *lock.lock().unwrap() = Some(
                    fs::OpenOptions::new()
                        .read(true)
                        .share_mode(1)
                        .open(&target)
                        .unwrap(),
                );
                cancel.cancel();
            }
        });
        let error = extract_archive(
            &info,
            &ExtractOptions {
                destination: output.clone(),
                password: None,
                selected: None,
                overwrite: true,
            },
            &control,
        )
        .unwrap_err();
        assert!(held.lock().unwrap().take().is_some());
        assert!(error.downcast_ref::<Cancelled>().is_none());
        assert!(
            format!("{error:#}").contains("回滚或清理未完成"),
            "{error:#}"
        );
        let recovery = fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".hx6zip-extract-")
            })
            .unwrap();
        let backup = recovery.join("backup").join("file-0.bin");
        assert_eq!(fs::read(&backup).unwrap(), b"original");
        fs::remove_file(output.join("file-0.bin")).unwrap();
        fs::rename(backup, output.join("file-0.bin")).unwrap();
    }

    #[test]
    fn parallel_sevenzip_can_resume_or_cancel_while_paused() {
        let temp = TempDir::new().unwrap();
        let path = sevenzip_sample(&temp, None, false);
        for cancel in [false, true] {
            let token = CancellationToken::default();
            let trigger = token.clone();
            let (paused_sender, paused_receiver) = mpsc::channel();
            let once = AtomicBool::new(false);
            let control = JobControl::new(token.clone(), move |_| {
                if !once.swap(true, Ordering::SeqCst) {
                    trigger.set_paused(true);
                    paused_sender.send(()).unwrap();
                }
            });
            let output = temp
                .path()
                .join(if cancel { "paused-cancel" } else { "resume" });
            let options = ExtractOptions {
                destination: output.clone(),
                password: None,
                selected: None,
                overwrite: false,
            };
            let path = path.clone();
            let (done_sender, done_receiver) = mpsc::channel();
            let worker = std::thread::spawn(move || {
                done_sender
                    .send(force_parallel_sevenzip(&path, &options, &control))
                    .unwrap();
            });
            if paused_receiver
                .recv_timeout(Duration::from_secs(10))
                .is_err()
            {
                token.cancel();
                worker.join().unwrap();
                panic!("parallel extraction did not report progress");
            }
            assert!(done_receiver.try_recv().is_err());
            if cancel {
                token.cancel();
            } else {
                token.set_paused(false);
            }
            let result = done_receiver.recv_timeout(Duration::from_secs(10));
            if result.is_err() {
                token.cancel();
            }
            worker.join().unwrap();
            let result = result.unwrap();
            if cancel {
                assert!(result.unwrap_err().downcast_ref::<Cancelled>().is_some());
                assert!(
                    fs::read_dir(output)
                        .unwrap()
                        .all(|entry| { entry.unwrap().file_name() == "empty-dir" })
                );
            } else {
                result.unwrap();
                assert_eq!(
                    fs::read(output.join("file-0.bin")).unwrap().len(),
                    128 * 1024
                );
            }
        }
    }

    #[test]
    fn extraction_plan_accepts_sizes_above_100_gib() {
        const PREVIOUS_LIMIT: u64 = 100 * 1024 * 1024 * 1024;
        let mut info = ArchiveInfo {
            path: PathBuf::from("sample.zip"),
            format: ArchiveKind::Zip,
            entries: vec![
                ArchiveEntry {
                    index: 0,
                    name: "large.bin".to_owned(),
                    size: 0,
                    compressed_size: Some(1),
                    is_directory: false,
                    is_encrypted: Some(false),
                    unsupported_kind: false,
                    is_symbolic_link: false,
                    modified: None,
                },
                ArchiveEntry {
                    index: 1,
                    name: "small.txt".to_owned(),
                    size: 5,
                    compressed_size: Some(5),
                    is_directory: false,
                    is_encrypted: Some(false),
                    unsupported_kind: false,
                    is_symbolic_link: false,
                    modified: None,
                },
            ],
            total_size: 0,
            archive_size: 1,
            modified: SystemTime::UNIX_EPOCH,
        };
        for size in [
            PREVIOUS_LIMIT - 5,
            PREVIOUS_LIMIT - 4,
            PREVIOUS_LIMIT + 1,
            u64::MAX - 5,
        ] {
            info.entries[0].size = size;
            info.total_size = size + 5;
            let plan = plan_entries(&info, None, &JobControl::default()).unwrap();
            assert_eq!(total_selected(&plan).unwrap(), info.total_size);
            let selected =
                plan_entries(&info, Some(&BTreeSet::from([1])), &JobControl::default()).unwrap();
            assert_eq!(total_selected(&selected).unwrap(), 5);
            assert_eq!(selected[0].entry.name, "small.txt");
        }
    }
}
