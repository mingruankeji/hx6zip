use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{Cursor, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime},
};

use hx6zip::archive::{
    ArchiveFormat, CancellationToken, Cancelled, CreateOptions, ExtractOptions, IntegrityMode,
    JobControl,
    create_archive, extract_archive, inspect_archive,
};
use tempfile::TempDir;

fn sample(temp: &TempDir, format: ArchiveFormat, password: Option<&str>) -> PathBuf {
    let source = temp.path().join("input");
    fs::create_dir_all(source.join("empty")).unwrap();
    fs::write(source.join("first.bin"), vec![17; 192 * 1024]).unwrap();
    fs::write(source.join("second.bin"), vec![39; 192 * 1024]).unwrap();
    let archive = temp.path().join(format!("sample.{}", format.extension()));
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
    archive
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, (bool, Vec<u8>, SystemTime)> {
    walkdir::WalkDir::new(root)
        .into_iter()
        .map(|entry| {
            let entry = entry.unwrap();
            let metadata = fs::symlink_metadata(entry.path()).unwrap();
            (
                entry.path().strip_prefix(root).unwrap().to_owned(),
                (
                    metadata.is_dir(),
                    if metadata.is_file() {
                        fs::read(entry.path()).unwrap()
                    } else {
                        Vec::new()
                    },
                    metadata.modified().unwrap(),
                ),
            )
        })
        .collect()
}

fn existing_output(output: &Path) {
    fs::create_dir_all(output.join("input")).unwrap();
    for name in ["first.bin", "second.bin"] {
        fs::write(output.join("input").join(name), b"original").unwrap();
    }
    fs::write(output.join("unrelated.txt"), b"unrelated").unwrap();
}

fn assert_no_work(temp: &TempDir) {
    assert!(fs::read_dir(temp.path()).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".hx6zip-")
    }));
}

#[cfg(windows)]
#[test]
fn rollback_tracks_case_aliases_as_the_same_directory() {
    for existing in [false, true] {
        let temp = TempDir::new().unwrap();
        let archive = temp.path().join("case.zip");
        let mut writer = zip::ZipWriter::new(File::create(&archive).unwrap());
        writer
            .add_directory("Dir/", zip::write::SimpleFileOptions::default())
            .unwrap();
        for name in ["dir/first.bin", "DIR/second.bin"] {
            writer
                .start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(b"new").unwrap();
        }
        writer.finish().unwrap();
        let output = temp.path().join("output");
        if existing {
            fs::create_dir_all(output.join("dir")).unwrap();
            fs::write(output.join("dir").join("first.bin"), b"original").unwrap();
        }
        let before = existing.then(|| snapshot(&output));
        let info = inspect_archive(&archive, None, &JobControl::default()).unwrap();
        let token = CancellationToken::default();
        let cancel = token.clone();
        let control = JobControl::new(token, move |progress| {
            if progress.operation == "正在提交解压结果"
                && progress.current_file.ends_with("second.bin")
            {
                cancel.cancel();
            }
        });
        let error = extract_archive(
            &info,
            &ExtractOptions {
                destination: output.clone(),
                password: None,
                selected: None,
                overwrite: existing,
            },
            &control,
        )
        .unwrap_err();
        assert!(error.downcast_ref::<Cancelled>().is_some(), "{error:#}");
        if let Some(before) = before {
            assert_eq!(snapshot(&output), before);
        } else {
            assert!(!output.exists());
        }
        assert_no_work(&temp);
    }
}

