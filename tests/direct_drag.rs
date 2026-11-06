use std::{
    fs,
    io::Write,
    path::Path,
    sync::{Arc, Mutex},
};

use mzip::archive::{
    ArchiveFormat, ArchiveInfo, CancellationToken, CreateOptions, IntegrityMode, JobControl,
    create_archive, drag_conflicts, extract_drag_entries as extract_with_overwrite,
    inspect_archive,
};

fn extract_drag_entries(
    info: &ArchiveInfo,
    paths: &[String],
    destination: &Path,
    password: Option<&str>,
    control: &JobControl,
) -> anyhow::Result<()> {
    extract_with_overwrite(info, paths, destination, password, false, control)
}

fn fixture(root: &Path, format: ArchiveFormat, password: Option<&str>) -> ArchiveInfo {
    let source = root.join("tree");
    fs::create_dir_all(source.join("nested").join("empty")).unwrap();
    fs::write(source.join("nested").join("data.txt"), b"drag payload").unwrap();
    fs::write(source.join("keep.txt"), b"not selected").unwrap();
    let archive = root.join(format!("drag.{}", format.extension()));
    create_archive(
        &CreateOptions {
            sources: vec![source],
            destination: archive.clone(),
            format,
            password: password.map(str::to_owned),
            overwrite: false,
        },
        &JobControl::default(),
    )
    .unwrap();
    inspect_archive(&archive, password, &JobControl::default()).unwrap()
}

#[test]
fn sevenzip_drag_writes_selected_roots_without_export_copy() {
    check_selected_roots(ArchiveFormat::SevenZip);
}

#[test]
fn zip_drag_writes_selected_roots_without_export_copy() {
    check_selected_roots(ArchiveFormat::Zip);
}

fn check_selected_roots(format: ArchiveFormat) {
    for mode in [IntegrityMode::Normal, IntegrityMode::Safe] {
        for password in [None, Some("drag-password")] {
            let temp = tempfile::tempdir().unwrap();
            let info = fixture(temp.path(), format, password);
            let original = fs::read(&info.path).unwrap();
            let destination = temp.path().join("destination");
            fs::create_dir(&destination).unwrap();
            let phases = Arc::new(Mutex::new(Vec::new()));
            let seen = phases.clone();
            let root = temp.path().to_path_buf();
            let control = JobControl::new(CancellationToken::default(), move |progress| {
                seen.lock().unwrap().push(progress.operation);
                assert!(!fs::read_dir(&root).unwrap().any(|entry| {
                    entry
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .starts_with("mzip-export-")
                }));
            })
            .with_integrity(mode);
            extract_drag_entries(
                &info,
                &["tree/nested/data.txt".into(), "tree/nested".into()],
                &destination,
                password,
                &control,
            )
            .unwrap();
            assert_eq!(
                fs::read(destination.join("nested").join("data.txt")).unwrap(),
                b"drag payload"
            );
            assert!(destination.join("nested").join("empty").is_dir());
            assert!(!destination.join("tree").exists());
            assert!(!destination.join("keep.txt").exists());
            assert_eq!(fs::read(&info.path).unwrap(), original);
            assert_eq!(
                phases.lock().unwrap().contains(&"正在校验解压结果"),
                mode == IntegrityMode::Safe
            );
            assert!(!fs::read_dir(temp.path()).unwrap().any(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".mzip-extract-")
            }));
        }
    }
}

#[test]
fn sevenzip_drag_rejects_conflicts_stale_paths_and_changed_sources() {
    check_rejections(ArchiveFormat::SevenZip);
}

#[test]
fn zip_drag_rejects_conflicts_stale_paths_and_changed_sources() {
    check_rejections(ArchiveFormat::Zip);
}

fn check_rejections(format: ArchiveFormat) {
    for mode in [IntegrityMode::Normal, IntegrityMode::Safe] {
        let temp = tempfile::tempdir().unwrap();
        let info = fixture(temp.path(), format, None);
        let control = JobControl::default().with_integrity(mode);
        let destination = temp.path().join("destination");
        fs::create_dir_all(destination.join("nested")).unwrap();
        let existing = destination.join("nested").join("data.txt");
        fs::write(&existing, b"keep existing").unwrap();
        assert!(
            extract_drag_entries(&info, &["tree/nested".into()], &destination, None, &control,)
                .is_err()
        );
        assert_eq!(fs::read(&existing).unwrap(), b"keep existing");
        for paths in [
            vec!["../escape".into()],
            vec!["tree/nested".into(), "missing".into()],
        ] {
            let target = temp.path().join("invalid");
            assert!(extract_drag_entries(&info, &paths, &target, None, &control).is_err());
            assert!(!target.exists());
        }
        fs::OpenOptions::new()
            .append(true)
            .open(&info.path)
            .unwrap()
            .write_all(b"changed")
            .unwrap();
        let target = temp.path().join("stale");
        assert!(
            extract_drag_entries(&info, &["tree/nested".into()], &target, None, &control,).is_err()
        );
        assert!(!target.exists());
    }
}

