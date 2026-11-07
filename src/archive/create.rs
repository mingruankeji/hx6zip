use std::{
    cell::RefCell,
    collections::{BTreeMap, HashSet},
    fs::{self, File, Metadata},
    io::{self, BufReader, BufWriter, Cursor, Read, Seek, SeekFrom, Write},
    ops::Range,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use sevenz_rust2::{
    ArchiveEntry as SevenZipEntry, ArchiveReader, ArchiveWriter, EncoderConfiguration,
    EncoderMethod, Password, PreparedBlock, SourceReader,
    encoder_options::{AesEncoderOptions, Lzma2Options},
    prepare_block,
};
use tempfile::{NamedTempFile, SpooledTempFile};
use walkdir::WalkDir;
use zip::{AesMode, CompressionMethod, ZipArchive, ZipWriter, write::FullFileOptions};

use super::{
    ArchiveFormat, Cancelled, Compression, CreateOptions, IO_BUFFER_SIZE, IntegrityMode,
    JobControl,
    common::{
        Tracker, TrackingReader, checked_metadata, collision_key, copy_stream,
        followed_source_metadata, is_link_or_reparse, persist_temp, safe_relative_path,
    },
    special::{self, MANIFEST_NAME, Manifest, SpecialEntry},
    timestamps::write_zip_times,
};

struct Source {
    path: PathBuf,
    name: String,
    size: u64,
    is_directory: bool,
    metadata: Metadata,
}

const PARALLEL_MIN_FILE_SIZE: u64 = 512 * 1024;

pub fn create_archive(options: &CreateOptions, control: &JobControl) -> Result<()> {
    create_archive_inner(options, control, false).map(|_| ())
}

pub(super) fn create_updated_archive(options: &CreateOptions, control: &JobControl) -> Result<()> {
    create_archive_impl(options, control, false, true).map(|_| ())
}

pub fn create_archive_auto(
    sources: Vec<PathBuf>,
    format: ArchiveFormat,
    control: &JobControl,
) -> Result<PathBuf> {
    let destination = crate::cli::automatic_archive_path(&sources, format)?;
    create_archive_inner(
        &CreateOptions {
            sources,
            destination,
            format,
            password: None,
            overwrite: false,
        },
        control,
        true,
    )
}

pub fn create_archives_separately(
    sources: Vec<PathBuf>,
    control: &JobControl,
) -> Result<Vec<PathBuf>> {
    if sources.is_empty() {
        bail!("请先选择要压缩的文件或文件夹");
    }
    let mut sources = sources
        .into_iter()
        .map(|path| {
            std::path::absolute(&path)
                .with_context(|| format!("无法确定源文件路径 {}", path.display()))
        })
        .collect::<Result<Vec<_>>>()?;
    // Process ancestors first so their archives cannot include this batch's later outputs.
    sources.sort_by_key(|path| path.components().count());
    let count = sources.len();
    let mut outputs = Vec::with_capacity(count);
    for (index, source) in sources.into_iter().enumerate() {
        control.check_cancelled()?;
        let parent_control = control.clone();
        let name = source.display().to_string();
        let integrity = control.integrity();
        let item_control = JobControl::new(control.cancellation.clone(), move |mut progress| {
            progress.current_file = format!(
                "[{} / {count}] {name} · {}",
                index + 1,
                progress.current_file
            );
            parent_control.report(progress);
        })
        .with_integrity(integrity);
        // Per-item progress has its own control, but warnings belong to the batch.
        let item_control = item_control.with_shared_warnings(control);
        let output = create_archive_auto(vec![source.clone()], ArchiveFormat::Zip, &item_control)
            .with_context(|| {
            format!(
                "压缩 {} 未完成；已生成 {} 个 ZIP，已完成的压缩包保留",
                source.display(),
                outputs.len()
            )
        })?;
        outputs.push(output);
    }
    Ok(outputs)
}

fn numbered_path(base: &Path, index: u64) -> PathBuf {
    if index == 0 {
        return base.to_path_buf();
    }
    let mut name = base
        .file_stem()
        .expect("validated archive filename")
        .to_os_string();
    name.push(format!(" ({index})."));
    name.push(base.extension().expect("validated archive extension"));
    base.with_file_name(name)
}

fn next_available_path(base: &Path, index: &mut u64, control: &JobControl) -> Result<PathBuf> {
    loop {
        control.check_cancelled()?;
        let path = numbered_path(base, *index);
        match fs::symlink_metadata(&path) {
            Ok(_) => *index = index.checked_add(1).context("压缩包重名序号溢出")?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(path),
            Err(error) => {
                return Err(error).with_context(|| format!("无法检查 {}", path.display()));
            }
        }
    }
}

fn create_archive_inner(
    options: &CreateOptions,
    control: &JobControl,
    auto_name: bool,
) -> Result<PathBuf> {
    create_archive_impl(options, control, auto_name, false)
}

fn create_archive_impl(
    options: &CreateOptions,
    control: &JobControl,
    auto_name: bool,
    allow_empty: bool,
) -> Result<PathBuf> {
    control.check_cancelled()?;
    if options.sources.is_empty() && !allow_empty {
        bail!("请先选择要压缩的文件或文件夹");
    }
    if options.format == ArchiveFormat::Tar
        && options
            .password
            .as_deref()
            .is_some_and(|password| !password.is_empty())
    {
        bail!("TAR 格式不支持加密");
    }
    if ArchiveFormat::from_path(&options.destination)? != options.format {
        bail!("目标文件扩展名与压缩格式不一致");
    }
    let parent = options
        .destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .context("请选择压缩包的目标文件夹")?;
    let parent = fs::canonicalize(parent)
        .with_context(|| format!("无法访问目标文件夹 {}", parent.display()))?;
    if !parent.is_dir() {
        bail!("压缩包的目标路径不是文件夹");
    }
    let file_name = options
        .destination
        .file_name()
        .context("压缩包文件名无效")?;
    let base = parent.join(file_name);
    let mut index = 0;
    let mut destination = if auto_name {
        next_available_path(&base, &mut index, control)?
    } else {
        base.clone()
    };
    if let Some(metadata) = checked_metadata(&destination)? {
        if !metadata.is_file() {
            bail!("目标压缩包不是普通文件：{}", destination.display());
        }
        if !options.overwrite {
            bail!("目标压缩包已存在，未启用覆盖：{}", destination.display());
        }
    }

    // The temporary archive and ZIP spool files live in a private folder created before the
    // sources are scanned, so writing them does not change a source folder that holds the
    // destination.
    let work = tempfile::Builder::new()
        .prefix(".hx6zip-")
        .tempdir_in(&parent)
        .context("无法在目标文件夹中创建临时压缩包")?;
    let (mut sources, special_entries) =
        collect_sources(&options.sources, &destination, work.path(), control)?;
    let ordinary_source_count = sources.len();
    if !special_entries.is_empty() {
        let reserved = MANIFEST_NAME.to_ascii_lowercase();
        if sources
            .iter()
            .any(|source| source.name.to_lowercase() == reserved)
            || special_entries
                .iter()
                .any(|entry| entry.path.to_lowercase() == reserved)
        {
            bail!("源文件与特殊情况清单的保留名称冲突：{MANIFEST_NAME}");
        }
        let manifest_path = work.path().join(MANIFEST_NAME);
        special::write(&manifest_path, &Manifest::new(special_entries.clone()))?;
        let metadata = fs::metadata(&manifest_path).context("无法读取生成的特殊情况清单")?;
        sources.push(Source {
            path: manifest_path,
            name: MANIFEST_NAME.to_owned(),
            size: metadata.len(),
            is_directory: false,
            metadata,
        });
    }
    let total = sources.iter().try_fold(0u64, |sum, source| {
        sum.checked_add(source.size).context("源文件总大小溢出")
    })?;
    let mut tracker = Tracker::new(control, "正在压缩", total);
    tracker.set_archive_size(0);
    let mut temporary =
        NamedTempFile::new_in(work.path()).context("无法在目标文件夹中创建临时压缩包")?;

    match options.format {
        ArchiveFormat::Zip => write_zip(&mut temporary, &sources, options, control, &mut tracker)?,
        ArchiveFormat::SevenZip => {
            write_sevenzip(&mut temporary, &sources, options, control, &mut tracker)?
        }
        ArchiveFormat::Tar => write_tar(&mut temporary, &sources, control, &mut tracker)?,
    }
    tracker.finish()?;
    temporary
        .as_file_mut()
        .sync_all()
        .context("无法保存压缩包数据")?;
    if control.integrity() == IntegrityMode::Safe {
        verify_created_archive(&temporary, &sources, options, control)?;
        let (fresh, fresh_special) =
            collect_sources(&options.sources, &destination, work.path(), control)?;
        let original_names = sources[..ordinary_source_count]
            .iter()
            .map(|source| &source.name)
            .collect::<HashSet<_>>();
        let fresh_names = fresh
            .iter()
            .map(|source| &source.name)
            .collect::<HashSet<_>>();
        if original_names != fresh_names || special_entries != fresh_special {
            bail!("源文件列表在压缩过程中发生了变化，未生成压缩包");
        }
    }
    for source in &sources {
        control.check_cancelled()?;
        validate_source(source)?;
    }
    control.check_cancelled()?;
    if auto_name {
        // Retry only the atomic publication if another task claimed the name.
        loop {
            control.check_cancelled()?;
            match temporary.persist_noclobber(&destination) {
                Ok(_) => return Ok(destination),
                Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
                    temporary = error.file;
                    index = index.checked_add(1).context("压缩包重名序号溢出")?;
                    destination = next_available_path(&base, &mut index, control)?;
                }
                Err(error) => {
                    return Err(error.error)
                        .with_context(|| format!("无法保存 {}", destination.display()));
                }
            }
        }
    }
    checked_metadata(&destination)?;
    persist_temp(temporary, &destination, options.overwrite)?;
    Ok(destination)
}

fn verify_created_archive(
    temporary: &NamedTempFile,
    sources: &[Source],
    options: &CreateOptions,
    control: &JobControl,
) -> Result<()> {
    let total = sources.iter().try_fold(0u64, |sum, source| {
        sum.checked_add(source.size).context("源文件总大小溢出")
    })?;
    let mut tracker = Tracker::new(control, "正在校验压缩包", total);
    tracker.set_archive_size(temporary.as_file().metadata()?.len());
    let by_name = sources
        .iter()
        .map(|source| (source.name.as_str(), source))
        .collect::<BTreeMap<_, _>>();
    let mut seen = HashSet::new();
    let mut verify = |name: &str, directory: bool, size: u64, input: &mut dyn Read| -> Result<()> {
        let name = name.trim_end_matches('/');
        let source = by_name
            .get(name)
            .with_context(|| format!("压缩包包含意外条目：{name}"))?;
        if !seen.insert(source.name.as_str())
            || directory != source.is_directory
            || size != source.size
        {
            bail!("压缩包条目与源文件不一致：{name}");
        }
        tracker.start_file(name)?;
        validate_source(source)?;
        if !directory {
            let file = File::open(&source.path)
                .with_context(|| format!("无法回读源文件 {}", source.path.display()))?;
            let mut comparison = CompareSource(BufReader::with_capacity(IO_BUFFER_SIZE, file));
            copy_stream(input, &mut comparison, source.size, control, |count| {
                tracker.advance(count)
            })
            .with_context(|| format!("压缩包回读校验失败：{name}"))?;
            if comparison.0.read(&mut [0u8; 1])? != 0 {
                bail!("源文件在校验过程中变大：{name}");
            }
            validate_source(source)?;
        }
        Ok(())
    };
    let mut file = temporary.reopen().context("无法回读临时压缩包")?;
    file.rewind()?;
    match options.format {
        ArchiveFormat::Zip => {
            let mut reader = ZipArchive::new(BufReader::with_capacity(IO_BUFFER_SIZE, file))
                .context("无法回读 ZIP 压缩包")?;
            for index in 0..reader.len() {
                control.check_cancelled()?;
                let mut entry = match options
                    .password
                    .as_deref()
                    .filter(|password| !password.is_empty())
                {
                    Some(password) => reader.by_index_decrypt(index, password.as_bytes()),
                    None => reader.by_index(index),
                }
                .context("无法校验 ZIP 条目")?;
                verify(
                    &entry.name().to_owned(),
                    entry.is_dir(),
                    entry.size(),
                    &mut entry,
                )?;
            }
        }
        ArchiveFormat::SevenZip => {
            let mut reader = ArchiveReader::new(
                BufReader::with_capacity(IO_BUFFER_SIZE, file),
                Password::from(options.password.as_deref().unwrap_or_default()),
            )
            .context("无法回读 7z 压缩包")?;
            let mut inner_error = None;
            let result = reader.for_each_entries(|entry, input| {
                match verify(&entry.name, entry.is_directory, entry.size, input) {
                    Ok(()) => Ok(true),
                    Err(error) => {
                        inner_error = Some(error);
                        Err(sevenz_rust2::Error::Other("压缩包回读校验失败".into()))
                    }
                }
            });
            if let Some(error) = inner_error {
                return Err(error);
            }
            result.context("7z 压缩包回读校验失败")?;
        }
        ArchiveFormat::Tar => super::other::read_tar_entries(
            &mut file,
            Compression::None,
            control,
            |entry, input| verify(&entry.name, entry.is_directory, entry.size, input),
        )?,
    }
    if seen.len() != sources.len() {
        bail!("压缩包缺少源文件条目，未生成压缩包");
    }
    tracker.finish()
}

