use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Cursor, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use mzip::archive::{
    ArchiveEdit, ArchiveFormat, CancellationToken, CreateOptions, IntegrityMode, JobControl,
    create_archive, edit_archive, export_entries, inspect_archive,
};
use sevenz_rust2::{
    Archive, ArchiveEntry, ArchiveWriter, EncoderConfiguration, EncoderMethod, Password,
    SourceReader,
};

fn zip_payloads(path: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut archive = zip::ZipArchive::new(File::open(path).unwrap()).unwrap();
    (0..archive.len())
        .map(|index| {
            let mut entry = archive.by_index_raw(index).unwrap();
            let name = entry.name().to_owned();
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).unwrap();
            (name, bytes)
        })
        .collect()
}

fn sevenzip_blocks(path: &Path, password: Option<&str>) -> BTreeMap<Vec<String>, Vec<u8>> {
    let mut file = File::open(path).unwrap();
    let archive = Archive::read(&mut file, &Password::from(password.unwrap_or_default())).unwrap();
    (0..archive.blocks.len())
        .map(|block| {
            let names = archive
                .files
                .iter()
                .enumerate()
                .filter(|(index, entry)| {
                    entry.has_stream && archive.stream_map.file_block_index[*index] == Some(block)
                })
                .map(|(_, entry)| entry.name.clone())
                .collect::<Vec<_>>();
            let first = archive.stream_map.block_first_pack_stream_index()[block];
            let end = archive
                .stream_map
                .block_first_pack_stream_index()
                .get(block + 1)
                .copied()
                .unwrap_or(archive.pack_sizes().len());
            let mut bytes = Vec::new();
            for index in first..end {
                file.seek(SeekFrom::Start(
                    32 + archive.pack_pos() + archive.stream_map.pack_stream_offsets()[index],
                ))
                .unwrap();
                (&mut file)
                    .take(archive.pack_sizes()[index])
                    .read_to_end(&mut bytes)
                    .unwrap();
            }
            (names, bytes)
        })
        .collect()
}

fn block_fixture(root: &Path) -> PathBuf {
    let path = root.join("blocks.7z");
    let mut writer = ArchiveWriter::new(File::create(&path).unwrap()).unwrap();
    writer.set_content_methods(vec![EncoderConfiguration::new(EncoderMethod::COPY)]);
    writer
        .push_archive_entries(
            vec![
                ArchiveEntry::new_file("a.txt"),
                ArchiveEntry::new_file("b.txt"),
            ],
            vec![
                SourceReader::new(Cursor::new(b"first".to_vec())),
                SourceReader::new(Cursor::new(vec![b'b'; 256 * 1024])),
            ],
        )
        .unwrap();
    writer
        .push_archive_entry(
            ArchiveEntry::new_file("unrelated.txt"),
            Some(Cursor::new(vec![b'c'; 256 * 1024])),
        )
        .unwrap();
    writer.finish().unwrap();
    path
}

#[test]
fn zip_update_preserves_ciphertext_metadata_and_omits_deleted_records() {
    for mode in [IntegrityMode::Normal, IntegrityMode::Safe] {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first.txt");
        let second = temp.path().join("second.txt");
        fs::write(&first, vec![b'a'; 128 * 1024]).unwrap();
        fs::write(&second, b"remove me").unwrap();
        let path = temp.path().join("encrypted.zip");
        let control = JobControl::default().with_integrity(mode);
        create_archive(
            &CreateOptions {
                sources: vec![first, second],
                destination: path.clone(),
                format: ArchiveFormat::Zip,
                password: Some("secret".into()),
                overwrite: false,
            },
            &control,
        )
        .unwrap();
        let before = zip_payloads(&path);
        let info = inspect_archive(&path, Some("secret"), &control).unwrap();
        let old_time = info
            .entries
            .iter()
            .find(|e| e.name == "first.txt")
            .unwrap()
            .modified;
        let info = edit_archive(
            &info,
            &ArchiveEdit::Delete {
                paths: vec!["second.txt".into()],
            },
            Some("secret"),
            &control,
        )
        .unwrap();
        let after = zip_payloads(&path);
        assert_eq!(
            after["first.txt"], before["first.txt"],
            "AES ciphertext must be copied, not re-encoded"
        );
        assert_eq!(after.len(), 1);
        assert_eq!(info.entries[0].modified, old_time);
        let bytes = fs::read(&path).unwrap();
        let archive = zip::ZipArchive::new(Cursor::new(&bytes)).unwrap();
        let offset = archive.file_metadata(0).unwrap().central_header_start as usize;
        assert_eq!(
            u16::from_le_bytes([bytes[offset + 10], bytes[offset + 11]]),
            99,
            "AES must remain method 99 in the rewritten central directory"
        );
        assert!(
            !bytes
                .windows(b"second.txt".len())
                .any(|window| window == b"second.txt")
        );
        let exported =
            export_entries(&info, &["first.txt".into()], Some("secret"), &control).unwrap();
        assert_eq!(
            fs::read(&exported.paths[0]).unwrap(),
            vec![b'a'; 128 * 1024]
        );
    }
}