#[test]
fn sevenzip_drag_safe_cancellation_rolls_back_and_wrong_password_writes_nothing() {
    check_cancellation(ArchiveFormat::SevenZip);
}

#[test]
fn zip_drag_safe_cancellation_rolls_back_and_wrong_password_writes_nothing() {
    check_cancellation(ArchiveFormat::Zip);
}

fn check_cancellation(format: ArchiveFormat) {
    let temp = tempfile::tempdir().unwrap();
    let info = fixture(temp.path(), format, Some("correct"));
    let target = temp.path().join("destination");
    fs::create_dir(&target).unwrap();
    let token = CancellationToken::default();
    let cancel = token.clone();
    let control = JobControl::new(token, move |progress| {
        if progress.operation == "正在提交解压结果" && progress.completed_bytes > 0 {
            cancel.cancel();
        }
    })
    .with_integrity(IntegrityMode::Safe);
    assert!(
        extract_drag_entries(&info, &["tree".into()], &target, Some("correct"), &control,).is_err()
    );
    assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
    assert!(
        extract_drag_entries(
            &info,
            &["tree".into()],
            &target,
            Some("wrong"),
            &JobControl::default(),
        )
        .is_err()
    );
    assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
}

#[test]
fn sevenzip_drag_flattens_independent_selections_and_refuses_duplicate_names() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("tree");
    for parent in ["one", "two"] {
        fs::create_dir_all(source.join(parent)).unwrap();
        fs::write(source.join(parent).join("same.txt"), parent).unwrap();
    }
    let archive = temp.path().join("drag.7z");
    create_archive(
        &CreateOptions {
            sources: vec![source],
            destination: archive.clone(),
            format: ArchiveFormat::SevenZip,
            password: None,
            overwrite: false,
        },
        &JobControl::default(),
    )
    .unwrap();
    let info = inspect_archive(&archive, None, &JobControl::default()).unwrap();
    let target = temp.path().join("destination");
    assert!(
        extract_drag_entries(
            &info,
            &["tree/one/same.txt".into(), "tree/two/same.txt".into()],
            &target,
            None,
            &JobControl::default(),
        )
        .is_err()
    );
    assert!(!target.exists());
    extract_drag_entries(
        &info,
        &["tree/one".into(), "tree/two".into()],
        &target,
        None,
        &JobControl::default(),
    )
    .unwrap();
    for parent in ["one", "two"] {
        assert_eq!(
            fs::read(target.join(parent).join("same.txt")).unwrap(),
            parent.as_bytes()
        );
    }
}

#[test]
fn sevenzip_drag_preserves_implicit_unicode_directories_and_rejects_corruption() {
    let temp = tempfile::tempdir().unwrap();
    let archive = temp.path().join("implicit.7z");
    let mut writer = sevenz_rust2::ArchiveWriter::create(&archive).unwrap();
    writer
        .push_archive_entry(
            sevenz_rust2::ArchiveEntry::new_file("parent/nested/\u{6587}\u{4ef6}.txt"),
            Some(std::io::Cursor::new(b"implicit payload")),
        )
        .unwrap();
    writer.finish().unwrap();
    let info = inspect_archive(&archive, None, &JobControl::default()).unwrap();
    let target = temp.path().join("destination");
    extract_drag_entries(
        &info,
        &["parent/nested".into()],
        &target,
        None,
        &JobControl::default(),
    )
    .unwrap();
    assert_eq!(
        fs::read(target.join("nested").join("\u{6587}\u{4ef6}.txt")).unwrap(),
        b"implicit payload"
    );
    let mut bytes = fs::read(&archive).unwrap();
    bytes[32] ^= 0xff;
    fs::write(&archive, bytes).unwrap();
    let corrupt = inspect_archive(&archive, None, &JobControl::default()).unwrap();
    let target = temp.path().join("corrupt");
    assert!(
        extract_drag_entries(
            &corrupt,
            &["parent/nested".into()],
            &target,
            None,
            &JobControl::default().with_integrity(IntegrityMode::Safe),
        )
        .is_err()
    );
    assert!(!target.exists());
}