fn write_tar(
    temporary: &mut NamedTempFile,
    sources: &[Source],
    control: &JobControl,
    tracker: &mut Tracker<'_>,
) -> Result<()> {
    let tracker = Mutex::new(tracker);
    let output = BufWriter::with_capacity(IO_BUFFER_SIZE, temporary.as_file_mut());
    let mut archive = tar::Builder::new(ArchiveSizeWriter::new(output, &tracker));
    for source in sources {
        start_source(source, &tracker)?;
        let modified = source
            .metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |time| time.as_secs());
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(if source.is_directory {
            tar::EntryType::Directory
        } else {
            tar::EntryType::Regular
        });
        header.set_size(source.size);
        header.set_mode(if source.is_directory { 0o755 } else { 0o644 });
        header.set_mtime(modified);
        if source.is_directory {
            archive
                .append_data(&mut header, &source.name, io::empty())
                .with_context(|| format!("无法添加 TAR 文件夹 {}", source.name))?;
        } else {
            let file = File::open(&source.path)
                .with_context(|| format!("无法打开源文件 {}", source.path.display()))?;
            let tracker = &tracker;
            let mut input = super::common::TrackingReader {
                file: BufReader::with_capacity(IO_BUFFER_SIZE, file),
                expected: source.size,
                read: 0,
                control,
                advance: |bytes| advance_tracker(tracker, bytes),
            };
            archive
                .append_data(&mut header, &source.name, &mut input)
                .with_context(|| format!("无法添加 TAR 文件 {}", source.name))?;
            if input.read != source.size {
                bail!("文件大小与预期不符：{}", source.name);
            }
        }
        validate_source(source)?;
    }
    control.check_cancelled()?;
    archive.finish().context("无法完成 TAR 压缩包")?;
    archive
        .into_inner()
        .context("无法写入 TAR 压缩包")?
        .flush()
        .context("无法刷新 TAR 压缩包")?;
    Ok(())
}

struct CompareSource(BufReader<File>);

impl Write for CompareSource {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut buffer = [0u8; IO_BUFFER_SIZE];
        for chunk in bytes.chunks(IO_BUFFER_SIZE) {
            self.0.read_exact(&mut buffer[..chunk.len()])?;
            if chunk != &buffer[..chunk.len()] {
                return Err(io::Error::other("解压数据与源文件内容不一致"));
            }
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn collect_sources(
    paths: &[PathBuf],
    destination: &Path,
    work: &Path,
    control: &JobControl,
) -> Result<(Vec<Source>, Vec<SpecialEntry>)> {
    let mut pending = Vec::new();
    let mut names = HashSet::new();
    let mut special_entries = BTreeMap::new();
    let destination_existing = if destination.exists() {
        Some(fs::canonicalize(destination).context("无法检查目标压缩包")?)
    } else {
        None
    };
    let work = fs::canonicalize(work).context("无法检查临时压缩包文件夹")?;
    let is_work = |item: &walkdir::DirEntry| {
        item.depth() > 0
            && item.file_type().is_dir()
            && (item.path_is_symlink() || item.file_name() == work.file_name().unwrap_or_default())
            && fs::canonicalize(item.path()).ok().as_ref() == Some(&work)
    };

    for root in paths {
        control.check_cancelled()?;
        let root_name = root
            .file_name()
            .and_then(|name| name.to_str())
            .with_context(|| format!("无法读取源文件名 {}", root.display()))?;
        let root_parent = root.parent().context("不能直接压缩磁盘根目录")?;

        for item in WalkDir::new(root)
            .follow_links(true)
            .into_iter()
            .filter_entry(|item| !is_work(item))
        {
            control.check_cancelled()?;
            // The archive being replaced is not packed into its successor.
            if let Ok(item) = &item
                && item.depth() > 0
                && item.file_type().is_file()
                && let Some(existing) = &destination_existing
                && (item.path_is_symlink()
                    || item.file_name() == existing.file_name().unwrap_or_default())
                && fs::canonicalize(item.path()).ok().as_ref() == Some(existing)
            {
                continue;
            }
            let (path, scan_error) = match item {
                Ok(item) => (item.into_path(), None),
                Err(error) if error.loop_ancestor().is_some() => {
                    return Err(error).context("源文件夹包含循环链接，无法继续压缩");
                }
                Err(error) => {
                    let reason = error.to_string();
                    let Some(path) = error.path().map(Path::to_owned) else {
                        control.warn(format!("扫描警告（已跳过）：{reason}"));
                        continue;
                    };
                    (path, Some(reason))
                }
            };
            let relative = path
                .strip_prefix(root_parent)
                .context("无法计算压缩包内的路径")?;
            let name = relative
                .to_str()
                .with_context(|| format!("文件名不是有效的 Unicode：{}", path.display()))?
                .replace('\\', "/");
            let name = if name.is_empty() {
                root_name.to_owned()
            } else {
                name
            };
            let safe = safe_relative_path(&name)?;
            if let Some(reason) = scan_error {
                let entry = special::capture(&path, &name, &reason)?;
                if entry.kind == special::SpecialKind::Skipped {
                    control.warn(format!("扫描警告（已跳过）：{reason}"));
                }
                special_entries.insert(name, entry);
                continue;
            }
            if !names.insert(collision_key(&safe)) {
                bail!("多个源文件在压缩包中会产生同名路径：{name}");
            }
            if let Some(entry) = special::unresolved_link(&path, &name) {
                if entry.kind == special::SpecialKind::Skipped {
                    control.warn(format!(
                        "扫描警告（已跳过链接）：{}：{}",
                        path.display(),
                        entry.reason
                    ));
                }
                special_entries.insert(name, entry);
                continue;
            }
            pending.push((path, name));
        }
    }

    let metadata = source_metadata(&pending, control)?;
    let mut sources = Vec::with_capacity(pending.len());
    for ((path, name), metadata) in pending.into_iter().zip(metadata) {
        match metadata {
            Ok(metadata) => sources.push(Source {
                path,
                name,
                size: if metadata.is_file() {
                    metadata.len()
                } else {
                    0
                },
                is_directory: metadata.is_dir(),
                metadata,
            }),
            Err(error) => {
                let reason = error.to_string();
                let entry = special::capture(&path, &name, &reason)?;
                if entry.kind == special::SpecialKind::Skipped {
                    control.warn(format!("扫描警告（已跳过）：{}：{reason}", path.display()));
                }
                special_entries.insert(name, entry);
            }
        }
    }
    Ok((sources, special_entries.into_values().collect()))
}

/// Reads metadata on several threads; each lookup opens the path on Windows.
fn source_metadata(
    items: &[(PathBuf, String)],
    control: &JobControl,
) -> Result<Vec<io::Result<Metadata>>> {
    const CHUNK: usize = 256;
    let read = |items: &[(PathBuf, String)]| {
        items
            .iter()
            .map(|(path, _)| {
                control.check_cancelled()?;
                let result = fs::metadata(path).and_then(|metadata| {
                    if metadata.is_file() {
                        File::open(path)?;
                    }
                    Ok(metadata)
                });
                if let Ok(metadata) = &result
                    && !metadata.is_file()
                    && !metadata.is_dir()
                {
                    bail!("不支持的源文件类型：{}", path.display());
                }
                Ok(result)
            })
            .collect::<Result<Vec<_>>>()
    };
    let workers = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1)
        .min(items.len().div_ceil(CHUNK));
    if workers <= 1 {
        return read(items);
    }
    let chunks = items.chunks(CHUNK).collect::<Vec<_>>();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let failed = std::sync::atomic::AtomicBool::new(false);
    let results = Mutex::new(Vec::with_capacity(chunks.len()));
    std::thread::scope(|scope| -> Result<()> {
        for _ in 0..workers {
            std::thread::Builder::new()
                .name("hx6zip-scan".to_owned())
                .spawn_scoped(scope, || {
                    use std::sync::atomic::Ordering;
                    while !failed.load(Ordering::Relaxed) {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        let Some(chunk) = chunks.get(index) else {
                            return;
                        };
                        let result = read(chunk);
                        failed.fetch_or(result.is_err(), Ordering::Relaxed);
                        if let Ok(mut results) = results.lock() {
                            results.push((index, result));
                        }
                    }
                })
                .context("无法启动文件扫描线程")?;
        }
        Ok(())
    })?;
    let mut results = results
        .into_inner()
        .map_err(|_| anyhow::anyhow!("文件扫描状态异常"))?;
    results.sort_unstable_by_key(|(index, _)| *index);
    if let Some(position) = results.iter().position(|(_, result)| result.is_err()) {
        return results.swap_remove(position).1.map(|_| Vec::new());
    }
    if results.len() != chunks.len() {
        bail!("文件扫描线程意外停止");
    }
    let mut metadata = Vec::with_capacity(items.len());
    for (_, result) in results {
        metadata.extend(result?);
    }
    Ok(metadata)
}

fn validate_source(source: &Source) -> Result<()> {
    let current = if source.is_directory && is_link_or_reparse(&source.metadata) {
        fs::symlink_metadata(&source.path)
            .with_context(|| format!("无法检查源文件夹：{}", source.path.display()))?
    } else {
        followed_source_metadata(&source.path)?
    };
    let same_kind = if source.is_directory {
        current.is_dir() || (is_link_or_reparse(&source.metadata) && is_link_or_reparse(&current))
    } else {
        current.is_file() && current.len() == source.size
    };
    let original_time = source
        .metadata
        .modified()
        .with_context(|| format!("无法读取 {} 的修改时间", source.path.display()))?;
    let current_time = current
        .modified()
        .with_context(|| format!("无法读取 {} 的修改时间", source.path.display()))?;
    if !same_kind || original_time != current_time {
        bail!(
            "源文件或文件夹在压缩过程中发生了变化：{}",
            source.path.display()
        );
    }
    Ok(())
}

fn write_zip(
    temporary: &mut NamedTempFile,
    sources: &[Source],
    options: &CreateOptions,
    control: &JobControl,
    tracker: &mut Tracker<'_>,
) -> Result<()> {
    let spool_dir = temporary
        .path()
        .parent()
        .context("无法确定临时压缩包所在文件夹")?
        .to_path_buf();
    let tracker = Mutex::new(tracker);
    let output = BufWriter::with_capacity(IO_BUFFER_SIZE, temporary.as_file_mut());
    let mut writer = ZipWriter::new(ArchiveSizeWriter::new(output, &tracker));
    let password = options.password.as_deref().filter(|p| !p.is_empty());
    // AES entries are encrypted by the ZIP writer, so they are never split.
    let tasks = zip_tasks(sources, password.is_none());
    let workers = if tasks.len() > 1 {
        std::thread::available_parallelism()
            .context("无法获取可用处理器数量")?
            .get()
            .min(tasks.len())
    } else {
        1
    };
    if workers > 1 {
        let shared = ZipShared {
            sources,
            tasks: &tasks,
            spool_dir: &spool_dir,
            password,
            control,
            tracker: &tracker,
        };
        write_zip_parallel(&mut writer, &shared, workers)?;
    } else {
        for source in sources {
            write_zip_source(&mut writer, source, password, control, &tracker)?;
        }
    }
    control.check_cancelled()?;
    writer
        .finish()
        .context("无法完成 ZIP 压缩包")?
        .flush()
        .context("无法写入 ZIP 压缩包")?;
    Ok(())
}

struct ArchiveSizeWriter<'a, 'b, 'c, W> {
    inner: W,
    tracker: &'a Mutex<&'b mut Tracker<'c>>,
    position: u64,
    size: u64,
    last_report: Instant,
}

