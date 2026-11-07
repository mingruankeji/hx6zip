use std::{
    fs::{self, File, Metadata},
    io::{self, BufReader, Read, Write},
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use tempfile::NamedTempFile;

use super::{JobControl, Progress};

pub(super) fn safe_relative_path(name: &str) -> Result<PathBuf> {
    let normalized = name.replace('\\', "/");
    if normalized.starts_with('/') {
        bail!("压缩包包含绝对路径：{name}");
    }

    let mut result = PathBuf::new();
    for part in normalized.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".."
            || part.ends_with([' ', '.'])
            || part
                .chars()
                .any(|c| c.is_control() || "<>:\"|?*".contains(c))
        {
            bail!("压缩包包含不安全的路径：{name}");
        }
        let base = part
            .split('.')
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        let reserved = matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || matches!(
                base.as_str(),
                "COM1"
                    | "COM2"
                    | "COM3"
                    | "COM4"
                    | "COM5"
                    | "COM6"
                    | "COM7"
                    | "COM8"
                    | "COM9"
                    | "LPT1"
                    | "LPT2"
                    | "LPT3"
                    | "LPT4"
                    | "LPT5"
                    | "LPT6"
                    | "LPT7"
                    | "LPT8"
                    | "LPT9"
            );
        if reserved {
            bail!("压缩包包含 Windows 保留文件名：{name}");
        }
        result.push(part);
    }
    if result.as_os_str().is_empty()
        || result
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        bail!("压缩包包含无效路径：{name}");
    }
    Ok(result)
}

pub(super) fn collision_key(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/").to_lowercase()
}

pub(super) fn is_link_or_reparse(metadata: &Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

pub(super) fn checked_metadata(path: &Path) -> Result<Option<Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if is_link_or_reparse(&metadata) {
                bail!("不允许通过符号链接或重解析点访问：{}", path.display());
            }
            Ok(Some(metadata))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("无法检查 {}", path.display())),
    }
}

pub(super) fn followed_source_metadata(path: &Path) -> Result<Metadata> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("源文件或链接目标不存在或无法读取：{}", path.display()))?;
    if !metadata.is_file() && !metadata.is_dir() {
        bail!("不支持的源文件类型：{}", path.display());
    }
    Ok(metadata)
}

fn link_directory(metadata: &Metadata) -> bool {
    if !is_link_or_reparse(metadata) {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x10 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.is_dir()
    }
}

pub(super) fn source_walk_path(
    item: walkdir::Result<walkdir::DirEntry>,
    control: &JobControl,
) -> Result<Option<PathBuf>> {
    control.check_cancelled()?;
    match item {
        Ok(item) => Ok(Some(item.into_path())),
        Err(error) if error.loop_ancestor().is_some() => {
            Err(error).context("源文件夹包含循环链接，无法继续压缩")
        }
        Err(error) => {
            control.warn(format!("扫描警告（未读取目录内容或文件）：{error}"));
            // A failed followed junction has no DirEntry. Like 7-Zip, retain
            // its directory entry even though its contents cannot be scanned.
            Ok(error.path().and_then(|path| {
                fs::symlink_metadata(path)
                    .ok()
                    .filter(link_directory)
                    .map(|_| path.to_owned())
            }))
        }
    }
}

pub(super) fn scan_source_metadata(path: &Path, control: &JobControl) -> Result<Option<Metadata>> {
    control.check_cancelled()?;
    let result = fs::metadata(path).and_then(|metadata| {
        if metadata.is_file() {
            File::open(path)?;
        }
        Ok(metadata)
    });
    match result {
        Ok(metadata) => {
            if !metadata.is_file() && !metadata.is_dir() {
                bail!("不支持的源文件类型：{}", path.display());
            }
            Ok(Some(metadata))
        }
        Err(error) => {
            if let Ok(metadata) = fs::symlink_metadata(path)
                && link_directory(&metadata)
            {
                control.warn(format!(
                    "扫描警告（仅保留空文件夹）：{}：{error}",
                    path.display()
                ));
                return Ok(Some(metadata));
            }
            control.warn(format!("扫描警告（已跳过）：{}：{error}", path.display()));
            Ok(None)
        }
    }
}