#[test]
#[ignore = "creates a manual drag fixture at an explicitly supplied path"]
fn prepare_sevenzip_drag_probe() {
    let root =
        std::path::PathBuf::from(std::env::var_os("MZIP_DRAG_PROBE_ROOT").expect("probe root"));
    let archive = root.join("MZipDragProbe.7z");
    assert!(!archive.exists());
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("MZipDragProbe");
    fs::create_dir_all(source.join("empty")).unwrap();
    let mut file = fs::File::create(source.join("payload.bin")).unwrap();
    let mut seed = 0x6f13_9852_17ac_f839u64;
    let mut buffer = vec![0u8; 1024 * 1024];
    for _ in 0..64 {
        for bytes in buffer.chunks_exact_mut(8) {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            bytes.copy_from_slice(&seed.to_le_bytes());
        }
        file.write_all(&buffer).unwrap();
    }
    drop(file);
    fs::create_dir_all(root.join("destination").join("ExistingFolder")).unwrap();
    create_archive(
        &CreateOptions {
            sources: vec![source],
            destination: archive.clone(),
            format: ArchiveFormat::SevenZip,
            password: None,
            overwrite: false,
        },
        &JobControl::default(),
    )
    .unwrap();
    eprintln!("Manual 64 MiB drag fixture: {}", archive.display());
}

#[test]
fn zip_drag_crc_failure_never_publishes_the_corrupt_file() {
    use std::io::{Seek, SeekFrom};
    for mode in [IntegrityMode::Normal, IntegrityMode::Safe] {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("corrupt.zip");
        let mut writer = zip::ZipWriter::new(fs::File::create(&archive).unwrap());
        writer
            .start_file(
                "parent/nested/data.txt",
                zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored),
            )
            .unwrap();
        writer.write_all(b"payload").unwrap();
        writer.finish().unwrap();
        let mut reader = zip::ZipArchive::new(fs::File::open(&archive).unwrap()).unwrap();
        let offset = reader.by_index(0).unwrap().data_start().unwrap();
        drop(reader);
        let mut file = fs::OpenOptions::new().write(true).open(&archive).unwrap();
        file.seek(SeekFrom::Start(offset)).unwrap();
        file.write_all(b"X").unwrap();
        drop(file);
        let info = inspect_archive(&archive, None, &JobControl::default()).unwrap();
        let target = temp.path().join("destination");
        assert!(
            extract_drag_entries(
                &info,
                &["parent/nested".into()],
                &target,
                None,
                &JobControl::default().with_integrity(mode),
            )
            .is_err()
        );
        assert!(!target.join("nested").join("data.txt").exists());
        if mode == IntegrityMode::Safe {
            assert!(!target.exists());
        }
    }
}

#[test]
fn direct_replace_all_checks_conflicts_without_writes_and_preserves_other_files() {
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        for mode in [IntegrityMode::Normal, IntegrityMode::Safe] {
            let temp = tempfile::tempdir().unwrap();
            let info = fixture(temp.path(), format, None);
            let source = fs::read(&info.path).unwrap();
            let target = temp.path().join("destination");
            let paths = ["tree/nested".into(), "tree/keep.txt".into()];
            let control = JobControl::default().with_integrity(mode);
            assert!(
                drag_conflicts(&info, &paths, &target, &control)
                    .unwrap()
                    .is_empty()
            );
            assert!(!target.exists());
            fs::create_dir_all(target.join("nested")).unwrap();
            assert!(
                drag_conflicts(&info, &paths, &target, &control)
                    .unwrap()
                    .is_empty()
            );
            let first = target.join("nested").join("data.txt");
            let second = target.join("keep.txt");
            let unrelated = target.join("nested").join("unrelated.txt");
            fs::write(&first, b"old data").unwrap();
            fs::write(&second, b"old keep").unwrap();
            fs::write(&unrelated, b"do not touch").unwrap();
            let mut conflicts = drag_conflicts(&info, &paths, &target, &control).unwrap();
            conflicts.sort();
            let mut expected = vec![first.clone(), second.clone()];
            expected.sort();
            assert_eq!(conflicts, expected);
            assert_eq!(fs::read(&first).unwrap(), b"old data");
            assert_eq!(fs::read(&second).unwrap(), b"old keep");
            assert!(!target.join("nested").join("empty").exists());
            extract_with_overwrite(&info, &paths, &target, None, true, &control).unwrap();
            assert_eq!(fs::read(&first).unwrap(), b"drag payload");
            assert_eq!(fs::read(&second).unwrap(), b"not selected");
            assert_eq!(fs::read(&unrelated).unwrap(), b"do not touch");
            assert!(target.join("nested").join("empty").is_dir());
            assert_eq!(fs::read(&info.path).unwrap(), source);
        }
    }
}