impl<'a, 'b, 'c, W> ArchiveSizeWriter<'a, 'b, 'c, W> {
    fn new(inner: W, tracker: &'a Mutex<&'b mut Tracker<'c>>) -> Self {
        Self {
            inner,
            tracker,
            // Both archive writers start with an empty temporary file.
            position: 0,
            size: 0,
            last_report: Instant::now(),
        }
    }

    fn report_size(&mut self) -> io::Result<()> {
        self.tracker
            .lock()
            .map_err(|_| io::Error::other("压缩进度状态异常"))?
            .set_archive_size(self.size);
        self.last_report = Instant::now();
        Ok(())
    }
}

impl<W: Write> Write for ArchiveSizeWriter<'_, '_, '_, W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buffer)?;
        if written > 0 {
            self.position = self
                .position
                .checked_add(written as u64)
                .ok_or_else(|| io::Error::other("压缩包大小溢出"))?;
            self.size = self.size.max(self.position);
            if self.last_report.elapsed() >= Duration::from_millis(100) {
                self.report_size()?;
            }
        }
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()?;
        self.report_size()
    }
}

impl<W: Seek> Seek for ArchiveSizeWriter<'_, '_, '_, W> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.position = self.inner.seek(position)?;
        Ok(self.position)
    }

    fn stream_position(&mut self) -> io::Result<u64> {
        // A position-only query must not flush the underlying buffer.
        Ok(self.position)
    }
}

fn start_source(source: &Source, tracker: &Mutex<&mut Tracker<'_>>) -> Result<()> {
    tracker
        .lock()
        .map_err(|_| anyhow::anyhow!("压缩进度状态异常"))?
        .start_file(&source.name)?;
    validate_source(source)
}

fn advance_tracker(tracker: &Mutex<&mut Tracker<'_>>, bytes: usize) -> Result<()> {
    tracker
        .lock()
        .map_err(|_| anyhow::anyhow!("压缩进度状态异常"))?
        .advance(bytes)
}

fn zip_file_options(source: &Source) -> Result<FullFileOptions<'static>> {
    let mut base = FullFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .compression_level(Some(ZIP_DEFLATE_LEVEL as i64))
        .large_file(source.size >= u32::MAX as u64);
    let modified = write_zip_times(&mut base, &source.metadata)
        .with_context(|| format!("无法保存 {} 的修改时间", source.name))?;
    Ok(base.last_modified_time(modified))
}

fn write_zip_source<W: Write + Seek>(
    writer: &mut ZipWriter<W>,
    source: &Source,
    password: Option<&str>,
    control: &JobControl,
    tracker: &Mutex<&mut Tracker<'_>>,
) -> Result<()> {
    start_source(source, tracker)?;
    let base = zip_file_options(source)?;
    if source.is_directory {
        writer
            .add_directory(&source.name, base)
            .with_context(|| format!("无法添加文件夹 {}", source.name))?;
    } else {
        let file_options = if let Some(password) = password {
            base.with_aes_encryption(AesMode::Aes256, password)
        } else {
            base
        };
        writer
            .start_file(&source.name, file_options)
            .with_context(|| format!("无法添加文件 {}", source.name))?;
        let mut input = File::open(&source.path)
            .with_context(|| format!("无法打开源文件 {}", source.path.display()))?;
        copy_stream(&mut input, writer, source.size, control, |bytes| {
            advance_tracker(tracker, bytes)
        })
        .with_context(|| format!("无法压缩 {}", source.name))?;
    }
    validate_source(source)
}

const ZIP_DEFLATE_LEVEL: u32 = 6;
/// Small files and folders are grouped so per-entry overhead does not dominate.
const GROUP_MAX_BYTES: u64 = 2 * 1024 * 1024;
const GROUP_MAX_ENTRIES: usize = 128;
/// Files in this range are split into independently compressed Deflate chunks.
const ZIP_CHUNKED_MIN_FILE_SIZE: u64 = 16 * 1024 * 1024;
const ZIP_CHUNKED_MAX_FILE_SIZE: u64 = 2 * 1024 * 1024 * 1024;
const ZIP_CHUNK_SIZE: u64 = 4 * 1024 * 1024;
/// Compressed job output above this size spills to a temporary file.
const ZIP_SPOOL_MEMORY: usize = 8 * 1024 * 1024;
/// Tasks that may be compressed ahead of the next one being merged.
const TASKS_AHEAD_PER_WORKER: usize = 4;
const MAX_TASKS_AHEAD: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq)]
enum ZipTask {
    /// Consecutive sources written into one temporary archive.
    Entries(Range<usize>),
    /// Part of one large file, merged in order by the main thread.
    Chunk {
        source: usize,
        offset: u64,
        len: u64,
    },
}

fn zip_tasks(sources: &[Source], allow_chunks: bool) -> Vec<ZipTask> {
    let mut tasks = Vec::new();
    let mut start = 0;
    let mut bytes = 0u64;
    for (index, source) in sources.iter().enumerate() {
        let alone = !source.is_directory && source.size >= PARALLEL_MIN_FILE_SIZE;
        if start < index
            && (alone
                || index - start >= GROUP_MAX_ENTRIES
                || bytes + source.size > GROUP_MAX_BYTES)
        {
            tasks.push(ZipTask::Entries(start..index));
            start = index;
            bytes = 0;
        }
        if !alone {
            bytes += source.size;
            continue;
        }
        start = index + 1;
        if allow_chunks
            && (ZIP_CHUNKED_MIN_FILE_SIZE..ZIP_CHUNKED_MAX_FILE_SIZE).contains(&source.size)
        {
            let mut offset = 0;
            while offset < source.size {
                let len = ZIP_CHUNK_SIZE.min(source.size - offset);
                tasks.push(ZipTask::Chunk {
                    source: index,
                    offset,
                    len,
                });
                offset += len;
            }
        } else {
            tasks.push(ZipTask::Entries(index..index + 1));
        }
    }
    if start < sources.len() {
        tasks.push(ZipTask::Entries(start..sources.len()));
    }
    tasks
}

enum ZipTaskOutput {
    Archive(ZipArchive<SpooledTempFile>),
    Chunk {
        data: Vec<u8>,
        crc: crc32fast::Hasher,
    },
}

struct Pipeline<T> {
    next_task: usize,
    merged: usize,
    stopped: bool,
    ready: BTreeMap<usize, Result<T>>,
}

/// Tasks compressed by workers in any order and merged in order by the calling thread.
struct PipelineState<T> {
    state: Mutex<Pipeline<T>>,
    changed: Condvar,
    format: &'static str,
}

impl<T> PipelineState<T> {
    fn new(format: &'static str) -> Self {
        Self {
            state: Mutex::new(Pipeline {
                next_task: 0,
                merged: 0,
                stopped: false,
                ready: BTreeMap::new(),
            }),
            changed: Condvar::new(),
            format,
        }
    }

    fn lock(&self) -> Result<MutexGuard<'_, Pipeline<T>>> {
        self.state
            .lock()
            .map_err(|_| anyhow::anyhow!("{} 压缩任务状态异常", self.format))
    }

    fn stop(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.stopped = true;
        }
        self.changed.notify_all();
    }

    fn take(&self, index: usize) -> Result<T> {
        let state = self.lock()?;
        let mut state = self
            .changed
            .wait_while(state, |state| {
                !(state.ready.contains_key(&index) || (state.stopped && index >= state.next_task))
            })
            .map_err(|_| anyhow::anyhow!("{} 压缩任务状态异常", self.format))?;
        match state.ready.remove(&index) {
            Some(output) => output,
            // A later task failed and no worker will start this one.
            None => {
                let failed = state.ready.iter().find(|(_, output)| output.is_err());
                match failed.map(|(&index, _)| index) {
                    Some(failed) => state.ready.remove(&failed).expect("failed task"),
                    None => Err(anyhow::anyhow!("{} 压缩线程意外停止", self.format)),
                }
            }
        }
    }

    fn finish_task(&self, index: usize) -> Result<()> {
        self.lock()?.merged = index + 1;
        self.changed.notify_all();
        Ok(())
    }
}

/// Wakes idle workers when the merging thread returns early.
struct StopOnDrop<'a, T>(&'a PipelineState<T>);

impl<T> Drop for StopOnDrop<'_, T> {
    fn drop(&mut self) {
        self.0.stop();
    }
}

/// Runs tasks until all are claimed or the pipeline stops. `run` returns `None` for tasks the
/// merging thread handles itself.
fn pipeline_worker<T>(
    pipeline: &PipelineState<T>,
    total: usize,
    blocked: impl Fn(&Pipeline<T>) -> bool,
    run: impl Fn(usize) -> Option<Result<T>>,
) {
    loop {
        let index = {
            let Ok(state) = pipeline.state.lock() else {
                return;
            };
            let Ok(mut state) = pipeline.changed.wait_while(state, |state| {
                !state.stopped && state.next_task < total && blocked(state)
            }) else {
                return;
            };
            if state.stopped || state.next_task >= total {
                return;
            }
            state.next_task += 1;
            state.next_task - 1
        };
        let output = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(index)))
            .unwrap_or_else(|_| Some(Err(anyhow::anyhow!("{} 压缩线程异常退出", pipeline.format))));
        let Some(output) = output else {
            continue;
        };
        let failed = output.is_err();
        if let Ok(mut state) = pipeline.state.lock() {
            state.ready.insert(index, output);
            state.stopped |= failed;
        }
        pipeline.changed.notify_all();
        if failed {
            return;
        }
    }
}

struct ZipShared<'a, 'b, 'c> {
    sources: &'a [Source],
    tasks: &'a [ZipTask],
    spool_dir: &'a Path,
    password: Option<&'a str>,
    control: &'a JobControl,
    tracker: &'a Mutex<&'b mut Tracker<'c>>,
}

fn write_zip_parallel<W: Write + Seek>(
    writer: &mut ZipWriter<W>,
    shared: &ZipShared<'_, '_, '_>,
    workers: usize,
) -> Result<()> {
    let pipeline = PipelineState::new("ZIP");
    let window = (workers * TASKS_AHEAD_PER_WORKER).min(MAX_TASKS_AHEAD);
    let blocked = |state: &Pipeline<ZipTaskOutput>| state.next_task >= state.merged + window;
    std::thread::scope(|scope| {
        let _stop = StopOnDrop(&pipeline);
        for _ in 0..workers {
            let pipeline = &pipeline;
            let blocked = &blocked;
            std::thread::Builder::new()
                .name("hx6zip-ZIP-worker".to_owned())
                .spawn_scoped(scope, move || {
                    pipeline_worker(pipeline, shared.tasks.len(), blocked, |index| {
                        Some(run_zip_task(&shared.tasks[index], shared))
                    })
                })
                .context("无法启动 ZIP 压缩线程")?;
        }
        let mut chunked: Option<ChunkedZipEntry> = None;
        for (index, task) in shared.tasks.iter().enumerate() {
            let output = pipeline.take(index)?;
            shared.control.check_cancelled()?;
            match (task, output) {
                (ZipTask::Entries(_), ZipTaskOutput::Archive(archive)) => {
                    writer.merge_archive(archive).context("无法合并 ZIP 条目")?;
                }
                (
                    &ZipTask::Chunk {
                        source,
                        offset,
                        len,
                    },
                    ZipTaskOutput::Chunk { data, crc },
                ) => {
                    let source = &shared.sources[source];
                    if offset == 0 {
                        chunked = Some(ChunkedZipEntry::new(source, shared.spool_dir)?);
                    }
                    let entry = chunked.as_mut().context("ZIP 分块顺序异常")?;
                    entry
                        .push(&data, &crc)
                        .with_context(|| format!("无法压缩 {}", source.name))?;
                    if offset + len == source.size {
                        let archive = chunked
                            .take()
                            .context("ZIP 分块顺序异常")?
                            .finish(source.size)
                            .with_context(|| format!("无法压缩 {}", source.name))?;
                        validate_source(source)?;
                        writer.merge_archive(archive).context("无法合并 ZIP 条目")?;
                    }
                }
                _ => bail!("ZIP 压缩任务结果类型异常"),
            }
            pipeline.finish_task(index)?;
        }
        Ok(())
    })
}