pub(super) fn extraction_metadata(path: &Path) -> Result<Option<Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if is_link_or_reparse(&metadata) => {
            let target = fs::metadata(path)
                .with_context(|| format!("无法读取解压目标链接：{}", path.display()))?;
            if !target.is_dir() {
                bail!("解压目标链接不是文件夹：{}", path.display());
            }
            Ok(Some(target))
        }
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("无法检查 {}", path.display())),
    }
}

pub(super) fn ensure_root(destination: &Path) -> Result<PathBuf> {
    if let Some(metadata) = extraction_metadata(destination)? {
        if !metadata.is_dir() {
            bail!("解压目标不是文件夹：{}", destination.display());
        }
    } else {
        fs::create_dir_all(destination)
            .with_context(|| format!("无法创建目标文件夹 {}", destination.display()))?;
        if extraction_metadata(destination)?.is_none() {
            bail!("创建目标文件夹后无法访问：{}", destination.display());
        }
    }
    fs::canonicalize(destination)
        .with_context(|| format!("无法访问目标文件夹 {}", destination.display()))
}

/// Validates the path and returns whether the target itself exists.
pub(super) fn check_target(
    root: &Path,
    relative: &Path,
    is_dir: bool,
    overwrite: bool,
) -> Result<bool> {
    let mut path = root.to_path_buf();
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        path.push(component.as_os_str());
        let Some(metadata) = extraction_metadata(&path)? else {
            return Ok(false);
        };
        let last = components.peek().is_none();
        if !last || is_dir {
            if !metadata.is_dir() {
                bail!("目标路径不是文件夹：{}", path.display());
            }
        } else if !metadata.is_file() {
            bail!("目标路径不是普通文件：{}", path.display());
        } else if !overwrite {
            bail!("目标文件已存在，未启用覆盖：{}", path.display());
        }
        if last {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn ensure_directory(root: &Path, relative: &Path) -> Result<PathBuf> {
    let mut current = root.to_path_buf();
    for component in relative.components() {
        if !matches!(component, Component::Normal(_)) {
            bail!("无效的目标文件夹路径：{}", relative.display());
        }
        current.push(component.as_os_str());
        match extraction_metadata(&current)? {
            Some(metadata) if metadata.is_dir() => {}
            Some(_) => bail!("目标路径不是文件夹：{}", current.display()),
            None => {
                fs::create_dir(&current)
                    .with_context(|| format!("无法创建文件夹 {}", current.display()))?;
                let metadata = extraction_metadata(&current)?
                    .with_context(|| format!("无法检查新建文件夹 {}", current.display()))?;
                if !metadata.is_dir() {
                    bail!("目标路径不是文件夹：{}", current.display());
                }
            }
        }
    }
    Ok(current)
}

pub(super) fn persist_temp(temp: NamedTempFile, destination: &Path, overwrite: bool) -> Result<()> {
    let result = if overwrite {
        temp.persist(destination)
    } else {
        temp.persist_noclobber(destination)
    };
    result
        .map(|_| ())
        .map_err(|error| error.error)
        .with_context(|| format!("无法保存 {}", destination.display()))
}

pub(super) struct Tracker<'a> {
    control: &'a JobControl,
    operation: &'static str,
    total: u64,
    completed: u64,
    current: String,
    archive_size: Option<u64>,
    last_report: Instant,
}

impl<'a> Tracker<'a> {
    pub(super) fn new(control: &'a JobControl, operation: &'static str, total: u64) -> Self {
        Self {
            control,
            operation,
            total,
            completed: 0,
            current: String::new(),
            archive_size: None,
            last_report: Instant::now(),
        }
    }

    pub(super) fn set_archive_size(&mut self, size: u64) {
        self.archive_size = Some(size);
        if self.last_report.elapsed() >= Duration::from_millis(100) {
            self.report();
        }
    }

    pub(super) fn start_file(&mut self, name: &str) -> Result<()> {
        self.control.check_cancelled()?;
        self.current = name.to_owned();
        self.report();
        Ok(())
    }

    pub(super) fn advance(&mut self, bytes: usize) -> Result<()> {
        self.control.check_cancelled()?;
        self.completed = self
            .completed
            .checked_add(bytes as u64)
            .context("处理的数据量过大")?;
        if self.completed > self.total {
            bail!("处理的数据量超过预期，文件可能已发生变化");
        }
        if self.last_report.elapsed() >= Duration::from_millis(100) {
            self.report();
        }
        Ok(())
    }

    pub(super) fn finish(&mut self) -> Result<()> {
        self.control.check_cancelled()?;
        if self.completed != self.total {
            bail!("处理的数据量与预期不符，任务未完成");
        }
        self.report();
        Ok(())
    }

    fn report(&mut self) {
        self.control.report(Progress {
            operation: self.operation,
            current_file: self.current.clone(),
            completed_bytes: self.completed,
            total_bytes: self.total,
            archive_size: self.archive_size,
        });
        self.last_report = Instant::now();
    }
}

pub(super) fn copy_stream(
    reader: &mut dyn Read,
    writer: &mut dyn Write,
    expected: u64,
    control: &JobControl,
    mut advance: impl FnMut(usize) -> Result<()>,
) -> Result<()> {
    let mut buffer = vec![0u8; expected.clamp(1, 512 * 1024) as usize];
    let mut copied = 0u64;
    loop {
        control.check_cancelled()?;
        let mut filled = 0;
        while filled < buffer.len() {
            control.check_cancelled()?;
            let count = match reader.read(&mut buffer[filled..]) {
                Ok(0) => break,
                Ok(count) => count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error).context("读取文件内容失败"),
            };
            copied = copied.checked_add(count as u64).context("文件大小溢出")?;
            if copied > expected {
                bail!("文件内容超过声明大小，已停止处理");
            }
            filled += count;
        }
        if filled > 0 {
            writer
                .write_all(&buffer[..filled])
                .context("写入文件内容失败")?;
            advance(filled)?;
        }
        if filled < buffer.len() {
            break;
        }
    }
    if copied != expected {
        bail!("文件内容大小与预期不符");
    }
    Ok(())
}

