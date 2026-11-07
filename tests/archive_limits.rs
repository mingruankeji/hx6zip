use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{BufWriter, Cursor, Write},
    path::{Path, PathBuf},
};

use hx6zip::archive::{
    ArchiveFormat, CreateOptions, ExtractOptions, JobControl, create_archive, extract_archive,
    inspect_archive,
};
use sevenz_rust2::{ArchiveEntry, ArchiveWriter};
use tempfile::TempDir;
use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

const ENTRY_COUNT: usize = 100_001;

fn make_large_archive(root: &Path, format: ArchiveFormat) -> PathBuf {
    let path = root.join(format!("many.{}", format.extension()));
    let output = BufWriter::new(File::create(&path).unwrap());
    match format {
        ArchiveFormat::Zip => {
            let mut writer = ZipWriter::new(output);
            let options =
                SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
            for index in 0..ENTRY_COUNT {
                writer
                    .start_file(format!("entry-{index:06}.txt"), options)
                    .unwrap();
            }
            writer.finish().unwrap().flush().unwrap();
        }
        ArchiveFormat::SevenZip => {
            let mut writer = ArchiveWriter::new(output).unwrap();
            for index in 0..ENTRY_COUNT {
                let entry = ArchiveEntry::new_file(&format!("entry-{index:06}.txt"));
                writer.push_archive_entry(entry, None::<File>).unwrap();
            }
            writer.finish().unwrap().flush().unwrap();
        }
        ArchiveFormat::Tar => unreachable!("TAR has a fixed entry limit"),
    }
    path
}

fn make_zip_with_declared_size(path: &Path, declared_size: u64) {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    writer
        .start_file(
            "large.bin",
            SimpleFileOptions::default()
                .compression_method(CompressionMethod::Stored)
                .large_file(true),
        )
        .unwrap();
    writer.write_all(b"x").unwrap();
    writer
        .start_file(
            "small.txt",
            SimpleFileOptions::default().compression_method(CompressionMethod::Stored),
        )
        .unwrap();
    writer.write_all(b"small").unwrap();
    let mut data = writer.finish().unwrap().into_inner();

    let footer = data
        .windows(4)
        .rposition(|bytes| bytes == b"PK\x05\x06")
        .unwrap();
    let central =
        u32::from_le_bytes(data[footer + 16..footer + 20].try_into().unwrap()) as usize;
    assert_eq!(&data[central..central + 4], b"PK\x01\x02");
    let name_length =
        u16::from_le_bytes(data[central + 28..central + 30].try_into().unwrap()) as usize;
    let local =
        u32::from_le_bytes(data[central + 42..central + 46].try_into().unwrap()) as usize;
    let local_name_length =
        u16::from_le_bytes(data[local + 26..local + 28].try_into().unwrap()) as usize;

    // The oversized entry is deliberately incomplete; only the small entry is extracted.
    for extra in [central + 46 + name_length, local + 30 + local_name_length] {
        assert_eq!(&data[extra..extra + 4], &[1, 0, 16, 0]);
        data[extra + 4..extra + 12].copy_from_slice(&declared_size.to_le_bytes());
    }
    fs::write(path, data).unwrap();
}

#[test]
fn zip64_selected_extraction_accepts_over_100_gib_declared_sizes() {
    const PREVIOUS_LIMIT: u64 = 100 * 1024 * 1024 * 1024;
    let temp = TempDir::new().unwrap();
    for declared_size in [PREVIOUS_LIMIT - 5, PREVIOUS_LIMIT - 4, PREVIOUS_LIMIT + 1] {
        let archive = temp.path().join(format!("{declared_size}.zip"));
        make_zip_with_declared_size(&archive, declared_size);
        let info = inspect_archive(&archive, None, &JobControl::default()).unwrap();
        assert_eq!(info.entries[0].size, declared_size);
        assert_eq!(info.total_size, declared_size + 5);

        let output = temp.path().join(format!("output-{declared_size}"));
        extract_archive(
            &info,
            &ExtractOptions {
                destination: output.clone(),
                password: None,
                selected: Some(BTreeSet::from([1])),
                overwrite: false,
            },
            &JobControl::default(),
        )
        .unwrap();
        assert_eq!(fs::read(output.join("small.txt")).unwrap(), b"small");
        assert!(!output.join("large.bin").exists());

        if declared_size > PREVIOUS_LIMIT {
            let output = temp.path().join("incomplete");
            let result = extract_archive(
                &info,
                &ExtractOptions {
                    destination: output.clone(),
                    password: None,
                    selected: Some(BTreeSet::from([0])),
                    overwrite: false,
                },
                &JobControl::default(),
            );
            assert!(result.is_err());
            assert!(!output.join("large.bin").exists());
        }
    }
}

#[test]
fn zip_and_sevenzip_open_and_extract_more_than_100000_entries() {
    let temp = TempDir::new().unwrap();
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        let archive = make_large_archive(temp.path(), format);
        let info = inspect_archive(&archive, None, &JobControl::default()).unwrap();
        assert_eq!(info.entries.len(), ENTRY_COUNT);
        let output = temp.path().join(format!("output-{}", format.extension()));
        extract_archive(
            &info,
            &ExtractOptions {
                destination: output.clone(),
                password: None,
                selected: Some(BTreeSet::from([ENTRY_COUNT - 1])),
                overwrite: false,
            },
            &JobControl::default(),
        )
        .unwrap();
        assert_eq!(
            fs::read(output.join(format!("entry-{:06}.txt", ENTRY_COUNT - 1))).unwrap(),
            b""
        );
        assert_eq!(fs::read_dir(output).unwrap().count(), 1);
    }
}

#[test]
#[ignore = "creates 100,000 directories on disk"]
fn zip_and_sevenzip_create_more_than_100000_entries() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    for index in 0..ENTRY_COUNT - 1 {
        fs::create_dir(source.join(format!("dir-{index:06}"))).unwrap();
    }
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        let archive = temp.path().join(format!("created.{}", format.extension()));
        create_archive(
            &CreateOptions {
                sources: vec![source.clone()],
                destination: archive.clone(),
                format,
                password: None,
                overwrite: false,
            },
            &JobControl::default(),
        )
        .unwrap();
        let info = inspect_archive(&archive, None, &JobControl::default()).unwrap();
        assert_eq!(info.entries.len(), ENTRY_COUNT);
    }
}