fn run_zip_task(task: &ZipTask, shared: &ZipShared<'_, '_, '_>) -> Result<ZipTaskOutput> {
    match task {
        ZipTask::Entries(range) => {
            let mut writer = ZipWriter::new(tempfile::spooled_tempfile_in(
                ZIP_SPOOL_MEMORY,
                shared.spool_dir,
            ));
            for source in &shared.sources[range.clone()] {
                write_zip_source(
                    &mut writer,
                    source,
                    shared.password,
                    shared.control,
                    shared.tracker,
                )?;
            }
            // Retain the writer's AES method metadata when merging.
            Ok(ZipTaskOutput::Archive(
                writer.finish_into_readable().context("无法完成 ZIP 条目")?,
            ))
        }
        &ZipTask::Chunk {
            source,
            offset,
            len,
        } => {
            let source = &shared.sources[source];
            compress_zip_chunk(source, offset, len, shared)
                .with_context(|| format!("无法压缩 {}", source.name))
        }
    }
}

fn compress_zip_chunk(
    source: &Source,
    offset: u64,
    len: u64,
    shared: &ZipShared<'_, '_, '_>,
) -> Result<ZipTaskOutput> {
    if offset == 0 {
        start_source(source, shared.tracker)?;
    }
    shared.control.check_cancelled()?;
    let mut input = vec![0; usize::try_from(len).context("ZIP 分块过大")?];
    let mut file = File::open(&source.path)
        .with_context(|| format!("无法打开源文件 {}", source.path.display()))?;
    file.seek(SeekFrom::Start(offset))
        .context("读取文件内容失败")?;
    file.read_exact(&mut input).map_err(|error| {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            anyhow::anyhow!("文件在压缩过程中发生变化")
        } else {
            anyhow::Error::new(error).context("读取文件内容失败")
        }
    })?;
    advance_tracker(shared.tracker, input.len())?;
    let mut crc = crc32fast::Hasher::new();
    crc.update(&input);
    let data = deflate_chunk(&input, offset + len == source.size)?;
    Ok(ZipTaskOutput::Chunk { data, crc })
}

/// Non-final chunks end with a sync flush: byte-aligned and without a final block, so
/// consecutive chunks form one valid Deflate stream.
fn deflate_chunk(input: &[u8], last: bool) -> Result<Vec<u8>> {
    use flate2::{Compress, Compression, FlushCompress, Status};

    let mut compressor = Compress::new(Compression::new(ZIP_DEFLATE_LEVEL), false);
    let flush = if last {
        FlushCompress::Finish
    } else {
        FlushCompress::Sync
    };
    let mut output = Vec::with_capacity(input.len() + input.len() / 256 + 64);
    loop {
        let consumed = usize::try_from(compressor.total_in()).context("压缩数据失败")?;
        let status = compressor
            .compress_vec(&input[consumed..], &mut output, flush)
            .context("压缩数据失败")?;
        let all_input = compressor.total_in() == input.len() as u64;
        // zlib finished the flush only if it stopped with output space left.
        let done = match status {
            Status::StreamEnd => true,
            Status::Ok | Status::BufError => !last && all_input && output.len() < output.capacity(),
        };
        if done {
            return Ok(output);
        }
        if output.len() == output.capacity() {
            output.reserve(64 * 1024);
        }
    }
}

/// Builds a one-entry archive from a header produced by `ZipWriter` and precompressed data.
struct ChunkedZipEntry {
    output: SpooledTempFile,
    local_header: Vec<u8>,
    central_header: Vec<u8>,
    end_record: Vec<u8>,
    crc: crc32fast::Hasher,
    compressed: u64,
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(
        bytes
            .get(offset..offset + 2)
            .context("ZIP 条目模板无效")?
            .try_into()?,
    ))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(
        bytes
            .get(offset..offset + 4)
            .context("ZIP 条目模板无效")?
            .try_into()?,
    ))
}

fn write_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn write_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

impl ChunkedZipEntry {
    const LOCAL_SIGNATURE: &[u8] = b"PK\x03\x04";
    const CENTRAL_SIGNATURE: &[u8] = b"PK\x01\x02";
    const END_SIGNATURE: &[u8] = b"PK\x05\x06";
    const DEFLATE_METHOD: u16 = 8;
    const DEFLATE_VERSION: u16 = 20;

    fn new(source: &Source, spool_dir: &Path) -> Result<Self> {
        let options = zip_file_options(source)?;
        let mut template = ZipWriter::new(Cursor::new(Vec::new()));
        template
            .start_file(&source.name, options)
            .with_context(|| format!("无法添加文件 {}", source.name))?;
        let bytes = template
            .finish()
            .context("无法生成 ZIP 条目头")?
            .into_inner();

        let end = bytes.len().checked_sub(22).context("ZIP 条目模板无效")?;
        let central_size = read_u32(&bytes, end + 12)? as usize;
        let central_start = read_u32(&bytes, end + 16)? as usize;
        let local_length =
            30 + usize::from(read_u16(&bytes, 26)?) + usize::from(read_u16(&bytes, 28)?);
        let central_length = 46
            + usize::from(read_u16(&bytes, central_start + 28)?)
            + usize::from(read_u16(&bytes, central_start + 30)?)
            + usize::from(read_u16(&bytes, central_start + 32)?);
        if !bytes.starts_with(Self::LOCAL_SIGNATURE)
            || bytes.get(central_start..central_start + 4) != Some(Self::CENTRAL_SIGNATURE)
            || &bytes[end..end + 4] != Self::END_SIGNATURE
            || read_u16(&bytes, end + 8)? != 1
            || read_u16(&bytes, end + 20)? != 0
            || central_start + central_size != end
            || central_length != central_size
            || local_length > central_start
            || read_u16(&bytes, 6)? & 0x08 != 0
            || read_u32(&bytes, central_start + 42)? != 0
        {
            bail!("ZIP 条目模板结构异常");
        }
        let mut local_header = bytes[..local_length].to_vec();
        let mut central_header = bytes[central_start..end].to_vec();
        let end_record = bytes[end..].to_vec();
        let version = read_u16(&local_header, 4)?.max(Self::DEFLATE_VERSION);
        write_u16(&mut local_header, 4, version);
        write_u16(&mut local_header, 8, Self::DEFLATE_METHOD);
        let version = read_u16(&central_header, 6)?.max(Self::DEFLATE_VERSION);
        write_u16(&mut central_header, 6, version);
        write_u16(&mut central_header, 10, Self::DEFLATE_METHOD);

        let mut output = tempfile::spooled_tempfile_in(ZIP_SPOOL_MEMORY, spool_dir);
        output
            .write_all(&local_header)
            .context("无法写入临时数据")?;
        Ok(Self {
            output,
            local_header,
            central_header,
            end_record,
            crc: crc32fast::Hasher::new(),
            compressed: 0,
        })
    }

    fn push(&mut self, data: &[u8], crc: &crc32fast::Hasher) -> Result<()> {
        self.output.write_all(data).context("无法写入临时数据")?;
        self.compressed += data.len() as u64;
        self.crc.combine(crc);
        Ok(())
    }

    fn finish(mut self, size: u64) -> Result<ZipArchive<SpooledTempFile>> {
        let crc = self.crc.finalize();
        let compressed = u32::try_from(self.compressed).context("压缩后的条目过大")?;
        let size = u32::try_from(size).context("条目过大")?;
        let central_start = u32::try_from(self.local_header.len() as u64 + self.compressed)
            .context("压缩后的条目过大")?;
        for (header, offset) in [(&mut self.local_header, 14), (&mut self.central_header, 16)] {
            write_u32(header, offset, crc);
            write_u32(header, offset + 4, compressed);
            write_u32(header, offset + 8, size);
        }
        write_u32(&mut self.end_record, 16, central_start);
        self.output.write_all(&self.central_header)?;
        self.output.write_all(&self.end_record)?;
        self.output.seek(SeekFrom::Start(0))?;
        self.output.write_all(&self.local_header)?;
        ZipArchive::new(self.output).context("无法完成 ZIP 条目")
    }
}

