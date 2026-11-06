use std::{
    fs::File,
    io::{self, BufReader, Read, Seek, SeekFrom},
    path::Path,
};

use anyhow::{Context, Result, bail, ensure};
use zip::ZipArchive;

use super::{ArchiveKind, IO_BUFFER_SIZE, JobControl};

const MAX_SFX_PREFIX: u64 = 4 * 1024 * 1024;
const SEVENZIP_SIGNATURE: &[u8] = b"7z\xbc\xaf\x27\x1c";

pub(super) fn detect(
    file: &mut File,
    path: &Path,
    control: &JobControl,
) -> Result<(ArchiveKind, u64)> {
    if let Ok(format) = ArchiveKind::from_path(path) {
        return Ok((
            format,
            if format == ArchiveKind::SevenZip {
                sevenzip_offset(file, control)?
            } else {
                0
            },
        ));
    }
    if let Some(header) = find_header(file, control)? {
        return Ok(header);
    }
    control.check_cancelled()?;
    file.rewind().context("无法读取压缩包")?;
    // ZIP's central directory accounts for both relative and SFX-adjusted offsets.
    ZipArchive::new(BufReader::with_capacity(IO_BUFFER_SIZE, &mut *file)).context(
        "未找到可读取的 ZIP、7z 或 RAR 归档；文件不是支持的压缩包、自解压包，或数据已损坏",
    )?;
    file.rewind().context("无法读取压缩包")?;
    Ok((ArchiveKind::Zip, 0))
}

pub(super) fn sevenzip_offset(file: &mut File, control: &JobControl) -> Result<u64> {
    match find_header(file, control)? {
        Some((ArchiveKind::SevenZip, offset)) => Ok(offset),
        _ => bail!("未找到有效的 7z 文件头（自解压前缀最多支持 4 MiB）"),
    }
}

fn find_header(file: &mut File, control: &JobControl) -> Result<Option<(ArchiveKind, u64)>> {
    let length = file.metadata().context("无法读取压缩包信息")?.len();
    let mut buffer = vec![0; IO_BUFFER_SIZE + 31];
    let mut base = 0;
    let mut tried_zip = false;
    while base <= MAX_SFX_PREFIX && base < length {
        control.check_cancelled()?;
        file.seek(SeekFrom::Start(base))
            .context("无法读取自解压文件")?;
        let count = (length - base).min(buffer.len() as u64) as usize;
        file.read_exact(&mut buffer[..count])
            .context("无法读取自解压文件")?;
        let positions = count
            .min(IO_BUFFER_SIZE)
            .min((MAX_SFX_PREFIX - base + 1) as usize);
        for index in 0..positions {
            let header = &buffer[index..count];
            let offset = base + index as u64;
            if !tried_zip
                && (header.starts_with(b"PK\x03\x04") || header.starts_with(b"PK\x05\x06"))
            {
                tried_zip = true;
                if ZipArchive::new(BufReader::with_capacity(IO_BUFFER_SIZE, &mut *file)).is_ok() {
                    return Ok(Some((ArchiveKind::Zip, 0)));
                }
            }
            if header.starts_with(SEVENZIP_SIGNATURE)
                && header.len() >= 32
                && crc32fast::hash(&header[12..32])
                    == u32::from_le_bytes(header[8..12].try_into().unwrap())
            {
                return Ok(Some((ArchiveKind::SevenZip, offset)));
            }
            if (header.len() >= 20 && header.starts_with(b"Rar!\x1a\x07\x00"))
                || (header.len() >= 16 && header.starts_with(b"Rar!\x1a\x07\x01\x00"))
            {
                return Ok(Some((ArchiveKind::Rar, offset)));
            }
        }
        base += IO_BUFFER_SIZE as u64;
    }
    Ok(None)
}

/// Presents the embedded archive as a seekable stream starting at byte zero.
#[derive(Clone)]
pub(super) struct OffsetReader<R> {
    inner: R,
    start: u64,
    length: u64,
    position: u64,
}

impl<R: Seek> OffsetReader<R> {
    pub(super) fn new(mut inner: R, start: u64) -> Result<Self> {
        let end = inner.seek(SeekFrom::End(0)).context("无法读取归档长度")?;
        ensure!(start <= end, "内嵌归档偏移超出文件");
        inner
            .seek(SeekFrom::Start(start))
            .context("无法定位内嵌归档")?;
        Ok(Self {
            inner,
            start,
            length: end - start,
            position: 0,
        })
    }
}

impl<R: Read> Read for OffsetReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = (self.length.saturating_sub(self.position)).min(buffer.len() as u64) as usize;
        let read = self.inner.read(&mut buffer[..count])?;
        self.position += read as u64;
        Ok(read)
    }
}

impl<R: Seek> Seek for OffsetReader<R> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let position = match from {
            SeekFrom::Start(position) => Some(position),
            SeekFrom::Current(offset) => self.position.checked_add_signed(offset),
            SeekFrom::End(offset) => self.length.checked_add_signed(offset),
        };
        let invalid = || io::Error::new(io::ErrorKind::InvalidInput, "无效的内嵌归档读取位置");
        let position = position.ok_or_else(invalid)?;
        let absolute = self.start.checked_add(position).ok_or_else(invalid)?;
        self.inner.seek(SeekFrom::Start(absolute))?;
        self.position = position;
        Ok(position)
    }
}
