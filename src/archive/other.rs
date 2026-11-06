use std::{
    fs::{File, Metadata},
    io::{self, BufRead, BufReader, Read, Seek},
    path::Path,
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result, ensure};
use chrono::{Local, NaiveDate, TimeZone};

use super::{
    ArchiveEntry, ArchiveInfo, ArchiveKind, Cancelled, Compression, JobControl,
    MAX_ARCHIVE_ENTRIES, rar,
};

const MAX_TAR_METADATA_BYTES: u64 = 1024 * 1024;
const MAX_TAR_PADDING_BYTES: u64 = 16 * 1024 * 1024;
const MAX_DECODER_MEMORY_KIB: u32 = 256 * 1024;

pub(super) fn inspect_other(
    path: &Path,
    metadata: &Metadata,
    mut file: File,
    format: ArchiveKind,
    password: Option<&str>,
    control: &JobControl,
) -> Result<ArchiveInfo> {
    let entries = match format {
        ArchiveKind::Cab => inspect_cab(file, control)?,
        ArchiveKind::Rar => rar::inspect(path, password, control)?,
        ArchiveKind::Tar(compression) => {
            let mut entries = Vec::new();
            read_tar_entries(&mut file, compression, control, |entry, _| {
                entries.push(entry.clone());
                Ok(())
            })?;
            entries
        }
        ArchiveKind::Stream(compression) => {
            let name = path
                .file_stem()
                .and_then(|value| value.to_str())
                .filter(|value| !value.is_empty())
                .context("无法确定解压后的文件名")?;
            let size = stream_size(open_stream(file, compression, control)?, control)?;
            vec![ArchiveEntry {
                index: 0,
                name: name.to_owned(),
                size,
                compressed_size: Some(metadata.len()),
                is_directory: false,
                is_encrypted: Some(false),
                unsupported_kind: false,
                is_symbolic_link: false,
                modified: None,
            }]
        }
        ArchiveKind::Zip | ArchiveKind::SevenZip | ArchiveKind::Iso => {
            unreachable!("ZIP, 7z and ISO are inspected by their own readers")
        }
    };
    let total_size = entries.iter().try_fold(0u64, |sum, entry| {
        sum.checked_add(entry.size).context("压缩包大小溢出")
    })?;
    Ok(ArchiveInfo {
        path: path.to_path_buf(),
        format,
        entries,
        total_size,
        archive_size: metadata.len(),
        modified: metadata.modified().context("无法读取压缩包修改时间")?,
    })
}

fn stream_size(mut reader: impl Read, control: &JobControl) -> Result<u64> {
    let mut size = 0u64;
    let mut buffer = [0; 64 * 1024];
    loop {
        control.check_cancelled()?;
        let count = reader.read(&mut buffer).context("无法读取压缩文件")?;
        if count == 0 {
            return Ok(size);
        }
        size = size.checked_add(count as u64).context("解压大小溢出")?;
    }
}

fn inspect_cab(file: File, control: &JobControl) -> Result<Vec<ArchiveEntry>> {
    let cabinet = cab::Cabinet::new(file).context("无法读取 CAB 压缩包")?;
    let mut entries = Vec::new();
    for folder in cabinet.folder_entries() {
        for file in folder.file_entries() {
            control.check_cancelled()?;
            ensure!(
                entries.len() < MAX_ARCHIVE_ENTRIES,
                "压缩包条目超过安全限制（{MAX_ARCHIVE_ENTRIES}）"
            );
            ensure!(
                !file.name().contains('\u{fffd}'),
                "CAB 文件名编码不受支持：{}",
                file.name()
            );
            let modified = file
                .datetime()
                .map(|value| {
                    let naive = NaiveDate::from_ymd_opt(
                        value.year(),
                        u8::from(value.month()).into(),
                        value.day().into(),
                    )
                    .and_then(|date| {
                        date.and_hms_opt(
                            value.hour().into(),
                            value.minute().into(),
                            value.second().into(),
                        )
                    })
                    .context("CAB 文件时间无效")?;
                    let local = Local
                        .from_local_datetime(&naive)
                        .earliest()
                        .context("CAB 文件时间无法转换为本地时间")?;
                    Ok::<SystemTime, anyhow::Error>(local.into())
                })
                .transpose()?;
            entries.push(ArchiveEntry {
                index: entries.len(),
                name: file.name().to_owned(),
                size: file.uncompressed_size().into(),
                compressed_size: None,
                is_directory: false,
                is_encrypted: Some(false),
                unsupported_kind: false,
                is_symbolic_link: false,
                modified,
            });
        }
    }
    Ok(entries)
}