#[test]
fn adding_to_sevenzip_copies_all_original_blocks_byte_for_byte() {
    for mode in [IntegrityMode::Normal, IntegrityMode::Safe] {
        let temp = tempfile::tempdir().unwrap();
        let path = block_fixture(temp.path());
        let before = sevenzip_blocks(&path, None);
        let added = temp.path().join("added.txt");
        fs::write(&added, b"new data").unwrap();
        let progress = Arc::new(Mutex::new(Vec::new()));
        let events = progress.clone();
        let control = JobControl::new(CancellationToken::default(), move |event| {
            events
                .lock()
                .unwrap()
                .push((event.operation, event.current_file));
        })
        .with_integrity(mode);
        let info = inspect_archive(&path, None, &control).unwrap();
        edit_archive(
            &info,
            &ArchiveEdit::Add {
                sources: vec![added],
                directory: String::new(),
                overwrite: false,
            },
            None,
            &control,
        )
        .unwrap();
        let after = sevenzip_blocks(&path, None);
        for (names, bytes) in before {
            assert_eq!(after[&names], bytes);
        }
        assert!(
            !progress
                .lock()
                .unwrap()
                .iter()
                .any(|(operation, _)| *operation == "正在解压")
        );
    }
}

#[test]
fn replacing_one_solid_member_reencodes_only_its_block() {
    let temp = tempfile::tempdir().unwrap();
    let path = block_fixture(temp.path());
    let before = sevenzip_blocks(&path, None);
    let replacement = temp.path().join("b.txt");
    fs::write(&replacement, b"replaced").unwrap();
    let progress = Arc::new(Mutex::new(Vec::new()));
    let events = progress.clone();
    let control = JobControl::new(CancellationToken::default(), move |event| {
        if event.operation == "正在解压" {
            events.lock().unwrap().push(event.current_file);
        }
    });
    let info = inspect_archive(&path, None, &control).unwrap();
    let info = edit_archive(
        &info,
        &ArchiveEdit::Add {
            sources: vec![replacement],
            directory: String::new(),
            overwrite: true,
        },
        None,
        &control,
    )
    .unwrap();
    let after = sevenzip_blocks(&path, None);
    let names = vec!["unrelated.txt".to_owned()];
    assert_eq!(after[&names], before[&names]);
    assert!(
        !progress
            .lock()
            .unwrap()
            .iter()
            .any(|name| name == "unrelated.txt")
    );
    let output = export_entries(&info, &["a.txt".into(), "b.txt".into()], None, &control).unwrap();
    assert_eq!(fs::read(&output.paths[0]).unwrap(), b"first");
    assert_eq!(fs::read(&output.paths[1]).unwrap(), b"replaced");
}

#[test]
fn selected_sevenzip_extraction_skips_unrelated_blocks_and_solid_suffixes() {
    for mode in [IntegrityMode::Normal, IntegrityMode::Safe] {
        let temp = tempfile::tempdir().unwrap();
        let path = block_fixture(temp.path());
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let archive = Archive::read(&mut file, &Password::empty()).unwrap();
        let pack = archive.stream_map.block_first_pack_stream_index();
        // Corrupt the unselected suffix of the first solid block and the separate second block.
        for offset in [
            32 + archive.pack_pos() + archive.stream_map.pack_stream_offsets()[pack[0]] + 100_000,
            32 + archive.pack_pos() + archive.stream_map.pack_stream_offsets()[pack[1]] + 100_000,
        ] {
            file.seek(SeekFrom::Start(offset)).unwrap();
            file.write_all(&[0xFF]).unwrap();
        }
        drop(file);
        let control = JobControl::default().with_integrity(mode);
        let info = inspect_archive(&path, None, &control).unwrap();
        let output = export_entries(&info, &["a.txt".into()], None, &control).unwrap();
        assert_eq!(fs::read(&output.paths[0]).unwrap(), b"first");
        assert!(export_entries(&info, &["b.txt".into()], None, &control).is_err());
        assert!(export_entries(&info, &["unrelated.txt".into()], None, &control).is_err());
    }
}