#[test]
fn cancellation_after_first_publication_rolls_back_the_whole_archive() {
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        for existing in [false, true] {
            for password in [None, Some("backup-password")] {
                let temp = TempDir::new().unwrap();
                let archive = sample(&temp, format, password);
                let info = inspect_archive(&archive, password, &JobControl::default()).unwrap();
                let output = temp.path().join("new-parent").join("output");
                if existing {
                    existing_output(&output);
                }
                let before = existing.then(|| snapshot(&output));
                let token = CancellationToken::default();
                let cancel = token.clone();
                let observed = Arc::new(AtomicBool::new(false));
                let observed_progress = observed.clone();
                let target = output.clone();
                let control = JobControl::new(token, move |progress| {
                    if progress.operation == "正在提交解压结果"
                        && progress.current_file.ends_with("second.bin")
                    {
                        assert_eq!(
                            fs::read(target.join("input").join("first.bin")).unwrap(),
                            vec![17; 192 * 1024]
                        );
                        observed_progress.store(true, Ordering::SeqCst);
                        cancel.cancel();
                    }
                });
                let error = extract_archive(
                    &info,
                    &ExtractOptions {
                        destination: output.clone(),
                        password: password.map(str::to_owned),
                        selected: None,
                        overwrite: existing,
                    },
                    &control,
                )
                .unwrap_err();
                assert!(
                    observed.load(Ordering::SeqCst),
                    "did not reach second publication"
                );
                assert!(error.downcast_ref::<Cancelled>().is_some(), "{error:#}");
                if let Some(before) = before {
                    assert_eq!(snapshot(&output), before);
                } else {
                    assert!(!temp.path().join("new-parent").exists());
                }
                assert_no_work(&temp);
            }
        }
    }
}

#[test]
fn sequential_tar_cancellation_does_not_publish_completed_entries() {
    let temp = TempDir::new().unwrap();
    let archive = temp.path().join("sample.tar");
    let mut writer = tar::Builder::new(File::create(&archive).unwrap());
    for name in ["first.bin", "second.bin"] {
        let mut header = tar::Header::new_gnu();
        header.set_size(3);
        header.set_mode(0o644);
        header.set_cksum();
        writer
            .append_data(&mut header, name, Cursor::new(b"new"))
            .unwrap();
    }
    writer.finish().unwrap();
    drop(writer);
    let info = inspect_archive(&archive, None, &JobControl::default()).unwrap();
    let output = temp.path().join("output");
    fs::create_dir(&output).unwrap();
    fs::write(output.join("first.bin"), b"old").unwrap();
    let before = snapshot(&output);
    let token = CancellationToken::default();
    let cancel = token.clone();
    let control = JobControl::new(token, move |progress| {
        if progress.operation == "正在解压" && progress.current_file == "second.bin" {
            cancel.cancel();
        }
    });
    let error = extract_archive(
        &info,
        &ExtractOptions {
            destination: output.clone(),
            password: None,
            selected: None,
            overwrite: true,
        },
        &control,
    )
    .unwrap_err();
    assert!(error.downcast_ref::<Cancelled>().is_some(), "{error:#}");
    assert_eq!(snapshot(&output), before);
    assert_no_work(&temp);
}

#[cfg(windows)]
#[test]
fn locked_second_target_rolls_back_previously_replaced_files_and_directories() {
    use std::os::windows::fs::OpenOptionsExt;
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        let temp = TempDir::new().unwrap();
        let archive = sample(&temp, format, None);
        let info = inspect_archive(&archive, None, &JobControl::default()).unwrap();
        let output = temp.path().join("output");
        existing_output(&output);
        let before = snapshot(&output);
        let held = Arc::new(Mutex::new(None));
        let lock = held.clone();
        let target = output.clone();
        let control = JobControl::new(CancellationToken::default(), move |progress| {
            if progress.operation == "正在提交解压结果"
                && progress.current_file.ends_with("second.bin")
            {
                assert_eq!(
                    fs::read(target.join("input").join("first.bin")).unwrap(),
                    vec![17; 192 * 1024]
                );
                *lock.lock().unwrap() = Some(
                    fs::OpenOptions::new()
                        .read(true)
                        .share_mode(1)
                        .open(target.join("input").join("second.bin"))
                        .unwrap(),
                );
            }
        });
        let error = extract_archive(
            &info,
            &ExtractOptions {
                destination: output.clone(),
                password: None,
                selected: None,
                overwrite: true,
            },
            &control,
        )
        .unwrap_err();
        assert!(held.lock().unwrap().take().is_some());
        assert!(format!("{error:#}").contains("second.bin"), "{error:#}");
        assert_eq!(snapshot(&output), before);
        assert_no_work(&temp);
    }
}