struct ControlledReader<'a, R> {
    inner: R,
    control: &'a JobControl,
}

impl<R: Read> Read for ControlledReader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.control
            .check_cancelled()
            .map_err(|_| io::Error::other(Cancelled))?;
        self.inner.read(buffer)
    }
}

pub(super) fn open_stream<'a>(
    input: impl Read + 'a,
    compression: Compression,
    control: &'a JobControl,
) -> Result<Box<dyn Read + 'a>> {
    let input = ControlledReader {
        inner: input,
        control,
    };
    let decoder: Box<dyn Read + 'a> = match compression {
        Compression::None => Box::new(input),
        Compression::Gzip => Box::new(flate2::read::MultiGzDecoder::new(input)),
        Compression::Bzip2 => Box::new(bzip2::read::MultiBzDecoder::new(input)),
        Compression::Xz => Box::new(lzma_rust2::XzReader::new_mem_limit(
            input,
            true,
            MAX_DECODER_MEMORY_KIB,
        )),
        Compression::Zstd => {
            let mut decoder =
                zstd::stream::read::Decoder::new(input).context("无法初始化 Zstandard 解码器")?;
            decoder
                .window_log_max(28)
                .context("无法设置 Zstandard 解码器内存限制")?;
            Box::new(decoder)
        }
        Compression::Brotli => {
            let mut input = BufReader::new(input);
            let header = *input
                .fill_buf()
                .context("无法读取 Brotli 文件头")?
                .first()
                .context("Brotli 文件为空")?;
            // The decoder otherwise accepts the non-standard large-window extension.
            ensure!(header & 0x7f != 0x11, "暂不支持大窗口 Brotli 文件");
            Box::new(brotli::Decompressor::new(input, 64 * 1024))
        }
    };
    Ok(Box::new(ControlledReader {
        inner: decoder,
        control,
    }))
}

fn preflight_tar<R: Read + Seek>(
    file: &mut R,
    compression: Compression,
    control: &JobControl,
) -> Result<()> {
    let mut archive = tar::Archive::new(open_stream(&mut *file, compression, control)?);
    let mut total = 0u64;
    let mut count = 0usize;
    for entry in archive.entries().context("无法读取 TAR 压缩包")?.raw(true) {
        control.check_cancelled()?;
        let entry = entry.context("无法读取 TAR 条目")?;
        count += 1;
        ensure!(count <= MAX_ARCHIVE_ENTRIES * 4 + 8, "TAR 条目超过安全限制");
        let kind = entry.header().entry_type();
        if kind.is_gnu_longname()
            || kind.is_gnu_longlink()
            || kind.is_pax_local_extensions()
            || kind.is_pax_global_extensions()
        {
            ensure!(
                entry.size() <= MAX_TAR_METADATA_BYTES,
                "TAR 扩展元数据超过安全限制"
            );
        } else {
            ensure!(!kind.is_gnu_sparse(), "暂不支持 TAR 稀疏文件");
            total = total.checked_add(entry.size()).context("压缩包大小溢出")?;
        }
    }
    drain_tar_padding(archive.into_inner(), control)?;
    file.rewind().context("无法重新读取 TAR 压缩包")
}

fn drain_tar_padding(mut reader: impl Read, control: &JobControl) -> Result<()> {
    let mut total = 0u64;
    let mut buffer = [0; 64 * 1024];
    loop {
        control.check_cancelled()?;
        let count = reader
            .read(&mut buffer)
            .context("无法验证 TAR 压缩包结尾")?;
        if count == 0 {
            return Ok(());
        }
        total = total
            .checked_add(count as u64)
            .context("TAR 尾部大小溢出")?;
        ensure!(
            total <= MAX_TAR_PADDING_BYTES && buffer[..count].iter().all(|byte| *byte == 0),
            "TAR 压缩包结尾无效或过大"
        );
    }
}

fn tar_root(name: &str) -> bool {
    !name.starts_with('/') && name.split('/').all(|part| part.is_empty() || part == ".")
}

