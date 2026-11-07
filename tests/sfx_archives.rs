use std::{
    collections::BTreeSet,
    fs,
    io::{Cursor, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use hx6zip::archive::{
    ArchiveFormat, ArchiveKind, CancellationToken, Cancelled, CreateOptions, ExtractOptions,
    JobControl, create_archive, extract_archive, inspect_archive, named_destination,
};
use tempfile::TempDir;
use zip::{ZipWriter, write::SimpleFileOptions};

fn archive(directory: &Path, format: ArchiveFormat, password: Option<&str>) -> Vec<u8> {
    let first = directory.join("first.txt");
    let second = directory.join("second.txt");
    fs::write(&first, b"first payload").unwrap();
    fs::write(&second, b"second payload").unwrap();
    let path = directory.join(format!("source.{}", format.extension()));
    create_archive(
        &CreateOptions {
            sources: vec![first, second],
            destination: path.clone(),
            format,
            password: password.map(str::to_owned),
            overwrite: true,
        },
        &JobControl::default(),
    )
    .unwrap();
    fs::read(path).unwrap()
}

fn prefixed(directory: &Path, name: &str, prefix: &[u8], data: &[u8]) -> PathBuf {
    let path = directory.join(name);
    let mut output = fs::File::create(&path).unwrap();
    output.write_all(prefix).unwrap();
    output.write_all(data).unwrap();
    path
}

fn options(destination: &Path, password: Option<&str>) -> ExtractOptions {
    ExtractOptions {
        destination: destination.to_owned(),
        password: password.map(str::to_owned),
        selected: None,
        overwrite: false,
    }
}

#[test]
fn zip_and_sevenzip_sfx_work_without_relying_on_platform_or_extension() {
    let directory = TempDir::new().unwrap();
    for (format, expected) in [
        (ArchiveFormat::Zip, ArchiveKind::Zip),
        (ArchiveFormat::SevenZip, ArchiveKind::SevenZip),
    ] {
        let data = archive(directory.path(), format, None);
        for (index, (name, magic)) in [
            ("windows.EXE", b"MZ".as_slice()),
            ("linux.run", b"\x7fELF".as_slice()),
            ("mac-program", b"\xcf\xfa\xed\xfe".as_slice()),
            ("unknown.data", b"prefix".as_slice()),
            ("renamed", b"".as_slice()),
        ]
        .into_iter()
        .enumerate()
        {
            let mut prefix = magic.to_vec();
            if !prefix.is_empty() {
                prefix.resize(65533, 0);
            }
            let path = prefixed(directory.path(), name, &prefix, &data);
            let info = inspect_archive(&path, None, &JobControl::default()).unwrap();
            assert_eq!(info.format, expected);
            assert_eq!(info.archive_size, (prefix.len() + data.len()) as u64);
            assert_eq!(info.path, path);
            let output = directory
                .path()
                .join(format!("{}-{index}", format.extension()));
            extract_archive(&info, &options(&output, None), &JobControl::default()).unwrap();
            assert_eq!(
                fs::read(output.join("first.txt")).unwrap(),
                b"first payload"
            );
            assert_eq!(
                fs::read(output.join("second.txt")).unwrap(),
                b"second payload"
            );
            assert_eq!(fs::read(&path).unwrap(), [prefix, data.clone()].concat());
            assert_eq!(
                named_destination(&path, directory.path()).unwrap(),
                directory.path().join(path.file_stem().unwrap())
            );
        }
    }
}

#[test]
fn encrypted_sfx_preserves_password_selection_and_no_overwrite_rules() {
    let directory = TempDir::new().unwrap();
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        let data = archive(directory.path(), format, Some("secret"));
        let path = prefixed(directory.path(), "encrypted.exe", &[0; 1024], &data);
        if format == ArchiveFormat::SevenZip {
            assert!(
                inspect_archive(&path, None, &JobControl::default()).is_err(),
                "{data:?}"
            );
            assert!(inspect_archive(&path, Some("wrong"), &JobControl::default()).is_err());
        }
        let info = inspect_archive(&path, Some("secret"), &JobControl::default()).unwrap();
        let output = directory.path().join(format.extension());
        let mut settings = options(&output, Some("wrong"));
        settings.selected = Some(BTreeSet::from([info
            .entries
            .iter()
            .find(|entry| entry.name == "second.txt")
            .unwrap()
            .index]));
        assert!(extract_archive(&info, &settings, &JobControl::default()).is_err());
        assert!(!output.join("second.txt").exists());
        settings.password = Some("secret".to_owned());
        extract_archive(&info, &settings, &JobControl::default()).unwrap();
        assert!(!output.join("first.txt").exists());
        assert_eq!(
            fs::read(output.join("second.txt")).unwrap(),
            b"second payload"
        );
        fs::write(output.join("second.txt"), b"keep existing").unwrap();
        assert!(extract_archive(&info, &settings, &JobControl::default()).is_err());
        assert_eq!(
            fs::read(output.join("second.txt")).unwrap(),
            b"keep existing"
        );
    }
}

#[test]
fn zip_sfx_accepts_absolute_offsets_and_prefers_outer_archive() {
    let directory = TempDir::new().unwrap();
    let inner = archive(directory.path(), ArchiveFormat::SevenZip, None);
    for absolute_offsets in [false, true] {
        let prefix = vec![0; 512];
        let mut cursor = Cursor::new(if absolute_offsets {
            prefix.clone()
        } else {
            Vec::new()
        });
        cursor.seek(SeekFrom::End(0)).unwrap();
        let mut writer = ZipWriter::new(cursor);
        writer
            .start_file(
                "nested.7z",
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored),
            )
            .unwrap();
        writer.write_all(&inner).unwrap();
        let data = writer.finish().unwrap().into_inner();
        let path = prefixed(
            directory.path(),
            "zip.exe",
            if absolute_offsets { &[] } else { &prefix },
            &data,
        );
        let info = inspect_archive(&path, None, &JobControl::default()).unwrap();
        assert_eq!(info.format, ArchiveKind::Zip);
        let output = directory
            .path()
            .join(format!("absolute-{absolute_offsets}"));
        extract_archive(&info, &options(&output, None), &JobControl::default()).unwrap();
        assert_eq!(fs::read(output.join("nested.7z")).unwrap(), inner);
    }
}

