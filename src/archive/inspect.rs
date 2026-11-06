use std::{
    fs::File,
    io::{BufReader, Read, Seek},
    path::Path,
};

use anyhow::{Context, Result};
use sevenz_rust2::{ArchiveReader, Password};
use zip::ZipArchive;

use super::{
    ArchiveEntry, ArchiveInfo, ArchiveKind, IO_BUFFER_SIZE, JobControl,
    iso::read_iso,
    other::inspect_other,
    sfx::{self, OffsetReader},
    timestamps::{sevenzip_modified, zip_metadata_modified},
};

pub fn inspect_archive(
    path: &Path,
    password: Option<&str>,
    control: &JobControl,
) -> Result<ArchiveInfo> {
    control.with_archive_password(path, password, false, true, |password| {
        inspect_archive_with_password(path, password, control)
    })
}

fn inspect_archive_with_password(
    path: &Path,
    password: Option<&str>,
    control: &JobControl,
) -> Result<ArchiveInfo> {
    control.check_cancelled()?;
    let mut file =
        File::open(path).with_context(|| format!("无法打开压缩包 {}", path.display()))?;
    let metadata = file.metadata().context("无法读取压缩包信息")?;
    let (format, offset) = sfx::detect(&mut file, path, control)?;
    file.rewind().context("无法重新读取压缩包")?;
    match format {
        ArchiveKind::Zip => {
            let mut reader = ZipArchive::new(BufReader::with_capacity(IO_BUFFER_SIZE, file))
                .context("无法读取 ZIP 压缩包")?;
            inspect_zip(path, &metadata, &mut reader, control)
        }
        ArchiveKind::SevenZip => {
            let reader = ArchiveReader::new(
                BufReader::with_capacity(IO_BUFFER_SIZE, OffsetReader::new(file, offset)?),
                Password::from(password.unwrap_or_default()),
            )
            .context("无法读取 7z 压缩包，密码可能不正确或格式不受支持")?;
            inspect_sevenzip(path, &metadata, &reader, control)
        }
        ArchiveKind::Iso => {
            let mut file = file;
            Ok(read_iso(path, &metadata, &mut file, control)?.info)
        }
        _ => inspect_other(path, &metadata, file, format, password, control),
    }
}

pub(super) fn inspect_zip<R: Read + Seek>(
    path: &Path,
    metadata: &std::fs::Metadata,
    reader: &mut ZipArchive<R>,
    control: &JobControl,
) -> Result<ArchiveInfo> {
    let mut entries = Vec::with_capacity(reader.len());
    let mut total_size = 0u64;
    for index in 0..reader.len() {
        control.check_cancelled()?;
        let file = reader
            .file_metadata(index)
            .context("无法读取 ZIP 条目信息")?;
        total_size = total_size
            .checked_add(file.uncompressed_size)
            .context("压缩包大小溢出")?;
        entries.push(ArchiveEntry {
            index,
            name: file.file_name.to_string(),
            size: file.uncompressed_size,
            compressed_size: Some(file.compressed_size),
            is_directory: file.is_dir(),
            is_encrypted: Some(file.encrypted),
            unsupported_kind: file.unix_mode().is_some_and(|mode| mode & 0xa000 == 0xa000),
            is_symbolic_link: file.unix_mode().is_some_and(|mode| mode & 0xf000 == 0xa000),
            modified: zip_metadata_modified(file)?,
        });
    }
    Ok(ArchiveInfo {
        path: path.to_path_buf(),
        format: ArchiveKind::Zip,
        entries,
        total_size,
        archive_size: metadata.len(),
        modified: metadata.modified().context("无法读取压缩包修改时间")?,
    })
}

pub(super) fn inspect_sevenzip<R: Read + Seek>(
    path: &Path,
    metadata: &std::fs::Metadata,
    reader: &ArchiveReader<R>,
    control: &JobControl,
) -> Result<ArchiveInfo> {
    let files = &reader.archive().files;
    let mut entries = Vec::with_capacity(files.len());
    let mut total_size = 0u64;
    for (index, file) in files.iter().enumerate() {
        control.check_cancelled()?;
        total_size = total_size
            .checked_add(file.size)
            .context("压缩包大小溢出")?;
        let unix_kind = (file.windows_attributes >> 16) & 0xf000;
        let is_link = file.has_windows_attributes
            && (unix_kind == 0xa000
                || (file.windows_attributes & 0x400 != 0
                    && !(file.is_directory && file.size == 0)));
        entries.push(ArchiveEntry {
            index,
            name: file.name.clone(),
            size: file.size,
            compressed_size: (!reader.archive().is_solid).then_some(file.compressed_size),
            is_directory: file.is_directory,
            is_encrypted: Some(
                reader.archive().stream_map.file_block_index[index].is_some_and(|block| {
                    reader.archive().blocks[block].coders.iter().any(|coder| {
                        coder.encoder_method_id() == sevenz_rust2::EncoderMethod::ID_AES256_SHA256
                    })
                }),
            ),
            unsupported_kind: file.is_anti_item || is_link,
            is_symbolic_link: file.has_windows_attributes && unix_kind == 0xa000,
            modified: sevenzip_modified(file)?,
        });
    }

    Ok(ArchiveInfo {
        path: path.to_path_buf(),
        format: ArchiveKind::SevenZip,
        entries,
        total_size,
        archive_size: metadata.len(),
        modified: metadata.modified().context("无法读取压缩包修改时间")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::Cell,
        io::{self, Cursor, SeekFrom, Write},
        rc::Rc,
    };

    struct Counted {
        input: Cursor<Vec<u8>>,
        bytes: Rc<Cell<usize>>,
        seeks: Rc<Cell<usize>>,
    }
    impl Read for Counted {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let count = self.input.read(buffer)?;
            self.bytes.set(self.bytes.get() + count);
            Ok(count)
        }
    }
    impl Seek for Counted {
        fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
            self.seeks.set(self.seeks.get() + 1);
            self.input.seek(position)
        }
    }

    #[test]
    fn zip_listing_performs_zero_local_header_reads_or_seeks() {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for index in 0..512 {
            writer
                .start_file(
                    format!("folder/{index}.bin"),
                    zip::write::SimpleFileOptions::default()
                        .compression_method(zip::CompressionMethod::Stored),
                )
                .unwrap();
            writer.write_all(&vec![index as u8; 32 * 1024]).unwrap();
        }
        let bytes = writer.finish().unwrap().into_inner();
        let length = bytes.len();
        let read = Rc::new(Cell::new(0));
        let seeks = Rc::new(Cell::new(0));
        let mut reader = ZipArchive::new(Counted {
            input: Cursor::new(bytes),
            bytes: read.clone(),
            seeks: seeks.clone(),
        })
        .unwrap();
        assert!(
            read.get() < length / 8,
            "opening must not scan file payloads"
        );
        read.set(0);
        seeks.set(0);
        let temp = tempfile::NamedTempFile::new().unwrap();
        let info = inspect_zip(
            temp.path(),
            &temp.as_file().metadata().unwrap(),
            &mut reader,
            &JobControl::default(),
        )
        .unwrap();
        assert_eq!(info.entries.len(), 512);
        assert_eq!(info.total_size, 512 * 32 * 1024);
        assert_eq!(
            read.get(),
            0,
            "listing must use already parsed central-directory metadata"
        );
        assert_eq!(seeks.get(), 0);
    }
}