/// Like 7-Zip/PeaZip "normal" (`-mx5 -mfb=32`): BT4 with a nice length of 32, but a smaller
/// dictionary and blocks.
const SEVENZIP_PRESET: u32 = 5;
const SEVENZIP_SIMILAR_PRESET: u32 = 3;
const SEVENZIP_DICTIONARY: u32 = 16 * 1024 * 1024;
/// Solid blocks are sized to keep every thread busy, within these bounds. Files from the
/// block size up are streamed by the multi-threaded LZMA2 encoder.
const SEVENZIP_MIN_BLOCK: u64 = 8 * 1024 * 1024;
const SEVENZIP_MAX_BLOCK: u64 = 64 * 1024 * 1024;
const SEVENZIP_SAMPLE_SIZE: usize = 4 * 1024;
const SEVENZIP_SAMPLE_MIN_FILE: u64 = 256 * 1024;
const SEVENZIP_SIMILAR_SEGMENTS: usize = 64;
const SEVENZIP_SIMILAR_MIN_MATCHES: usize = (SEVENZIP_SIMILAR_SEGMENTS * 9 + 9) / 10;
const SEVENZIP_PE_MIN_FILE: u64 = 16 * 1024;
const SEVENZIP_PE_HEADER_SIZE: usize = 4096 + 512;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct SevenZipHint {
    filter: Option<EncoderMethod>,
    similarity: Option<SevenZipSimilarity>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SevenZipSimilarity {
    sample: SevenZipSample,
    segments: Box<[u32; SEVENZIP_SIMILAR_SEGMENTS]>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SevenZipSample {
    size: u64,
    samples: [u32; 3],
}

impl SevenZipSimilarity {
    // Fingerprints select compression tasks only; the encoder still reads every file.
    fn matches(&self, other: &Self) -> bool {
        self.sample == other.sample
            && self
                .segments
                .iter()
                .zip(other.segments.iter())
                .filter(|(left, right)| left == right)
                .count()
                >= SEVENZIP_SIMILAR_MIN_MATCHES
    }
}

fn sevenzip_fingerprint(
    source: &Source,
    control: &JobControl,
) -> Result<Box<[u32; SEVENZIP_SIMILAR_SEGMENTS]>> {
    control.check_cancelled()?;
    validate_source(source)?;
    let mut file = File::open(&source.path)
        .with_context(|| format!("无法打开源文件 {}", source.path.display()))?;
    let mut buffer = [0; IO_BUFFER_SIZE];
    let mut remaining = source.size;
    let segment_size = source.size.div_ceil(SEVENZIP_SIMILAR_SEGMENTS as u64);
    let mut segments = Box::new([0; SEVENZIP_SIMILAR_SEGMENTS]);
    for segment in segments.iter_mut() {
        let mut hasher = crc32fast::Hasher::new();
        let mut segment_remaining = remaining.min(segment_size);
        while segment_remaining > 0 {
            control.check_cancelled()?;
            let length = segment_remaining.min(buffer.len() as u64) as usize;
            file.read_exact(&mut buffer[..length])
                .with_context(|| format!("无法读取源文件 {}", source.path.display()))?;
            hasher.update(&buffer[..length]);
            segment_remaining -= length as u64;
            remaining -= length as u64;
        }
        *segment = hasher.finalize();
    }
    validate_source(source)?;
    Ok(segments)
}

fn sevenzip_pe_candidate(source: &Source) -> bool {
    if source.is_directory || source.size < SEVENZIP_PE_MIN_FILE {
        return false;
    }
    let Some(extension) = source.path.extension().and_then(|value| value.to_str()) else {
        return true;
    };
    [
        "exe", "dll", "sys", "ocx", "cpl", "scr", "efi", "drv", "pyd", "node", "ax", "com", "bin",
    ]
    .iter()
    .any(|candidate| extension.eq_ignore_ascii_case(candidate))
}

fn sevenzip_pe_filter(header: &[u8]) -> Option<EncoderMethod> {
    if header.get(..2)? != b"MZ" {
        return None;
    }
    let offset = u32::from_le_bytes(header.get(0x3c..0x40)?.try_into().ok()?) as usize;
    if offset >= 4096 || offset % 8 != 0 || offset.checked_add(512)? > header.len() {
        return None;
    }
    let pe = header.get(offset..)?;
    if pe.get(..4)? != b"PE\0\0" {
        return None;
    }
    let machine = u16::from_le_bytes(pe.get(4..6)?.try_into().ok()?);
    let sections = u16::from_le_bytes(pe.get(6..8)?.try_into().ok()?);
    let optional_size = usize::from(u16::from_le_bytes(pe.get(20..22)?.try_into().ok()?));
    let magic = u16::from_le_bytes(pe.get(24..26)?.try_into().ok()?);
    if !(2..=1024).contains(&optional_size) || !matches!(magic, 0x10b | 0x20b) {
        return None;
    }
    match machine {
        0x014c => Some(EncoderMethod::BCJ_X86_FILTER),
        0x8664 => {
            if sections <= 64 {
                let first = 24 + optional_size;
                for index in 0..usize::from(sections) {
                    let start = first + index * 40;
                    let Some(name) = pe.get(start..start + 8) else {
                        break;
                    };
                    if name == b".a64xrm\0" {
                        return Some(EncoderMethod::BCJ_ARM64_FILTER);
                    }
                }
            }
            Some(EncoderMethod::BCJ_X86_FILTER)
        }
        0xaa64 => Some(EncoderMethod::BCJ_ARM64_FILTER),
        _ => None,
    }
}

fn sevenzip_hints(sources: &[Source], control: &JobControl) -> Result<Vec<SevenZipHint>> {
    let files = sources
        .iter()
        .enumerate()
        .filter(|(_, source)| !source.is_directory && source.size > 0)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let mut sample = vec![false; sources.len()];
    for (index, pair) in files.windows(2).enumerate() {
        if index % 256 == 0 {
            control.check_cancelled()?;
        }
        let size = sources[pair[0]].size;
        if (SEVENZIP_SAMPLE_MIN_FILE..=u64::from(SEVENZIP_DICTIONARY)).contains(&size)
            && size == sources[pair[1]].size
        {
            sample[pair[0]] = true;
            sample[pair[1]] = true;
        }
    }

    let probes = sources
        .iter()
        .zip(sample)
        .map(|(source, sample)| {
            control.check_cancelled()?;
            let pe_candidate = sevenzip_pe_candidate(source);
            if !sample && !pe_candidate {
                return Ok((None, None));
            }
            validate_source(source)?;
            let mut file = File::open(&source.path)
                .with_context(|| format!("无法打开源文件 {}", source.path.display()))?;
            let filter = if pe_candidate {
                let length = source.size.min(SEVENZIP_PE_HEADER_SIZE as u64) as usize;
                let mut header = vec![0; length];
                file.read_exact(&mut header)
                    .with_context(|| format!("无法读取源文件 {}", source.path.display()))?;
                sevenzip_pe_filter(&header)
            } else {
                None
            };
            let sample = if sample {
                let length = SEVENZIP_SAMPLE_SIZE as u64;
                let offsets = [0, (source.size - length) / 2, source.size - length];
                let mut samples = [0; 3];
                let mut buffer = [0; SEVENZIP_SAMPLE_SIZE];
                for (offset, hash) in offsets.into_iter().zip(&mut samples) {
                    control.check_cancelled()?;
                    file.seek(SeekFrom::Start(offset))
                        .with_context(|| format!("无法定位源文件 {}", source.path.display()))?;
                    file.read_exact(&mut buffer)
                        .with_context(|| format!("无法读取源文件 {}", source.path.display()))?;
                    *hash = crc32fast::hash(&buffer);
                }
                Some(SevenZipSample {
                    size: source.size,
                    samples,
                })
            } else {
                None
            };
            validate_source(source)?;
            Ok((filter, sample))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut confirm = vec![false; sources.len()];
    for (index, pair) in files.windows(2).enumerate() {
        if index % 256 == 0 {
            control.check_cancelled()?;
        }
        let (left_filter, left_sample) = probes[pair[0]];
        let (right_filter, right_sample) = probes[pair[1]];
        if left_sample.is_some() && left_sample == right_sample && left_filter == right_filter {
            confirm[pair[0]] = true;
            confirm[pair[1]] = true;
        }
    }
    sources
        .iter()
        .zip(probes)
        .zip(confirm)
        .map(|((source, (filter, sample)), confirm)| {
            control.check_cancelled()?;
            let similarity = match (confirm, sample) {
                (true, Some(sample)) => Some(SevenZipSimilarity {
                    sample,
                    segments: sevenzip_fingerprint(source, control)?,
                }),
                _ => None,
            };
            Ok(SevenZipHint { filter, similarity })
        })
        .collect()
}

/// LZMA2 options whose dictionary is no larger than the data, which saves memory.
fn sevenzip_lzma(data: u64) -> Lzma2Options {
    let mut options = Lzma2Options::from_level(SEVENZIP_PRESET);
    options.set_dictionary_size(sevenzip_dictionary(data));
    options
}

fn sevenzip_similar_lzma(file_size: u64) -> Lzma2Options {
    let mut options = Lzma2Options::from_level(SEVENZIP_SIMILAR_PRESET);
    options.set_dictionary_size(sevenzip_dictionary(file_size));
    options
}

fn sevenzip_dictionary(data: u64) -> u32 {
    let data = data.clamp(64 * 1024, u64::from(SEVENZIP_DICTIONARY));
    u32::try_from(data.next_power_of_two())
        .unwrap_or(SEVENZIP_DICTIONARY)
        .min(SEVENZIP_DICTIONARY)
}

fn sevenzip_block_size(sources: &[Source], threads: usize) -> u64 {
    let total = sources.iter().map(|source| source.size).sum::<u64>();
    total
        .div_ceil(threads.max(1) as u64)
        .clamp(SEVENZIP_MIN_BLOCK, SEVENZIP_MAX_BLOCK)
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum SevenZipTask {
    /// A folder or empty file, which has no data stream.
    Empty(usize),
    /// Non-empty files compressed into one solid block by a worker.
    Block(Vec<usize>),
    /// Similar neighboring files share a dictionary without internal chunk resets.
    Similar(Vec<usize>),
    /// One file of at least a block, too small to keep every thread busy alone. A worker
    /// compresses it with one encoder thread per chunk, alongside other tasks.
    Single(usize),
    /// One large file streamed by the multi-threaded encoder on the merging thread.
    Large(usize),
}

/// Large files run first; other files form bounded blocks with a common filter.
fn sevenzip_tasks(
    sources: &[Source],
    hints: &[SevenZipHint],
    block_size: u64,
    stream_size: u64,
) -> Vec<SevenZipTask> {
    let empty = |source: &Source| source.is_directory || source.size == 0;
    let files = sources
        .iter()
        .enumerate()
        .filter(|(_, source)| !empty(source))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let mut grouped = vec![false; sources.len()];
    let mut similar = Vec::new();
    let mut first = 0;
    while first < files.len() {
        let index = files[first];
        let Some(signature) = hints[index].similarity.as_ref() else {
            first += 1;
            continue;
        };
        let filter = hints[index].filter;
        let mut end = first + 1;
        while end < files.len()
            && hints[files[end]]
                .similarity
                .as_ref()
                .is_some_and(|other| signature.matches(other))
            && hints[files[end]].filter == filter
        {
            end += 1;
        }
        while first < end {
            let mut group = Vec::new();
            let mut bytes = 0u64;
            while first < end {
                let index = files[first];
                if !group.is_empty()
                    && bytes.saturating_add(sources[index].size) > SEVENZIP_MAX_BLOCK
                {
                    break;
                }
                bytes += sources[index].size;
                group.push(index);
                first += 1;
            }
            if group.len() > 1 {
                for &index in &group {
                    grouped[index] = true;
                }
                similar.push(SevenZipTask::Similar(group));
            }
        }
    }
    let mut tasks = sources
        .iter()
        .enumerate()
        .filter(|(_, source)| empty(source))
        .map(|(index, _)| SevenZipTask::Empty(index))
        .collect::<Vec<_>>();
    let alone = |index: usize, source: &Source| {
        !empty(source) && !grouped[index] && source.size >= block_size
    };
    tasks.extend(
        sources
            .iter()
            .enumerate()
            .filter(|(index, source)| alone(*index, source) && source.size >= stream_size)
            .map(|(index, _)| SevenZipTask::Large(index)),
    );
    tasks.extend(
        sources
            .iter()
            .enumerate()
            .filter(|(index, source)| alone(*index, source) && source.size < stream_size)
            .map(|(index, _)| SevenZipTask::Single(index)),
    );
    tasks.extend(similar);
    let mut group: Vec<usize> = Vec::new();
    let mut bytes = 0u64;
    for (index, source) in sources.iter().enumerate() {
        if empty(source) || grouped[index] || source.size >= block_size {
            continue;
        }
        if !group.is_empty()
            && (bytes + source.size > block_size || hints[group[0]].filter != hints[index].filter)
        {
            tasks.push(SevenZipTask::Block(std::mem::take(&mut group)));
            bytes = 0;
        }
        group.push(index);
        bytes += source.size;
    }
    if !group.is_empty() {
        tasks.push(SevenZipTask::Block(group));
    }
    tasks
}

#[cfg(windows)]
fn available_memory() -> Option<u64> {
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

    let mut status = MEMORYSTATUSEX {
        dwLength: size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: `status` is a valid, writable MEMORYSTATUSEX with its length set.
    unsafe { GlobalMemoryStatusEx(&mut status) }.ok()?;
    Some(status.ullAvailPhys)
}

#[cfg(not(windows))]
fn available_memory() -> Option<u64> {
    None
}

/// All logical processors, limited so the encoders fit in 3/4 of the available memory.
fn sevenzip_threads() -> Result<usize> {
    let processors = std::thread::available_parallelism()
        .context("无法获取可用处理器数量")?
        .get();
    let mut encoder = lzma_rust2::LzmaOptions::with_preset(SEVENZIP_PRESET);
    encoder.dict_size = SEVENZIP_DICTIONARY;
    let encoder = u64::from(encoder.get_memory_usage()) * 1024;
    // Input is streamed; each thread holds up to one compressed block in memory.
    let per_thread = encoder + SEVENZIP_MAX_BLOCK;
    Ok(match available_memory() {
        Some(memory) => processors.min(
            usize::try_from(memory / 4 * 3 / per_thread)
                .unwrap_or(usize::MAX)
                .max(1),
        ),
        None => processors,
    })
}

/// Chunks as large as possible (better ratio) while still giving every thread work, but
/// never smaller than the dictionary.
fn sevenzip_chunk_size(size: u64, threads: usize) -> u64 {
    const MIB: u64 = 1024 * 1024;
    size.div_ceil(threads as u64)
        .div_ceil(MIB)
        .saturating_mul(MIB)
        .clamp(u64::from(SEVENZIP_DICTIONARY), SEVENZIP_MAX_BLOCK)
}

/// Files from this size can keep every thread busy on their own, so they are streamed.
fn sevenzip_stream_size(threads: usize) -> u64 {
    threads as u64 * u64::from(SEVENZIP_DICTIONARY)
}

/// Encoder threads for `bytes` of a block or file, one per chunk of about `chunk` bytes, and
/// the chunk size that splits it evenly between them.
fn sevenzip_task_chunks(bytes: u64, chunk: u64, threads: usize) -> (usize, u64) {
    const MIB: u64 = 1024 * 1024;
    let chunk = chunk.max(1);
    let count = (bytes + chunk / 2) / chunk;
    let count = usize::try_from(count.min(bytes / u64::from(SEVENZIP_DICTIONARY)))
        .unwrap_or(usize::MAX)
        .clamp(1, threads.max(1));
    let size = bytes
        .div_ceil(count as u64)
        .div_ceil(MIB)
        .saturating_mul(MIB)
        .max(u64::from(SEVENZIP_DICTIONARY));
    (count, size)
}

struct SevenZipShared<'a, 'b, 'c> {
    sources: &'a [Source],
    hints: &'a [SevenZipHint],
    password: Option<&'a str>,
    control: &'a JobControl,
    tracker: &'a Mutex<&'b mut Tracker<'c>>,
    threads: usize,
    /// Target LZMA2 chunk size of blocks and single files, so their chunks spread evenly
    /// over every thread.
    chunk: u64,
}

impl SevenZipShared<'_, '_, '_> {
    fn task_bytes(&self, task: &SevenZipTask) -> u64 {
        match task {
            SevenZipTask::Block(block) | SevenZipTask::Similar(block) => {
                block.iter().map(|&index| self.sources[index].size).sum()
            }
            &SevenZipTask::Single(index) => self.sources[index].size,
            _ => 0,
        }
    }

    /// Threads a task keeps busy while a worker runs it.
    fn task_threads(&self, task: &SevenZipTask) -> usize {
        match task {
            SevenZipTask::Empty(_) => 0,
            SevenZipTask::Block(_) | SevenZipTask::Single(_) => {
                sevenzip_task_chunks(self.task_bytes(task), self.chunk, self.threads).0
            }
            SevenZipTask::Similar(_) => 1,
            SevenZipTask::Large(_) => self.threads,
        }
    }

    fn task_lzma(&self, bytes: u64) -> Lzma2Options {
        let (threads, chunk) = sevenzip_task_chunks(bytes, self.chunk, self.threads);
        if threads < 2 {
            return sevenzip_lzma(bytes);
        }
        let mut lzma = Lzma2Options::from_level_mt(SEVENZIP_PRESET, threads as u32, chunk);
        lzma.set_dictionary_size(SEVENZIP_DICTIONARY);
        lzma
    }
}

fn write_sevenzip(
    temporary: &mut NamedTempFile,
    sources: &[Source],
    options: &CreateOptions,
    control: &JobControl,
    tracker: &mut Tracker<'_>,
) -> Result<()> {
    let tracker = Mutex::new(tracker);
    let output = BufWriter::with_capacity(IO_BUFFER_SIZE, temporary.as_file_mut());
    let mut writer = ArchiveWriter::new(ArchiveSizeWriter::new(output, &tracker))
        .context("无法创建 7z 压缩包")?;
    let password = options.password.as_deref().filter(|p| !p.is_empty());
    let threads = sevenzip_threads()?;
    let block_size = sevenzip_block_size(sources, threads);
    let hints = sevenzip_hints(sources, control)?;
    let tasks = sevenzip_tasks(sources, &hints, block_size, sevenzip_stream_size(threads));
    let blocks = tasks
        .iter()
        .filter(|task| {
            matches!(
                task,
                SevenZipTask::Block(_) | SevenZipTask::Similar(_) | SevenZipTask::Single(_)
            )
        })
        .count();
    let mut shared = SevenZipShared {
        sources,
        hints: &hints,
        password,
        control,
        tracker: &tracker,
        threads,
        chunk: 0,
    };
    let worker_bytes = tasks
        .iter()
        .filter(|task| !matches!(task, SevenZipTask::Similar(_)))
        .map(|task| shared.task_bytes(task))
        .sum();
    shared.chunk = sevenzip_chunk_size(worker_bytes, threads);
    let workers = threads.min(blocks);
    if workers > 1 {
        write_sevenzip_parallel(&mut writer, &tasks, &shared, threads, workers, block_size)?;
    } else {
        for task in &tasks {
            control.check_cancelled()?;
            write_sevenzip_task(&mut writer, task, &shared, threads, None)?;
        }
    }
    control.check_cancelled()?;
    // A fresh method list also gives an encrypted header its own salt and IV.
    configure_sevenzip_header(&mut writer, password);
    writer
        .finish()
        .context("无法完成 7z 压缩包")?
        .flush()
        .context("无法写入 7z 压缩包")?;
    Ok(())
}

pub(super) fn configure_sevenzip_header<W: Write + Seek>(
    writer: &mut ArchiveWriter<W>,
    password: Option<&str>,
) {
    writer.set_content_methods(sevenzip_methods(password, sevenzip_lzma(1024 * 1024), None));
}

fn write_sevenzip_parallel<W: Write + Seek>(
    writer: &mut ArchiveWriter<W>,
    tasks: &[SevenZipTask],
    shared: &SevenZipShared<'_, '_, '_>,
    threads: usize,
    workers: usize,
    block_size: u64,
) -> Result<()> {
    let pipeline = PipelineState::new("7z");
    let window = (workers * TASKS_AHEAD_PER_WORKER).min(MAX_TASKS_AHEAD);
    // Finished blocks stay in memory until merged, so bound the input bytes in flight. Each
    // encoder thread, including those of single files, is budgeted two blocks.
    let budget = 2 * threads.max(workers) as u64 * block_size;
    let mut offsets = Vec::with_capacity(tasks.len() + 1);
    offsets.push(0u64);
    for task in tasks {
        let bytes = match task {
            SevenZipTask::Block(block) | SevenZipTask::Similar(block) => {
                block.iter().map(|&index| shared.sources[index].size).sum()
            }
            &SevenZipTask::Single(index) => shared.sources[index].size,
            _ => 0,
        };
        offsets.push(offsets.last().copied().unwrap_or(0) + bytes);
    }
    let large = tasks
        .iter()
        .enumerate()
        .filter(|(_, task)| matches!(task, SevenZipTask::Large(_)))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let blocked = |state: &Pipeline<PreparedBlock>| {
        let next = state.next_task;
        // A large file uses every thread, so later tasks wait until it is written.
        let barrier = large.get(large.partition_point(|&index| index < state.merged));
        if barrier.is_some_and(|&barrier| next > barrier) || next >= state.merged + window {
            return true;
        }
        let in_flight = offsets[next.max(state.merged)] - offsets[state.merged];
        if in_flight > 0 && in_flight + (offsets[next + 1] - offsets[next]) > budget {
            return true;
        }
        // Single files run their own encoder threads, so keep the total near the thread count.
        let busy = (state.merged..next)
            .filter(|index| !state.ready.contains_key(index))
            .map(|index| shared.task_threads(&tasks[index]))
            .sum::<usize>();
        busy > 0 && busy + shared.task_threads(&tasks[next]) > threads
    };
    std::thread::scope(|scope| {
        let _stop = StopOnDrop(&pipeline);
        for _ in 0..workers {
            let pipeline = &pipeline;
            let blocked = &blocked;
            std::thread::Builder::new()
                .name("hx6zip-7z-worker".to_owned())
                .spawn_scoped(scope, move || {
                    pipeline_worker(pipeline, tasks.len(), blocked, |index| {
                        match &tasks[index] {
                            task @ (SevenZipTask::Block(_)
                            | SevenZipTask::Similar(_)
                            | SevenZipTask::Single(_)) => Some(prepare_sevenzip_task(task, shared)),
                            _ => None,
                        }
                    })
                })
                .context("无法启动 7z 压缩线程")?;
        }
        for (index, task) in tasks.iter().enumerate() {
            let block = match task {
                SevenZipTask::Block(_) | SevenZipTask::Similar(_) | SevenZipTask::Single(_) => {
                    Some(pipeline.take(index)?)
                }
                _ => None,
            };
            shared.control.check_cancelled()?;
            write_sevenzip_task(writer, task, shared, threads, block)?;
            pipeline.finish_task(index)?;
        }
        Ok(())
    })
}

/// Writes `task` in archive order, compressing a block here unless a worker prepared it.
fn write_sevenzip_task<W: Write + Seek>(
    writer: &mut ArchiveWriter<W>,
    task: &SevenZipTask,
    shared: &SevenZipShared<'_, '_, '_>,
    threads: usize,
    block: Option<PreparedBlock>,
) -> Result<()> {
    match task {
        &SevenZipTask::Empty(index) => {
            let source = &shared.sources[index];
            start_source(source, shared.tracker)?;
            writer
                .push_archive_entry(sevenzip_entry(source)?, None::<io::Empty>)
                .with_context(|| format!("无法添加 {}", source.name))?;
            validate_source(source)
        }
        SevenZipTask::Block(_) | SevenZipTask::Similar(_) | SevenZipTask::Single(_) => {
            let block = match block {
                Some(block) => block,
                None => prepare_sevenzip_task(task, shared)?,
            };
            shared.control.check_cancelled()?;
            writer
                .push_prepared_block(block)
                .context("无法合并 7z 条目")?;
            Ok(())
        }
        &SevenZipTask::Large(index) => write_large_sevenzip(writer, index, shared, threads),
    }
}

fn write_large_sevenzip<W: Write + Seek>(
    writer: &mut ArchiveWriter<W>,
    index: usize,
    shared: &SevenZipShared<'_, '_, '_>,
    threads: usize,
) -> Result<()> {
    let source = &shared.sources[index];
    start_source(source, shared.tracker)?;
    let lzma = if threads > 1 {
        let mut lzma = Lzma2Options::from_level_mt(
            SEVENZIP_PRESET,
            threads as u32,
            sevenzip_chunk_size(source.size, threads),
        );
        lzma.set_dictionary_size(SEVENZIP_DICTIONARY);
        lzma
    } else {
        sevenzip_lzma(source.size)
    };
    // New methods for every file also give each encrypted file its own salt and IV.
    writer.set_content_methods(sevenzip_methods(
        shared.password,
        lzma,
        shared.hints[index].filter,
    ));
    let entry = sevenzip_entry(source)?;
    let file = File::open(&source.path)
        .with_context(|| format!("无法打开源文件 {}", source.path.display()))?;
    let reader = TrackingReader {
        file: BufReader::with_capacity(IO_BUFFER_SIZE, file),
        expected: source.size,
        read: 0,
        control: shared.control,
        advance: |bytes| advance_tracker(shared.tracker, bytes),
    };
    let result = writer.push_archive_entry(entry, Some(reader));
    if shared.control.cancellation.is_cancelled() {
        return Err(Cancelled.into());
    }
    result.with_context(|| format!("无法压缩 {}", source.name))?;
    validate_source(source)
}

fn sevenzip_methods(
    password: Option<&str>,
    lzma: Lzma2Options,
    filter: Option<EncoderMethod>,
) -> Vec<EncoderConfiguration> {
    let mut methods = Vec::new();
    if let Some(password) = password {
        methods.push(AesEncoderOptions::new(Password::from(password)).into());
    }
    methods.push(lzma.into());
    if let Some(filter) = filter {
        methods.push(EncoderConfiguration::new(filter));
    }
    methods
}

fn sevenzip_entry(source: &Source) -> Result<SevenZipEntry> {
    let mut entry = SevenZipEntry::from_path(&source.path, source.name.clone());
    entry.is_directory = source.is_directory;
    entry.has_stream = !source.is_directory;
    let modified = source
        .metadata
        .modified()
        .with_context(|| format!("无法读取 {} 的修改时间", source.path.display()))?;
    entry.last_modified_date = modified.try_into().map_err(|error| {
        anyhow::anyhow!(
            "无法保存 {} 的修改时间（超出 7z 支持范围：{error:?}）",
            source.name
        )
    })?;
    entry.has_last_modified_date = true;
    Ok(entry)
}

/// Compresses a solid block, or a single file with one encoder thread per chunk.
fn prepare_sevenzip_task(
    task: &SevenZipTask,
    shared: &SevenZipShared<'_, '_, '_>,
) -> Result<PreparedBlock> {
    match task {
        SevenZipTask::Block(block) => {
            prepare_sevenzip_block(block, shared, shared.task_lzma(shared.task_bytes(task)))
        }
        SevenZipTask::Similar(block) => prepare_sevenzip_block(
            block,
            shared,
            sevenzip_similar_lzma(shared.sources[block[0]].size),
        ),
        &SevenZipTask::Single(index) => {
            prepare_sevenzip_block(&[index], shared, shared.task_lzma(shared.task_bytes(task)))
        }
        _ => Err(anyhow::anyhow!("7z 压缩任务状态异常")),
    }
}

/// Compresses non-empty files into one solid block. Each block gets its own
/// methods, so encrypted blocks never share a salt or IV.
fn prepare_sevenzip_block(
    block: &[usize],
    shared: &SevenZipShared<'_, '_, '_>,
    lzma: Lzma2Options,
) -> Result<PreparedBlock> {
    let sources = block
        .iter()
        .map(|&index| &shared.sources[index])
        .collect::<Vec<_>>();
    let entries = sources
        .iter()
        .copied()
        .map(sevenzip_entry)
        .collect::<Result<Vec<_>>>()?;
    let failure = RefCell::new(None);
    let readers = sources
        .iter()
        .map(|&source| {
            SourceReader::new(SevenZipSource {
                source,
                shared,
                failure: &failure,
                file: None,
                read: 0,
                done: false,
            })
        })
        .collect();
    let result = prepare_block(
        Arc::new(sevenzip_methods(
            shared.password,
            lzma,
            shared.hints[block[0]].filter,
        )),
        entries,
        readers,
    );
    if shared.control.cancellation.is_cancelled() {
        return Err(Cancelled.into());
    }
    if let Some(error) = failure.into_inner() {
        return Err(error);
    }
    result.with_context(|| match sources.as_slice() {
        [source] => format!("无法压缩 {}", source.name),
        _ => format!("无法压缩 {} 等 {} 个文件", sources[0].name, sources.len()),
    })
}

/// Opens its file only when the block reaches it, and validates it once fully read.
struct SevenZipSource<'a, 'b, 'c, 'd> {
    source: &'a Source,
    shared: &'a SevenZipShared<'b, 'c, 'd>,
    /// The original error, which the block encoder would otherwise only keep as text.
    failure: &'a RefCell<Option<anyhow::Error>>,
    file: Option<BufReader<File>>,
    read: u64,
    done: bool,
}

impl SevenZipSource<'_, '_, '_, '_> {
    fn fail(&self, error: anyhow::Error) -> io::Error {
        let message = format!("{error:#}");
        self.failure.borrow_mut().get_or_insert(error);
        io::Error::other(message)
    }
}

impl Read for SevenZipSource<'_, '_, '_, '_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.done {
            return Ok(0);
        }
        if self.shared.control.cancellation.is_cancelled() {
            return Err(io::Error::other("操作已取消"));
        }
        if self.file.is_none() {
            start_source(self.source, self.shared.tracker).map_err(|error| self.fail(error))?;
            let file = File::open(&self.source.path)
                .with_context(|| format!("无法打开源文件 {}", self.source.path.display()))
                .map_err(|error| self.fail(error))?;
            self.file = Some(BufReader::with_capacity(IO_BUFFER_SIZE, file));
        }
        let count = self.file.as_mut().expect("opened source").read(buffer)?;
        self.read = self
            .read
            .checked_add(count as u64)
            .ok_or_else(|| io::Error::other("文件大小溢出"))?;
        if self.read > self.source.size || (count == 0 && self.read != self.source.size) {
            return Err(io::Error::other("文件在压缩过程中发生变化"));
        }
        advance_tracker(self.shared.tracker, count).map_err(|error| self.fail(error))?;
        if count == 0 {
            self.done = true;
            self.file = None;
            validate_source(self.source).map_err(|error| self.fail(error))?;
        }
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tar_creation_round_trips_files_and_directories() {
        let temp = tempfile::TempDir::new().unwrap();
        let input = temp.path().join("input");
        fs::create_dir_all(input.join("empty")).unwrap();
        fs::write(input.join("资料.txt"), b"tar contents").unwrap();
        let long_name = "l".repeat(120);
        fs::write(input.join(&long_name), b"long path").unwrap();
        let destination = temp.path().join("sample.tar");

        create_archive(
            &CreateOptions {
                sources: vec![input.clone()],
                destination: destination.clone(),
                format: ArchiveFormat::Tar,
                password: None,
                overwrite: false,
            },
            &JobControl::default().with_integrity(IntegrityMode::Safe),
        )
        .unwrap();

        let info =
            super::super::inspect_archive(&destination, None, &JobControl::default()).unwrap();
        assert!(info.entries.iter().any(|entry| {
            entry.name == "input/资料.txt" && entry.size == b"tar contents".len() as u64
        }));
        assert!(
            info.entries
                .iter()
                .any(|entry| entry.name == "input/empty" && entry.is_directory)
        );
        assert!(
            info.entries
                .iter()
                .any(|entry| entry.name == format!("input/{long_name}"))
        );
    }

    #[test]
    fn archive_size_tracks_written_extent_across_seeks() {
        let reports = Arc::new(Mutex::new(Vec::new()));
        let captured = reports.clone();
        let control = JobControl::new(Default::default(), move |progress| {
            captured.lock().unwrap().push(progress.archive_size);
        });
        let mut progress = Tracker::new(&control, "正在压缩", 0);
        progress.set_archive_size(0);
        let tracker = Mutex::new(&mut progress);
        let mut writer = ArchiveSizeWriter::new(Cursor::new(Vec::new()), &tracker);

        writer.write_all(b"abcdefgh").unwrap();
        writer.flush().unwrap();
        tracker.lock().unwrap().finish().unwrap();

        writer.seek(SeekFrom::Start(2)).unwrap();
        writer.write_all(b"XY").unwrap();
        writer.flush().unwrap();
        tracker.lock().unwrap().finish().unwrap();

        writer.seek(SeekFrom::Start(20)).unwrap();
        writer.flush().unwrap();
        tracker.lock().unwrap().finish().unwrap();

        writer.write_all(b"z").unwrap();
        writer.flush().unwrap();
        tracker.lock().unwrap().finish().unwrap();
        assert_eq!(writer.inner.get_ref().len(), 21);
        assert_eq!(
            *reports.lock().unwrap(),
            [Some(8), Some(8), Some(8), Some(21)]
        );
    }

    #[test]
    fn querying_output_position_does_not_flush_the_archive_buffer() {
        let control = JobControl::default();
        let mut progress = Tracker::new(&control, "正在压缩", 0);
        let tracker = Mutex::new(&mut progress);
        let buffer = BufWriter::with_capacity(64, Cursor::new(Vec::new()));
        let mut writer = ArchiveSizeWriter::new(buffer, &tracker);

        writer.write_all(b"buffered").unwrap();
        assert_eq!(writer.stream_position().unwrap(), 8);
        assert!(writer.inner.get_ref().get_ref().is_empty());
        writer.flush().unwrap();
        assert_eq!(writer.inner.get_ref().get_ref(), b"buffered");
    }

    #[test]
    fn archive_writes_report_progress_even_when_input_is_not_advancing() {
        let reports = Arc::new(Mutex::new(Vec::new()));
        let captured = reports.clone();
        let control = JobControl::new(Default::default(), move |progress| {
            captured.lock().unwrap().push(progress);
        });
        let mut progress = Tracker::new(&control, "正在压缩", 100);
        progress.set_archive_size(0);
        progress.start_file("source").unwrap();
        let tracker = Mutex::new(&mut progress);
        let mut writer = ArchiveSizeWriter::new(Cursor::new(Vec::new()), &tracker);

        std::thread::sleep(Duration::from_millis(120));
        writer.write_all(b"encoded").unwrap();
        let reports = reports.lock().unwrap();
        let update = reports.last().unwrap();
        assert_eq!(update.completed_bytes, 0);
        assert_eq!(update.archive_size, Some(7));
    }

    #[test]
    fn zip_tasks_group_small_entries_and_split_large_files() {
        let temp = tempfile::TempDir::new().unwrap();
        let metadata = fs::metadata(temp.path()).unwrap();
        let source = |size: u64, is_directory: bool| Source {
            path: temp.path().to_path_buf(),
            name: String::new(),
            size,
            is_directory,
            metadata: metadata.clone(),
        };
        let small = PARALLEL_MIN_FILE_SIZE - 1;
        let mut sources = vec![
            source(0, true),
            source(small, false),
            source(PARALLEL_MIN_FILE_SIZE, false),
            source(small, false),
            source(small, false),
            source(small, false),
            source(small, false),
            source(small, false),
        ];
        sources.extend((0..GROUP_MAX_ENTRIES + 1).map(|_| source(0, true)));
        let end = sources.len();
        let entries = |range: Range<usize>| ZipTask::Entries(range);
        assert_eq!(
            zip_tasks(&sources, true),
            [
                entries(0..2),
                entries(2..3),
                entries(3..7),
                entries(7..8 + GROUP_MAX_ENTRIES - 1),
                entries(8 + GROUP_MAX_ENTRIES - 1..end),
            ]
        );
        assert!(zip_tasks(&[], true).is_empty());

        let chunked = ZIP_CHUNKED_MIN_FILE_SIZE + 1;
        let sources = [
            source(1, false),
            source(chunked, false),
            source(ZIP_CHUNKED_MIN_FILE_SIZE - 1, false),
            source(ZIP_CHUNKED_MAX_FILE_SIZE, false),
        ];
        let mut expected = vec![entries(0..1)];
        let mut offset = 0;
        while offset < chunked {
            let len = ZIP_CHUNK_SIZE.min(chunked - offset);
            expected.push(ZipTask::Chunk {
                source: 1,
                offset,
                len,
            });
            offset += len;
        }
        expected.extend([entries(2..3), entries(3..4)]);
        assert_eq!(zip_tasks(&sources, true), expected);
        assert_eq!(
            zip_tasks(&sources, false),
            [entries(0..1), entries(1..2), entries(2..3), entries(3..4)]
        );
    }

    #[test]
    fn chunked_zip_entries_round_trip_with_metadata() {
        let temp = tempfile::TempDir::new().unwrap();
        let input = temp.path().join("input");
        fs::create_dir(&input).unwrap();
        let large_size = (ZIP_CHUNKED_MIN_FILE_SIZE + ZIP_CHUNK_SIZE / 2 + 7) as usize;
        // Incompressible data leaves large pending output when a chunk is flushed.
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let large = (0..large_size)
            .map(|offset| {
                if offset < large_size / 2 {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    state as u8
                } else {
                    (offset / 4096 % 7) as u8 ^ (offset % 251) as u8
                }
            })
            .collect::<Vec<_>>();
        fs::write(input.join("large-大文件.bin"), &large).unwrap();
        fs::write(input.join("small.txt"), b"small").unwrap();
        fs::create_dir(input.join("empty")).unwrap();
        let destination = temp.path().join("chunked.zip");
        create_archive(
            &CreateOptions {
                sources: vec![input.clone()],
                destination: destination.clone(),
                format: ArchiveFormat::Zip,
                password: None,
                overwrite: false,
            },
            &JobControl::default(),
        )
        .unwrap();
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 2);

        let mut archive = ZipArchive::new(File::open(&destination).unwrap()).unwrap();
        let names = archive.file_names().map(str::to_owned).collect::<Vec<_>>();
        assert!(names.contains(&"input/large-大文件.bin".to_owned()));
        let mut entry = archive.by_name("input/large-大文件.bin").unwrap();
        assert_eq!(entry.compression(), CompressionMethod::Deflated);
        assert_eq!(entry.size(), large_size as u64);
        assert!(entry.compressed_size() < large_size as u64);
        let expected_time = write_zip_times(
            &mut FullFileOptions::default(),
            &fs::metadata(input.join("large-大文件.bin")).unwrap(),
        )
        .unwrap();
        assert_eq!(entry.last_modified(), Some(expected_time));
        let mut extracted = Vec::new();
        io::copy(&mut entry, &mut extracted).unwrap();
        assert_eq!(extracted, large);
        drop(entry);

        let data_start = archive
            .by_name("input/large-大文件.bin")
            .unwrap()
            .data_start()
            .unwrap();
        let compressed_size = archive
            .by_name("input/large-大文件.bin")
            .unwrap()
            .compressed_size();
        let bytes = fs::read(&destination).unwrap();
        let raw = &bytes[data_start as usize..(data_start + compressed_size) as usize];
        let mut independent = Vec::new();
        io::copy(
            &mut flate2::read::DeflateDecoder::new(raw),
            &mut independent,
        )
        .unwrap();
        assert_eq!(independent, large);
        let mut small = String::new();
        io::Read::read_to_string(&mut archive.by_name("input/small.txt").unwrap(), &mut small)
            .unwrap();
        assert_eq!(small, "small");
        assert!(archive.by_name("input/empty/").unwrap().is_dir());
    }

    #[test]
    fn sevenzip_tasks_fill_solid_blocks_across_folders_and_isolate_large_files() {
        let temp = tempfile::TempDir::new().unwrap();
        let metadata = fs::metadata(temp.path()).unwrap();
        let source = |size: u64, is_directory: bool| Source {
            path: temp.path().to_path_buf(),
            name: String::new(),
            size,
            is_directory,
            metadata: metadata.clone(),
        };
        let block = 100;
        let sources = vec![
            source(0, true),
            source(1, false),
            source(99, false),
            source(0, false),
            source(40, false),
            source(60, false),
            source(1, false),
            source(block, false),
            source(block - 1, false),
            source(2, false),
            source(3 * block, false),
        ];
        use SevenZipTask::{Block, Empty, Large, Single};
        let hints = vec![SevenZipHint::default(); sources.len()];
        assert_eq!(
            sevenzip_tasks(&sources, &hints, block, 3 * block),
            [
                Empty(0),
                Empty(3),
                Large(10),
                Single(7),
                Block(vec![1, 2]),
                Block(vec![4, 5]),
                Block(vec![6, 8]),
                Block(vec![9]),
            ]
        );
        assert!(sevenzip_tasks(&[], &[], block, 3 * block).is_empty());
        assert_eq!(sevenzip_block_size(&sources, 1), SEVENZIP_MIN_BLOCK);
        let huge = [source(u64::MAX / 2, false)];
        assert_eq!(sevenzip_block_size(&huge, 4), SEVENZIP_MAX_BLOCK);
        let medium = [source(12 * SEVENZIP_MIN_BLOCK, false)];
        assert_eq!(sevenzip_block_size(&medium, 4), 3 * SEVENZIP_MIN_BLOCK);
    }

    #[test]
    fn sevenzip_detects_supported_pe_filters_from_valid_headers() {
        let mut header = vec![0; 640];
        header[..2].copy_from_slice(b"MZ");
        header[0x3c..0x40].copy_from_slice(&128u32.to_le_bytes());
        header[128..132].copy_from_slice(b"PE\0\0");
        header[132..134].copy_from_slice(&0x8664u16.to_le_bytes());
        header[134..136].copy_from_slice(&1u16.to_le_bytes());
        header[148..150].copy_from_slice(&240u16.to_le_bytes());
        header[152..154].copy_from_slice(&0x20bu16.to_le_bytes());

        assert_eq!(
            sevenzip_pe_filter(&header),
            Some(EncoderMethod::BCJ_X86_FILTER)
        );
        header[392..400].copy_from_slice(b".a64xrm\0");
        assert_eq!(
            sevenzip_pe_filter(&header),
            Some(EncoderMethod::BCJ_ARM64_FILTER)
        );
        header[132..134].copy_from_slice(&0xaa64u16.to_le_bytes());
        assert_eq!(
            sevenzip_pe_filter(&header),
            Some(EncoderMethod::BCJ_ARM64_FILTER)
        );
        header[132..134].copy_from_slice(&0x1234u16.to_le_bytes());
        assert_eq!(sevenzip_pe_filter(&header), None);
        assert_eq!(sevenzip_pe_filter(&header[..200]), None);
        header[128] = 0;
        assert_eq!(sevenzip_pe_filter(&header), None);
    }

    #[test]
    fn sevenzip_streaming_tasks_preserve_filters_and_encryption() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("stream.exe");
        let contents = (0..256 * 1024)
            .map(|index| (index * 37) as u8)
            .collect::<Vec<_>>();
        fs::write(&path, &contents).unwrap();
        let metadata = fs::metadata(&path).unwrap();
        let sources = [Source {
            path,
            name: "stream.exe".to_owned(),
            size: metadata.len(),
            is_directory: false,
            metadata,
        }];
        for (filter, method_id) in [
            (EncoderMethod::BCJ_X86_FILTER, EncoderMethod::ID_BCJ_X86),
            (EncoderMethod::BCJ_ARM64_FILTER, EncoderMethod::ID_BCJ_ARM64),
        ] {
            for password in [None, Some("stream-password")] {
                let hints = [SevenZipHint {
                    filter: Some(filter),
                    similarity: None,
                }];
                let control = JobControl::default();
                let mut progress = Tracker::new(&control, "正在压缩", sources[0].size);
                let tracker = Mutex::new(&mut progress);
                let shared = SevenZipShared {
                    sources: &sources,
                    hints: &hints,
                    password,
                    control: &control,
                    tracker: &tracker,
                    threads: 2,
                    chunk: 0,
                };
                let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).unwrap();
                write_sevenzip_task(&mut writer, &SevenZipTask::Large(0), &shared, 2, None)
                    .unwrap();
                writer.set_content_methods(sevenzip_methods(
                    password,
                    sevenzip_lzma(1024 * 1024),
                    None,
                ));
                let mut archive = writer.finish().unwrap();
                archive.set_position(0);
                let mut reader = sevenz_rust2::ArchiveReader::new(
                    archive,
                    password.map(Password::from).unwrap_or_default(),
                )
                .unwrap();
                assert!(
                    reader.archive().blocks[0]
                        .coders
                        .iter()
                        .any(|coder| coder.encoder_method_id() == method_id)
                );
                assert_eq!(reader.read_file("stream.exe").unwrap(), contents);
            }
        }
    }

    #[test]
    fn sevenzip_groups_similar_files_and_separates_filters() {
        let temp = tempfile::TempDir::new().unwrap();
        let metadata = fs::metadata(temp.path()).unwrap();
        let source = |size| Source {
            path: temp.path().to_path_buf(),
            name: String::new(),
            size,
            is_directory: false,
            metadata: metadata.clone(),
        };
        let sources = vec![
            source(20),
            source(20),
            source(1),
            source(1),
            source(1),
            source(1),
        ];
        let mut hints = vec![SevenZipHint::default(); sources.len()];
        let signature = Some(SevenZipSimilarity {
            sample: SevenZipSample {
                size: 20,
                samples: [1, 2, 3],
            },
            segments: Box::new([4; SEVENZIP_SIMILAR_SEGMENTS]),
        });
        hints[0].similarity = signature.clone();
        hints[1].similarity = signature;
        hints[3].filter = Some(EncoderMethod::BCJ_X86_FILTER);
        hints[4].filter = Some(EncoderMethod::BCJ_X86_FILTER);
        assert_eq!(
            sevenzip_tasks(&sources, &hints, 10, 100),
            [
                SevenZipTask::Similar(vec![0, 1]),
                SevenZipTask::Block(vec![2]),
                SevenZipTask::Block(vec![3, 4]),
                SevenZipTask::Block(vec![5]),
            ]
        );
    }

    #[test]
    fn sevenzip_similar_groups_remain_bounded() {
        let temp = tempfile::TempDir::new().unwrap();
        let metadata = fs::metadata(temp.path()).unwrap();
        let size = u64::from(SEVENZIP_DICTIONARY);
        let sources = (0..5)
            .map(|_| Source {
                path: temp.path().to_path_buf(),
                name: String::new(),
                size,
                is_directory: false,
                metadata: metadata.clone(),
            })
            .collect::<Vec<_>>();
        let hints = vec![
            SevenZipHint {
                filter: None,
                similarity: Some(SevenZipSimilarity {
                    sample: SevenZipSample {
                        size,
                        samples: [1, 2, 3],
                    },
                    segments: Box::new([4; SEVENZIP_SIMILAR_SEGMENTS]),
                }),
            };
            sources.len()
        ];
        assert_eq!(
            sevenzip_tasks(&sources, &hints, SEVENZIP_MIN_BLOCK, u64::MAX),
            [
                SevenZipTask::Single(4),
                SevenZipTask::Similar(vec![0, 1, 2, 3]),
            ]
        );
    }

    #[test]
    fn sevenzip_confirms_matching_samples_with_full_content() {
        let temp = tempfile::TempDir::new().unwrap();
        let contents = vec![0; SEVENZIP_SAMPLE_MIN_FILE as usize];
        let mut changed = contents.clone();
        changed[10 * 1024] = 1;
        let mut unrelated = vec![2; contents.len()];
        let middle = (contents.len() - SEVENZIP_SAMPLE_SIZE) / 2;
        for offset in [0, middle, contents.len() - SEVENZIP_SAMPLE_SIZE] {
            unrelated[offset..offset + SEVENZIP_SAMPLE_SIZE].fill(0);
        }
        let mut different_sample = contents.clone();
        different_sample[0] = 1;
        let sources = [
            &contents,
            &contents,
            &changed,
            &changed,
            &unrelated,
            &different_sample,
        ]
        .into_iter()
        .enumerate()
        .map(|(index, contents)| {
            let path = temp.path().join(format!("{index}.dat"));
            fs::write(&path, contents).unwrap();
            let metadata = fs::metadata(&path).unwrap();
            Source {
                path,
                name: format!("{index}.dat"),
                size: metadata.len(),
                is_directory: false,
                metadata,
            }
        })
        .collect::<Vec<_>>();
        let hints = sevenzip_hints(&sources, &JobControl::default()).unwrap();
        let first = hints[0].similarity.as_ref().unwrap();
        assert_eq!(hints[0].similarity, hints[1].similarity);
        assert_ne!(hints[1].similarity, hints[2].similarity);
        assert_eq!(hints[2].similarity, hints[3].similarity);
        assert!(first.matches(hints[2].similarity.as_ref().unwrap()));
        assert!(!first.matches(hints[4].similarity.as_ref().unwrap()));
        assert_eq!(hints[5].similarity, None);
        assert_eq!(
            sevenzip_tasks(&sources, &hints, SEVENZIP_SAMPLE_MIN_FILE, u64::MAX),
            [
                SevenZipTask::Single(4),
                SevenZipTask::Single(5),
                SevenZipTask::Similar(vec![0, 1, 2, 3]),
            ]
        );
    }

    #[test]
    fn sevenzip_similarity_requires_ninety_percent_of_segments() {
        let original = SevenZipSimilarity {
            sample: SevenZipSample {
                size: SEVENZIP_SAMPLE_MIN_FILE,
                samples: [1, 2, 3],
            },
            segments: Box::new([0; SEVENZIP_SIMILAR_SEGMENTS]),
        };
        let mut changed = original.clone();
        let differences = SEVENZIP_SIMILAR_SEGMENTS - SEVENZIP_SIMILAR_MIN_MATCHES;
        changed.segments[..differences].fill(1);
        assert!(original.matches(&changed));
        changed.segments[differences] = 1;
        assert!(!original.matches(&changed));
    }

    #[test]
    fn sevenzip_fingerprint_reports_source_changes_and_cancellation() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("source.dat");
        fs::write(&path, vec![0; SEVENZIP_SAMPLE_MIN_FILE as usize]).unwrap();
        let metadata = fs::metadata(&path).unwrap();
        let source = Source {
            path: path.clone(),
            name: "source.dat".to_owned(),
            size: metadata.len(),
            is_directory: false,
            metadata,
        };
        let control = JobControl::default();
        assert!(sevenzip_fingerprint(&source, &control).is_ok());
        fs::write(&path, b"changed").unwrap();
        let error = sevenzip_fingerprint(&source, &control).unwrap_err();
        assert!(format!("{error:#}").contains("source.dat"), "{error:#}");
        control.cancellation.cancel();
        let error = sevenzip_fingerprint(&source, &control).unwrap_err();
        assert!(error.downcast_ref::<Cancelled>().is_some(), "{error:#}");
    }

    #[test]
    fn sevenzip_hint_read_errors_and_cancellation_are_reported() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("gone.exe");
        fs::write(&path, vec![0; SEVENZIP_PE_MIN_FILE as usize]).unwrap();
        let metadata = fs::metadata(&path).unwrap();
        fs::remove_file(&path).unwrap();
        let source = Source {
            path,
            name: "gone.exe".to_owned(),
            size: metadata.len(),
            is_directory: false,
            metadata,
        };
        let control = JobControl::default();
        let error = sevenzip_hints(std::slice::from_ref(&source), &control).unwrap_err();
        assert!(format!("{error:#}").contains("gone.exe"), "{error:#}");
        control.cancellation.cancel();
        let error = sevenzip_hints(&[source], &control).unwrap_err();
        assert!(error.downcast_ref::<Cancelled>().is_some(), "{error:#}");
    }

    #[test]
    fn sevenzip_dictionary_and_chunks_follow_data_size() {
        const MIB: u64 = 1024 * 1024;
        assert_eq!(sevenzip_dictionary(0), 64 * 1024);
        assert_eq!(sevenzip_dictionary(3 * MIB), 4 * 1024 * 1024);
        assert_eq!(sevenzip_dictionary(u64::MAX), SEVENZIP_DICTIONARY);
        let dictionary = u64::from(SEVENZIP_DICTIONARY);
        assert_eq!(sevenzip_chunk_size(16 * MIB, 16), dictionary);
        assert_eq!(sevenzip_chunk_size(300 * MIB, 8), 38 * MIB);
        assert_eq!(sevenzip_chunk_size(u64::MAX, 1), SEVENZIP_MAX_BLOCK);
        assert_eq!(sevenzip_stream_size(12), 12 * dictionary);
        assert_eq!(sevenzip_task_chunks(64 * MIB, 22 * MIB, 12), (3, 22 * MIB));
        assert_eq!(sevenzip_task_chunks(56 * MIB, 29 * MIB, 12), (2, 28 * MIB));
        assert_eq!(
            sevenzip_task_chunks(20 * MIB, dictionary, 12),
            (1, 20 * MIB)
        );
        assert_eq!(sevenzip_task_chunks(1, 22 * MIB, 12), (1, dictionary));
        assert_eq!(sevenzip_task_chunks(u64::MAX / 2, dictionary, 12).0, 12);
        assert!(sevenzip_threads().unwrap() >= 1);
    }
}