#[test]
fn creation_readback_detects_same_size_content_changes_even_with_restored_timestamp() {
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source.bin");
        fs::write(&source, b"original").unwrap();
        let modified = fs::metadata(&source).unwrap().modified().unwrap();
        let destination = temp.path().join(format!("backup.{}", format.extension()));
        fs::write(&destination, b"previous backup").unwrap();
        let changed = source.clone();
        let observed = Arc::new(AtomicBool::new(false));
        let trigger = observed.clone();
        let control = JobControl::new(CancellationToken::default(), move |progress| {
            if progress.operation == "正在校验压缩包" && !trigger.swap(true, Ordering::SeqCst)
            {
                let mut file = File::create(&changed).unwrap();
                file.write_all(b"modified").unwrap();
                file.set_times(fs::FileTimes::new().set_modified(modified))
                    .unwrap();
            }
        });
        let error = create_archive(
            &CreateOptions {
                sources: vec![source],
                destination: destination.clone(),
                format,
                password: Some("backup-password".to_owned()),
                overwrite: true,
            },
            &control,
        )
        .unwrap_err();
        assert!(observed.load(Ordering::SeqCst));
        assert!(format!("{error:#}").contains("内容不一致"), "{error:#}");
        assert_eq!(fs::read(destination).unwrap(), b"previous backup");
        assert_no_work(&temp);
    }
}

#[test]
fn cancellation_during_creation_readback_never_publishes_an_archive() {
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        for existing in [false, true] {
            let temp = TempDir::new().unwrap();
            let source = temp.path().join("source.bin");
            fs::write(&source, vec![91; 128 * 1024]).unwrap();
            let destination = temp.path().join(format!("backup.{}", format.extension()));
            if existing {
                fs::write(&destination, b"previous backup").unwrap();
            }
            let token = CancellationToken::default();
            let cancel = token.clone();
            let control = JobControl::new(token, move |progress| {
                if progress.operation == "正在校验压缩包" {
                    cancel.cancel();
                }
            });
            let error = create_archive(
                &CreateOptions {
                    sources: vec![source],
                    destination: destination.clone(),
                    format,
                    password: None,
                    overwrite: existing,
                },
                &control,
            )
            .unwrap_err();
            assert!(error.downcast_ref::<Cancelled>().is_some(), "{error:#}");
            if existing {
                assert_eq!(fs::read(destination).unwrap(), b"previous backup");
            } else {
                assert!(!destination.exists());
            }
            assert_no_work(&temp);
        }
    }
}

#[test]
fn source_directory_changes_after_encoding_abort_creation() {
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("file.bin"), b"original").unwrap();
        let destination = temp.path().join(format!("backup.{}", format.extension()));
        let changed = source.clone();
        let once = AtomicBool::new(false);
        let control = JobControl::new(CancellationToken::default(), move |progress| {
            if progress.operation == "正在校验压缩包" && !once.swap(true, Ordering::SeqCst) {
                fs::write(changed.join("added.bin"), b"must not be omitted").unwrap();
                // Force a timestamp difference even on filesystems with coarse precision.
                let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(1_500_000_000);
                #[cfg(windows)]
                {
                    use std::os::windows::fs::OpenOptionsExt;
                    fs::OpenOptions::new()
                        .write(true)
                        .custom_flags(0x0200_0000)
                        .open(&changed)
                        .unwrap()
                        .set_times(fs::FileTimes::new().set_modified(modified))
                        .unwrap();
                }
                #[cfg(not(windows))]
                File::open(&changed)
                    .unwrap()
                    .set_times(fs::FileTimes::new().set_modified(modified))
                    .unwrap();
            }
        });
        assert!(
            create_archive(
                &CreateOptions {
                    sources: vec![source],
                    destination: destination.clone(),
                    format,
                    password: None,
                    overwrite: false,
                },
                &control
            )
            .is_err()
        );
        assert!(!destination.exists());
        assert_no_work(&temp);
    }
}