pub(super) struct TrackingReader<'a, F> {
    pub file: BufReader<File>,
    pub expected: u64,
    pub read: u64,
    pub control: &'a JobControl,
    pub advance: F,
}

impl<F: FnMut(usize) -> Result<()>> Read for TrackingReader<'_, F> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.control.cancellation.is_cancelled() {
            return Err(io::Error::other("操作已取消"));
        }

        let count = self.file.read(buffer)?;
        self.read = self
            .read
            .checked_add(count as u64)
            .ok_or_else(|| io::Error::other("文件大小溢出"))?;
        if self.read > self.expected || (count == 0 && self.read != self.expected) {
            return Err(io::Error::other("文件在压缩过程中发生变化"));
        }
        (self.advance)(count).map_err(|error| io::Error::other(error.to_string()))?;
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::CancellationToken;

    struct ShortReads {
        remaining: usize,
        interrupted: bool,
    }

    impl Read for ShortReads {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if !self.interrupted {
                self.interrupted = true;
                return Err(io::ErrorKind::Interrupted.into());
            }
            let count = self.remaining.min(buffer.len()).min(127);
            buffer[..count].fill(42);
            self.remaining -= count;
            Ok(count)
        }
    }

    #[test]
    fn copy_stream_coalesces_short_reads_and_retries_interruptions() {
        let size = 1024 * 1024 + 17;
        let mut reader = ShortReads {
            remaining: size,
            interrupted: false,
        };
        let mut output = Vec::new();
        let mut writes = Vec::new();
        copy_stream(
            &mut reader,
            &mut output,
            size as u64,
            &JobControl::default(),
            |count| {
                writes.push(count);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(output, vec![42; size]);
        assert_eq!(writes, vec![512 * 1024, 512 * 1024, 17]);
    }

    #[test]
    fn copy_stream_rejects_size_mismatches_and_honors_cancellation() {
        for (actual, expected) in [(100, 99), (99, 100), (1, 0)] {
            let mut reader = ShortReads {
                remaining: actual,
                interrupted: false,
            };
            assert!(
                copy_stream(
                    &mut reader,
                    &mut Vec::new(),
                    expected,
                    &JobControl::default(),
                    |_| Ok(())
                )
                .is_err()
            );
        }
        let token = CancellationToken::default();
        let control = JobControl::new(token.clone(), |_| {});
        let mut reader = ShortReads {
            remaining: 1024 * 1024,
            interrupted: false,
        };
        let error = copy_stream(&mut reader, &mut Vec::new(), 1024 * 1024, &control, |_| {
            token.cancel();
            Ok(())
        })
        .unwrap_err();
        assert!(error.downcast_ref::<crate::archive::Cancelled>().is_some());
        assert_eq!(reader.remaining, 512 * 1024);
    }
}