#[test]
fn sfx_rejects_plain_corrupt_traversal_and_changed_files() {
    let directory = TempDir::new().unwrap();
    for data in [
        b"MZ ordinary executable".as_slice(),
        b"plain text".as_slice(),
        b"7z\xbc\xaf\x27\x1c".as_slice(),
        b"Rar!\x1a\x07\x01\x00".as_slice(),
        b"PK\x03\x04".as_slice(),
    ] {
        let path = prefixed(directory.path(), "not-an-archive.exe", &[], data);
        assert!(inspect_archive(&path, None, &JobControl::default()).is_err());
    }
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    writer
        .start_file("../escape.txt", SimpleFileOptions::default())
        .unwrap();
    writer.write_all(b"unsafe").unwrap();
    let path = prefixed(
        directory.path(),
        "unsafe.exe",
        b"MZ stub",
        &writer.finish().unwrap().into_inner(),
    );
    let info = inspect_archive(&path, None, &JobControl::default()).unwrap();
    let output = directory.path().join("output");
    assert!(extract_archive(&info, &options(&output, None), &JobControl::default()).is_err());
    assert!(!output.exists());
    assert!(!directory.path().join("escape.txt").exists());

    let data = archive(directory.path(), ArchiveFormat::SevenZip, None);
    let path = prefixed(directory.path(), "changed.exe", b"MZ stub", &data);
    let info = inspect_archive(&path, None, &JobControl::default()).unwrap();
    prefixed(directory.path(), "changed.exe", b"changed prefix", &data);
    assert!(extract_archive(&info, &options(&output, None), &JobControl::default()).is_err());
    assert!(!output.exists());
}

#[test]
fn sevenzip_scan_checks_prefix_limit_false_signatures_and_cancellation() {
    let directory = TempDir::new().unwrap();
    let data = archive(directory.path(), ArchiveFormat::SevenZip, None);
    for size in [4 * 1024 * 1024, 4 * 1024 * 1024 + 1] {
        let mut prefix = vec![0; size];
        prefix[..6].copy_from_slice(b"7z\xbc\xaf\x27\x1c");
        let path = prefixed(directory.path(), "limit.exe", &prefix, &data);
        let result = inspect_archive(&path, None, &JobControl::default());
        assert_eq!(result.is_ok(), size == 4 * 1024 * 1024);
    }
    let token = CancellationToken::default();
    token.cancel();
    let control = JobControl::new(token, |_| {});
    let error = inspect_archive(&directory.path().join("limit.exe"), None, &control).unwrap_err();
    assert!(error.is::<Cancelled>());
}

#[test]
#[ignore = "requires 7-Zip on PATH; creates but never executes a real SFX"]
fn real_7zip_sfx_is_read_without_executing_it() {
    let directory = TempDir::new().unwrap();
    fs::write(directory.path().join("payload.txt"), b"real SFX payload").unwrap();
    let result = std::process::Command::new("7z")
        .current_dir(directory.path())
        .args(["a", "-sfx", "sample.exe", "payload.txt"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let path = directory.path().join("sample.exe");
    let info = inspect_archive(&path, None, &JobControl::default()).unwrap();
    assert_eq!(info.format, ArchiveKind::SevenZip);
    let output = directory.path().join("output");
    extract_archive(&info, &options(&output, None), &JobControl::default()).unwrap();
    assert_eq!(
        fs::read(output.join("payload.txt")).unwrap(),
        b"real SFX payload"
    );
}
