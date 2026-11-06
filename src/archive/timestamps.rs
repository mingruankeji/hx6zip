use std::{
    fs::{self, File, Metadata, OpenOptions},
    path::Path,
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result, bail};
use chrono::{Datelike, Local, NaiveDateTime, TimeZone};
use zip::{DateTime, extra_fields::ExtraField, write::FullFileOptions};

use super::common::is_link_or_reparse;

const WINDOWS_EPOCH_OFFSET: u64 = 11_644_473_600;
const WINDOWS_TICKS_PER_SECOND: u64 = 10_000_000;

pub(super) fn windows_time(value: u64) -> Result<SystemTime> {
    let epoch = SystemTime::UNIX_EPOCH
        .checked_sub(Duration::from_secs(WINDOWS_EPOCH_OFFSET))
        .context("无法计算 Windows 时间起点")?;
    let seconds = value / WINDOWS_TICKS_PER_SECOND;
    let nanos = ((value % WINDOWS_TICKS_PER_SECOND) * 100) as u32;
    epoch
        .checked_add(Duration::new(seconds, nanos))
        .context("压缩包中的文件时间超出系统支持的范围")
}

pub(super) fn zip_metadata_modified(file: &zip::ZipFileData) -> Result<Option<SystemTime>> {
    let mut extended = None;
    for field in &file.extra_fields {
        match field {
            ExtraField::Ntfs(timestamp) if timestamp.mtime() != 0 => {
                return windows_time(timestamp.mtime()).map(Some);
            }
            ExtraField::ExtendedTimestamp(timestamp) => {
                if let Some(seconds) = timestamp.mod_time() {
                    extended = Some(
                        SystemTime::UNIX_EPOCH
                            .checked_add(Duration::from_secs(u64::from(seconds)))
                            .context("ZIP 文件时间超出系统支持的范围")?,
                    );
                }
            }
            _ => {}
        }
    }
    if extended.is_some() {
        return Ok(extended);
    }
    let Some(value) = file.last_modified_time else {
        return Ok(None);
    };
    let naive = NaiveDateTime::try_from(value).context("ZIP 文件时间无效")?;
    let local = Local
        .from_local_datetime(&naive)
        .earliest()
        .context("ZIP 文件时间无法转换为本地时间")?;
    Ok(Some(local.into()))
}

pub(super) fn sevenzip_modified(entry: &sevenz_rust2::ArchiveEntry) -> Result<Option<SystemTime>> {
    if entry.has_last_modified_date {
        windows_time(entry.last_modified_date.into()).map(Some)
    } else {
        Ok(None)
    }
}

pub(super) fn write_zip_times(
    options: &mut FullFileOptions<'_>,
    metadata: &Metadata,
) -> Result<DateTime> {
    let modified = metadata.modified().context("无法读取源文件修改时间")?;
    let local: chrono::DateTime<Local> = modified.into();
    let dos_time = match DateTime::try_from(local.naive_local()) {
        Ok(value) => value,
        Err(_) if local.year() < 1980 => DateTime::default(),
        Err(_) => DateTime::from_date_and_time(2107, 12, 31, 23, 59, 58)
            .context("无法设置 ZIP 文件时间")?,
    };

    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        let mut ntfs = Vec::with_capacity(32);
        ntfs.extend_from_slice(&0u32.to_le_bytes());
        ntfs.extend_from_slice(&1u16.to_le_bytes());
        ntfs.extend_from_slice(&24u16.to_le_bytes());
        ntfs.extend_from_slice(&metadata.last_write_time().to_le_bytes());
        ntfs.extend_from_slice(&metadata.last_access_time().to_le_bytes());
        ntfs.extend_from_slice(&metadata.creation_time().to_le_bytes());
        options
            .add_extra_data(0x000a, ntfs, false)
            .context("无法保存 ZIP 文件时间")?;
    }
    #[cfg(not(windows))]
    {
        if let Ok(duration) = modified.duration_since(SystemTime::UNIX_EPOCH) {
            if let Ok(seconds) = u32::try_from(duration.as_secs()) {
                let mut extended = vec![1u8];
                extended.extend_from_slice(&seconds.to_le_bytes());
                options
                    .add_extra_data(0x5455, extended, false)
                    .context("无法保存 ZIP 文件时间")?;
            }
        }
    }
    Ok(dos_time)
}

pub(super) fn set_file_modified(file: &File, modified: Option<SystemTime>) -> Result<()> {
    if let Some(modified) = modified {
        file.set_times(fs::FileTimes::new().set_modified(modified))
            .context("无法恢复文件修改时间")?;
    }
    Ok(())
}

pub(super) fn set_directory_modified(path: &Path, modified: Option<SystemTime>) -> Result<()> {
    let Some(modified) = modified else {
        return Ok(());
    };
    let mut options = OpenOptions::new();
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options
            .write(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    }
    #[cfg(not(windows))]
    options.read(true);

    let directory = options
        .open(path)
        .with_context(|| format!("无法打开文件夹 {}", path.display()))?;
    let metadata = directory.metadata().context("无法检查目标文件夹")?;
    if !metadata.is_dir() || is_link_or_reparse(&metadata) {
        bail!("目标文件夹不是普通目录：{}", path.display());
    }
    set_file_modified(&directory, Some(modified))
}