#[test]
fn zip_raw_update_preserves_comments_and_unknown_extra_fields() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("metadata.zip");
    let mut writer = zip::ZipWriter::new(File::create(&path).unwrap());
    writer.set_comment("archive comment").unwrap();
    let mut options = zip::write::FullFileOptions::default().with_file_comment("entry comment");
    options
        .add_extra_data(0xbeef, b"custom".to_vec(), false)
        .unwrap();
    writer.start_file("keep.txt", options).unwrap();
    writer.write_all(b"kept").unwrap();
    writer
        .start_file("delete.txt", zip::write::SimpleFileOptions::default())
        .unwrap();
    writer.write_all(b"deleted").unwrap();
    writer.finish().unwrap();
    let info = inspect_archive(&path, None, &JobControl::default()).unwrap();
    edit_archive(
        &info,
        &ArchiveEdit::Delete {
            paths: vec!["delete.txt".into()],
        },
        None,
        &JobControl::default(),
    )
    .unwrap();
    let mut archive = zip::ZipArchive::new(File::open(path).unwrap()).unwrap();
    assert_eq!(archive.comment(), b"archive comment");
    let file = archive.by_name("keep.txt").unwrap();
    assert_eq!(file.comment(), "entry comment");
    assert!(
        file.extra_data()
            .unwrap()
            .windows(6)
            .any(|bytes| bytes == b"custom")
    );
}

#[test]
fn encrypted_sevenzip_with_plain_header_checks_password_before_copying_blocks() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("plain-header.7z");
    let mut writer = ArchiveWriter::new(File::create(&path).unwrap()).unwrap();
    writer.set_content_methods(vec![
        sevenz_rust2::encoder_options::AesEncoderOptions::new(Password::from("secret")).into(),
        EncoderConfiguration::new(EncoderMethod::COPY),
    ]);
    writer.set_encrypt_header(false);
    writer
        .push_archive_entry(
            ArchiveEntry::new_file("keep.txt"),
            Some(Cursor::new(b"encrypted data")),
        )
        .unwrap();
    writer.finish().unwrap();
    let before = fs::read(&path).unwrap();
    let blocks = sevenzip_blocks(&path, Some("secret"));
    let added = temp.path().join("new.txt");
    fs::write(&added, b"new data").unwrap();
    let info = inspect_archive(&path, None, &JobControl::default()).unwrap();
    let edit = ArchiveEdit::Add {
        sources: vec![added],
        directory: String::new(),
        overwrite: false,
    };
    assert!(edit_archive(&info, &edit, Some("wrong"), &JobControl::default()).is_err());
    assert_eq!(fs::read(&path).unwrap(), before);
    let info = edit_archive(&info, &edit, Some("secret"), &JobControl::default()).unwrap();
    let after = sevenzip_blocks(&path, Some("secret"));
    for (names, data) in blocks {
        assert_eq!(after[&names], data);
    }
    let output = export_entries(
        &info,
        &["keep.txt".into(), "new.txt".into()],
        Some("secret"),
        &JobControl::default(),
    )
    .unwrap();
    assert_eq!(fs::read(&output.paths[0]).unwrap(), b"encrypted data");
    assert_eq!(fs::read(&output.paths[1]).unwrap(), b"new data");
}

#[test]
fn zip_streaming_descriptors_zip64_and_legacy_encryption_survive_offset_changes() {
    use zip::unstable::write::FileOptionsExt;
    for encryption in 0..3 {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("streaming.zip");
        let mut writer = zip::ZipWriter::new_stream(File::create(&path).unwrap());
        writer
            .start_file("delete.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        writer
            .write_all(b"remove this local record before the kept record")
            .unwrap();
        let options = zip::write::SimpleFileOptions::default().large_file(true);
        let options = match encryption {
            1 => options.with_deprecated_encryption(b"secret").unwrap(),
            2 => options.with_aes_encryption(zip::AesMode::Aes256, "secret"),
            _ => options,
        };
        writer.start_file("keep.txt", options).unwrap();
        writer.write_all(b"streamed content").unwrap();
        writer.finish().unwrap();
        let before = zip_payloads(&path);
        let control = JobControl::default();
        let info = inspect_archive(&path, None, &control).unwrap();
        edit_archive(
            &info,
            &ArchiveEdit::Delete {
                paths: vec!["delete.txt".into()],
            },
            Some("secret"),
            &control,
        )
        .unwrap();
        assert_eq!(zip_payloads(&path)["keep.txt"], before["keep.txt"]);
        let mut archive = zip::ZipArchive::new(File::open(&path).unwrap()).unwrap();
        let mut file = archive.by_index_decrypt(0, b"secret").unwrap();
        let mut output = Vec::new();
        file.read_to_end(&mut output).unwrap();
        assert_eq!(output, b"streamed content");
    }
}