pub(super) fn read_tar_entries<R: Read + Seek>(
    file: &mut R,
    compression: Compression,
    control: &JobControl,
    mut visit: impl FnMut(&ArchiveEntry, &mut dyn Read) -> Result<()>,
) -> Result<()> {
    preflight_tar(file, compression, control)?;
    let mut archive = tar::Archive::new(open_stream(&mut *file, compression, control)?);
    let mut index = 0usize;
    let mut total = 0u64;
    for result in archive.entries().context("无法读取 TAR 压缩包")? {
        control.check_cancelled()?;
        let mut file = result.context("无法读取 TAR 条目")?;
        let name = std::str::from_utf8(&file.path_bytes())
            .context("TAR 文件名不是有效的 UTF-8")?
            .to_owned();
        let kind = file.header().entry_type();
        let is_directory = kind.is_dir();
        if is_directory && tar_root(&name) {
            continue;
        }
        ensure!(index < MAX_ARCHIVE_ENTRIES, "压缩包条目超过安全限制");
        let size = file.size();
        ensure!(!is_directory || size == 0, "TAR 文件夹包含非空数据：{name}");
        total = total.checked_add(size).context("压缩包大小溢出")?;
        let seconds = file.header().mtime().context("TAR 文件时间无效")?;
        let modified = SystemTime::UNIX_EPOCH
            .checked_add(Duration::from_secs(seconds))
            .context("TAR 文件时间超出系统支持的范围")?;
        let entry = ArchiveEntry {
            index,
            name,
            size,
            compressed_size: None,
            is_directory,
            is_encrypted: Some(false),
            unsupported_kind: !kind.is_file() && !is_directory,
            is_symbolic_link: kind.is_symlink(),
            modified: Some(modified),
        };
        visit(&entry, &mut file)?;
        index += 1;
    }
    drain_tar_padding(archive.into_inner(), control)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::SeekFrom;

    const LARGE_SIZE: u64 = 100 * 1024 * 1024 * 1024 + 1;

    struct VirtualInput {
        prefix: Vec<u8>,
        position: u64,
        length: u64,
    }

    impl Read for VirtualInput {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let count = self.length.saturating_sub(self.position).min(buffer.len() as u64) as usize;
            buffer[..count].fill(0);
            if self.position < self.prefix.len() as u64 {
                let start = self.position as usize;
                let copied = count.min(self.prefix.len() - start);
                buffer[..copied].copy_from_slice(&self.prefix[start..start + copied]);
            }
            self.position += count as u64;
            Ok(count)
        }
    }

    impl Seek for VirtualInput {
        fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
            self.position = match position {
                SeekFrom::Start(value) => Some(value),
                SeekFrom::Current(value) => self.position.checked_add_signed(value),
                SeekFrom::End(value) => self.length.checked_add_signed(value),
            }
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid position"))?;
            Ok(self.position)
        }
    }

    fn append_header(output: &mut Vec<u8>, name: &str, size: u64) {
        let mut header = tar::Header::new_gnu();
        header.set_path(name).unwrap();
        header.set_size(size);
        header.set_mode(0o644);
        header.set_mtime(1_600_000_000);
        header.set_cksum();
        output.extend_from_slice(header.as_bytes());
    }

    #[test]
    fn standalone_stream_size_accepts_over_100_gib() {
        let input = VirtualInput {
            prefix: Vec::new(),
            position: 0,
            length: LARGE_SIZE,
        };
        assert_eq!(stream_size(input, &JobControl::default()).unwrap(), LARGE_SIZE);
    }

    #[test]
    fn tar_entries_accept_over_100_gib() {
        let mut prefix = Vec::new();
        append_header(&mut prefix, "small.txt", 5);
        prefix.extend_from_slice(b"small");
        prefix.resize(1024, 0);
        append_header(&mut prefix, "large.bin", LARGE_SIZE);
        let padding_size = (LARGE_SIZE + 511) & !511;
        let mut input = VirtualInput {
            length: prefix.len() as u64 + padding_size + 1024,
            prefix,
            position: 0,
        };
        let mut entries = Vec::new();
        read_tar_entries(
            &mut input,
            Compression::None,
            &JobControl::default(),
            |entry, _| {
                entries.push(entry.clone());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].size, 5);
        assert_eq!(entries[1].size, LARGE_SIZE);
    }
}