#[test]
fn normal_mode_keeps_entries_extracted_before_cancellation() {
    let temp = TempDir::new().unwrap();
    let archive = temp.path().join("sample.tar");
    let mut writer = tar::Builder::new(File::create(&archive).unwrap());
    for name in ["first.bin", "second.bin"] {
        let mut header = tar::Header::new_gnu();
        header.set_size(3);
        header.set_mode(0o644);
        header.set_cksum();
        writer
            .append_data(&mut header, name, Cursor::new(b"new"))
            .unwrap();
    }
    writer.finish().unwrap();
    drop(writer);
    let info = inspect_archive(&archive, None, &JobControl::default()).unwrap();
    let output = temp.path().join("output");
    let token = CancellationToken::default();
    let cancel = token.clone();
    let operations = Arc::new(Mutex::new(Vec::new()));
    let seen = operations.clone();
    let control = JobControl::new(token, move |progress| {
        seen.lock().unwrap().push(progress.operation);
        if progress.operation == "正在解压" && progress.current_file == "second.bin" {
            cancel.cancel();
        }
    })
    .with_integrity(IntegrityMode::Normal);
    let error = extract_archive(
        &info,
        &ExtractOptions {
            destination: output.clone(),
            password: None,
            selected: None,
            overwrite: false,
        },
        &control,
    )
    .unwrap_err();
    assert!(error.downcast_ref::<Cancelled>().is_some(), "{error:#}");
    assert_eq!(fs::read(output.join("first.bin")).unwrap(), b"new");
    assert!(!output.join("second.bin").exists());
    assert!(!operations.lock().unwrap().contains(&"正在提交解压结果"));
    assert_no_work(&temp);
}

#[test]
fn normal_mode_skips_readback_but_still_produces_identical_archives() {
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("input");
        fs::create_dir_all(source.join("empty")).unwrap();
        fs::write(source.join("first.bin"), vec![17; 192 * 1024]).unwrap();
        let archive = temp.path().join(format!("normal.{}", format.extension()));
        let operations = Arc::new(Mutex::new(Vec::new()));
        let seen = operations.clone();
        let control = JobControl::new(CancellationToken::default(), move |progress| {
            seen.lock().unwrap().push(progress.operation);
        })
        .with_integrity(IntegrityMode::Normal);
        assert_eq!(control.integrity(), IntegrityMode::Normal);
        create_archive(
            &CreateOptions {
                sources: vec![source.clone()],
                destination: archive.clone(),
                format,
                password: None,
                overwrite: false,
            },
            &control,
        )
        .unwrap();
        assert!(!operations.lock().unwrap().contains(&"正在校验压缩包"));

        let info = inspect_archive(&archive, None, &JobControl::default()).unwrap();
        let output = temp.path().join("output");
        extract_archive(
            &info,
            &ExtractOptions {
                destination: output.clone(),
                password: None,
                selected: None,
                overwrite: false,
            },
            &JobControl::default().with_integrity(IntegrityMode::Normal),
        )
        .unwrap();
        assert_eq!(
            fs::read(output.join("input").join("first.bin")).unwrap(),
            vec![17; 192 * 1024]
        );
        assert!(output.join("input").join("empty").is_dir());
        assert_no_work(&temp);
    }
}

#[test]
fn safe_mode_is_the_default_and_verifies_created_archives() {
    assert_eq!(JobControl::default().integrity(), IntegrityMode::Safe);
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("input.bin");
    fs::write(&source, b"payload").unwrap();
    let operations = Arc::new(Mutex::new(Vec::new()));
    let seen = operations.clone();
    create_archive(
        &CreateOptions {
            sources: vec![source],
            destination: temp.path().join("safe.zip"),
            format: ArchiveFormat::Zip,
            password: None,
            overwrite: false,
        },
        &JobControl::new(CancellationToken::default(), move |progress| {
            seen.lock().unwrap().push(progress.operation);
        }),
    )
    .unwrap();
    assert!(operations.lock().unwrap().contains(&"正在校验压缩包"));
}