#[test]
fn cancelling_safe_direct_replacement_restores_every_existing_file() {
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        let temp = tempfile::tempdir().unwrap();
        let info = fixture(temp.path(), format, None);
        let target = temp.path().join("destination");
        fs::create_dir_all(target.join("nested")).unwrap();
        let first = target.join("nested").join("data.txt");
        let second = target.join("keep.txt");
        fs::write(&first, b"old data").unwrap();
        fs::write(&second, b"old keep").unwrap();
        let token = CancellationToken::default();
        let cancel = token.clone();
        let control = JobControl::new(token, move |progress| {
            if progress.operation == "正在提交解压结果" && progress.completed_bytes > 0 {
                cancel.cancel();
            }
        })
        .with_integrity(IntegrityMode::Safe);
        assert!(
            extract_with_overwrite(
                &info,
                &["tree/nested".into(), "tree/keep.txt".into()],
                &target,
                None,
                true,
                &control,
            )
            .is_err()
        );
        assert_eq!(fs::read(&first).unwrap(), b"old data");
        assert_eq!(fs::read(&second).unwrap(), b"old keep");
        assert!(!target.join("nested").join("empty").exists());
    }
}

#[test]
fn direct_conflict_check_rejects_file_directory_type_conflicts() {
    let temp = tempfile::tempdir().unwrap();
    let info = fixture(temp.path(), ArchiveFormat::Zip, None);
    let target = temp.path().join("destination");
    fs::create_dir_all(target.join("nested").join("data.txt")).unwrap();
    let sentinel = target.join("nested").join("data.txt").join("keep");
    fs::write(&sentinel, b"preserve").unwrap();
    assert!(
        drag_conflicts(
            &info,
            &["tree/nested".into()],
            &target,
            &JobControl::default(),
        )
        .is_err()
    );
    assert_eq!(fs::read(&sentinel).unwrap(), b"preserve");
}

#[test]
#[ignore = "manual release-mode throughput comparison; writes generated test data only"]
fn compare_direct_and_regular_extraction_throughput() {
    use mzip::archive::{ExtractOptions, extract_archive};
    use std::time::Instant;

    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("benchmark");
    fs::create_dir(&source).unwrap();
    let mut payload = vec![0u8; 2 * 1024 * 1024];
    let mut random = 0x1234_5678u32;
    for chunk in payload.chunks_exact_mut(4) {
        random ^= random << 13;
        random ^= random >> 17;
        random ^= random << 5;
        chunk.copy_from_slice(&random.to_le_bytes());
    }

    for index in 0..16u64 {
        payload[..8].copy_from_slice(&index.to_le_bytes());
        fs::write(source.join(format!("{index}.bin")), &payload).unwrap();
    }
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        let archive = temp
            .path()
            .join(format!("benchmark.{}", format.extension()));
        create_archive(
            &CreateOptions {
                sources: vec![source.clone()],
                destination: archive.clone(),
                format,
                password: None,
                overwrite: false,
            },
            &JobControl::default().with_integrity(IntegrityMode::Normal),
        )
        .unwrap();
        let info = inspect_archive(&archive, None, &JobControl::default()).unwrap();
        for mode in [IntegrityMode::Normal, IntegrityMode::Safe] {
            let control = JobControl::default().with_integrity(mode);
            let mut regular = Vec::new();
            let mut direct = Vec::new();
            for round in 0..5 {
                // Warm both routes, then alternate order to reduce cache/order bias.
                for is_direct in [round % 2 == 0, round % 2 != 0] {
                    let output = tempfile::tempdir_in(temp.path()).unwrap();
                    let start = Instant::now();
                    if is_direct {
                        let paths = vec!["benchmark".to_owned()];
                        assert!(
                            drag_conflicts(&info, &paths, output.path(), &control)
                                .unwrap()
                                .is_empty()
                        );
                        extract_with_overwrite(&info, &paths, output.path(), None, false, &control)
                            .unwrap();
                    } else {
                        extract_archive(
                            &info,
                            &ExtractOptions {
                                destination: output.path().to_path_buf(),
                                password: None,
                                selected: None,
                                overwrite: false,
                            },
                            &control,
                        )
                        .unwrap();
                    }
                    let elapsed = start.elapsed().as_secs_f64();
                    if round > 0 {
                        if is_direct { &mut direct } else { &mut regular }.push(elapsed);
                    }
                    for index in 0..16u64 {
                        payload[..8].copy_from_slice(&index.to_le_bytes());
                        assert_eq!(
                            fs::read(output.path().join("benchmark").join(format!("{index}.bin")))
                                .unwrap(),
                            payload
                        );
                    }
                }
            }
            regular.sort_by(f64::total_cmp);
            direct.sort_by(f64::total_cmp);
            let regular_seconds = (regular[1] + regular[2]) / 2.0;
            let direct_seconds = (direct[1] + direct[2]) / 2.0;
            println!(
                "{format:?} {mode:?}: regular={regular_seconds:.3}s direct={direct_seconds:.3}s ratio={:.3}; 32 MiB, 16 files, four measured runs per route",
                direct_seconds / regular_seconds
            );
        }
    }
}
