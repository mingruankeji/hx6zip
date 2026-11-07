use std::{
    collections::HashSet,
    fs::{self, File},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, SystemTime},
};

use hx6zip::archive::{
    ArchiveFormat, CancellationToken, Cancelled, CreateOptions, ExtractOptions, JobControl,
    create_archive, extract_archive, inspect_archive,
};
use sevenz_rust2::{ArchiveReader, EncoderMethod, Password};
use tempfile::TempDir;

mod support;

const PARALLEL_FILE_SIZE: usize = 1024 * 1024;
const LARGE_SEVENZIP_SIZE: u64 = 32 * 1024 * 1024;

fn parallel_sources(root: &Path) -> Vec<PathBuf> {
    (0..4)
        .map(|index| {
            let path = root.join(format!("file-{index}.bin"));
            let contents = (0..PARALLEL_FILE_SIZE)
                .map(|offset| ((offset * 31 + index * 17) % 251) as u8)
                .collect::<Vec<_>>();
            fs::write(&path, contents).unwrap();
            path
        })
        .collect()
}

fn check_zip_methods(path: &Path, expected_method: u16) {
    let data = fs::read(path).unwrap();
    let read_u16 = |offset| u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap());
    let read_u32 = |offset| u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
    let footer = data
        .windows(4)
        .rposition(|bytes| bytes == b"PK\x05\x06")
        .unwrap();
    let count = read_u16(footer + 10);
    let mut central = read_u32(footer + 16) as usize;
    for _ in 0..count {
        assert_eq!(&data[central..central + 4], b"PK\x01\x02");
        let local = read_u32(central + 42) as usize;
        assert_eq!(&data[local..local + 4], b"PK\x03\x04");
        assert_eq!(read_u16(central + 10), expected_method);
        assert_eq!(read_u16(local + 8), expected_method);
        central += 46
            + usize::from(read_u16(central + 28))
            + usize::from(read_u16(central + 30))
            + usize::from(read_u16(central + 32));
    }
}

#[test]
fn parallel_archives_preserve_order_contents_and_encryption() {
    let temp = TempDir::new().unwrap();
    let sources = parallel_sources(temp.path());
    let modified = SystemTime::UNIX_EPOCH
        + Duration::from_secs(1_600_000_000)
        + Duration::from_nanos(123_456_700);
    for source in &sources {
        fs::OpenOptions::new()
            .write(true)
            .open(source)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();
    }
    for (format, password) in [
        (ArchiveFormat::Zip, None),
        (ArchiveFormat::Zip, Some("parallel-password")),
        (ArchiveFormat::SevenZip, None),
        (ArchiveFormat::SevenZip, Some("parallel-password")),
    ] {
        let suffix = format!(
            "{}-{}",
            format.extension(),
            if password.is_some() {
                "encrypted"
            } else {
                "plain"
            }
        );
        let archive = temp.path().join(format!("{suffix}.{}", format.extension()));
        let reports = Arc::new(Mutex::new(Vec::new()));
        let captured = reports.clone();
        let control = JobControl::new(CancellationToken::default(), move |progress| {
            captured.lock().unwrap().push(progress);
        });
        create_archive(
            &CreateOptions {
                sources: sources.clone(),
                destination: archive.clone(),
                format,
                password: password.map(str::to_owned),
                overwrite: false,
            },
            &control,
        )
        .unwrap();
        {
            let reports = reports.lock().unwrap();
            assert!(
                reports
                    .iter()
                    .all(|progress| progress.archive_size.is_some())
            );
            assert!(
                reports
                    .windows(2)
                    .all(|pair| pair[0].archive_size <= pair[1].archive_size)
            );
            let final_progress = reports.last().unwrap();
            assert_eq!(final_progress.total_bytes, 4 * PARALLEL_FILE_SIZE as u64);
            assert_eq!(final_progress.completed_bytes, final_progress.total_bytes);
            assert_eq!(
                final_progress.archive_size,
                Some(fs::metadata(&archive).unwrap().len())
            );
        }
        if format == ArchiveFormat::Zip {
            check_zip_methods(&archive, if password.is_some() { 99 } else { 8 });
        }
        let info = inspect_archive(&archive, password, &JobControl::default()).unwrap();
        assert_eq!(
            info.entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["file-0.bin", "file-1.bin", "file-2.bin", "file-3.bin"]
        );
        let expected_time = if format == ArchiveFormat::Zip && !cfg!(windows) {
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000)
        } else {
            modified
        };
        assert!(
            info.entries
                .iter()
                .all(|entry| entry.modified == Some(expected_time))
        );
        if format == ArchiveFormat::SevenZip && password.is_some() {
            let reader = ArchiveReader::open(&archive, Password::from(password.unwrap())).unwrap();
            let properties = reader
                .archive()
                .blocks
                .iter()
                .flat_map(|block| &block.coders)
                .filter(|coder| coder.encoder_method_id() == EncoderMethod::ID_AES256_SHA256)
                .map(|coder| coder.properties().to_vec())
                .collect::<Vec<_>>();
            assert_eq!(properties.len(), reader.archive().blocks.len());
            assert_eq!(
                properties.iter().collect::<HashSet<_>>().len(),
                properties.len()
            );
        }
        let output = temp.path().join(format!("output-{suffix}"));
        let extract_reports = Arc::new(Mutex::new(Vec::new()));
        let captured = extract_reports.clone();
        let extract_control = JobControl::new(CancellationToken::default(), move |progress| {
            captured.lock().unwrap().push(progress);
        });
        extract_archive(
            &info,
            &ExtractOptions {
                destination: output.clone(),
                password: password.map(str::to_owned),
                selected: None,
                overwrite: false,
            },
            &extract_control,
        )
        .unwrap();
        {
            let captured = extract_reports.lock().unwrap();
            let reports = captured
                .iter()
                .filter(|progress| progress.operation == "正在解压")
                .collect::<Vec<_>>();
            assert!(
                reports
                    .windows(2)
                    .all(|pair| pair[0].completed_bytes <= pair[1].completed_bytes)
            );
            let last = reports.last().unwrap();
            assert_eq!(last.completed_bytes, info.total_size);
            assert_eq!(last.total_bytes, info.total_size);
        }
        for source in &sources {
            assert_eq!(
                fs::read(output.join(source.file_name().unwrap())).unwrap(),
                fs::read(source).unwrap()
            );
        }

        if password.is_some() {
            let wrong_output = temp.path().join(format!("wrong-{suffix}"));
            fs::create_dir(&wrong_output).unwrap();
            fs::write(wrong_output.join("file-0.bin"), b"keep").unwrap();
            assert!(
                extract_archive(
                    &info,
                    &ExtractOptions {
                        destination: wrong_output.clone(),
                        password: Some("wrong".to_owned()),
                        selected: None,
                        overwrite: true,
                    },
                    &JobControl::default(),
                )
                .is_err()
            );
            assert_eq!(fs::read(wrong_output.join("file-0.bin")).unwrap(), b"keep");
        }
    }
}

#[test]
fn parallel_zip_extraction_cancellation_preserves_targets_and_removes_temporaries() {
    let temp = TempDir::new().unwrap();
    let sources = parallel_sources(temp.path());
    let archive = temp.path().join("cancel.zip");
    create_archive(
        &CreateOptions {
            sources: sources.clone(),
            destination: archive.clone(),
            format: ArchiveFormat::Zip,
            password: None,
            overwrite: false,
        },
        &JobControl::default(),
    )
    .unwrap();
    let info = inspect_archive(&archive, None, &JobControl::default()).unwrap();
    let output = temp.path().join("output");
    fs::create_dir(&output).unwrap();
    for source in &sources {
        fs::write(output.join(source.file_name().unwrap()), b"keep").unwrap();
    }
    let token = CancellationToken::default();
    let trigger = token.clone();
    let control = JobControl::new(token, move |_| trigger.cancel());
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
    for source in &sources {
        assert_eq!(
            fs::read(output.join(source.file_name().unwrap())).unwrap(),
            b"keep"
        );
    }
    assert_eq!(fs::read_dir(output).unwrap().count(), sources.len());
}

#[test]
fn parallel_zip_extraction_resumes_and_cancels_while_paused() {
    let temp = TempDir::new().unwrap();
    let sources = parallel_sources(temp.path());
    let archive = temp.path().join("pause.zip");
    create_archive(
        &CreateOptions {
            sources,
            destination: archive.clone(),
            format: ArchiveFormat::Zip,
            password: None,
            overwrite: false,
        },
        &JobControl::default(),
    )
    .unwrap();
    let info = inspect_archive(&archive, None, &JobControl::default()).unwrap();
    for cancel in [false, true] {
        let token = CancellationToken::default();
        let trigger = token.clone();
        let (paused_sender, paused_receiver) = mpsc::channel();
        let once = AtomicBool::new(false);
        let control = JobControl::new(token.clone(), move |_| {
            if !once.swap(true, Ordering::SeqCst) {
                trigger.set_paused(true);
                paused_sender.send(()).unwrap();
            }
        });
        let info = info.clone();
        let output = temp.path().join(if cancel { "cancel" } else { "resume" });
        let destination = output.clone();
        let (finished_sender, finished_receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            let result = extract_archive(
                &info,
                &ExtractOptions {
                    destination,
                    password: None,
                    selected: None,
                    overwrite: false,
                },
                &control,
            );
            finished_sender.send(result).unwrap();
        });
        if paused_receiver
            .recv_timeout(Duration::from_secs(10))
            .is_err()
        {
            token.cancel();
            worker.join().unwrap();
            panic!("extraction did not report progress");
        }
        assert!(finished_receiver.try_recv().is_err());
        if cancel {
            token.cancel();
        } else {
            token.set_paused(false);
        }
        let result = finished_receiver.recv_timeout(Duration::from_secs(10));
        if result.is_err() {
            token.cancel();
        }
        worker.join().unwrap();
        let result = result.unwrap();
        if cancel {
            assert!(result.unwrap_err().downcast_ref::<Cancelled>().is_some());
            assert!(!output.exists());
        } else {
            result.unwrap();
            assert_eq!(fs::read_dir(output).unwrap().count(), 4);
        }
    }
}

#[test]
fn parallel_zip_extraction_checks_crc_before_publishing_each_file() {
    use std::io::{Seek, SeekFrom, Write};
    use zip::{CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};

    let temp = TempDir::new().unwrap();
    let archive = temp.path().join("corrupt.zip");
    let mut writer = ZipWriter::new(File::create(&archive).unwrap());
    for name in ["bad.bin", "good.bin"] {
        writer
            .start_file(
                name,
                SimpleFileOptions::default().compression_method(CompressionMethod::Stored),
            )
            .unwrap();
        writer.write_all(b"original contents").unwrap();
    }
    writer.finish().unwrap();
    let mut reader = ZipArchive::new(File::open(&archive).unwrap()).unwrap();
    let offset = reader.by_index(0).unwrap().data_start().unwrap();
    drop(reader);
    let mut file = fs::OpenOptions::new().write(true).open(&archive).unwrap();
    file.seek(SeekFrom::Start(offset)).unwrap();
    file.write_all(b"X").unwrap();
    drop(file);
    let info = inspect_archive(&archive, None, &JobControl::default()).unwrap();
    let output = temp.path().join("output");
    fs::create_dir(&output).unwrap();
    fs::write(output.join("bad.bin"), b"keep").unwrap();
    let error = extract_archive(
        &info,
        &ExtractOptions {
            destination: output.clone(),
            password: None,
            selected: None,
            overwrite: true,
        },
        &JobControl::default(),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("bad.bin"), "{error:#}");
    assert_eq!(fs::read(output.join("bad.bin")).unwrap(), b"keep");
    assert!(!output.join("good.bin").exists());
    assert_eq!(fs::read_dir(output).unwrap().count(), 1);
}

#[test]
fn parallel_cancellation_keeps_existing_archive_and_cleans_up() {
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        let temp = TempDir::new().unwrap();
        let sources = parallel_sources(temp.path());
        let destination = temp.path().join(format!("existing.{}", format.extension()));
        fs::write(&destination, b"keep").unwrap();
        let cancellation = CancellationToken::default();
        let triggered = cancellation.clone();
        let control = JobControl::new(cancellation, move |progress| {
            if progress.current_file == "file-2.bin" {
                triggered.cancel();
            }
        });
        let error = create_archive(
            &CreateOptions {
                sources: sources.clone(),
                destination: destination.clone(),
                format,
                password: None,
                overwrite: true,
            },
            &control,
        )
        .unwrap_err();
        assert!(error.downcast_ref::<Cancelled>().is_some(), "{error:#}");
        assert_eq!(fs::read(destination).unwrap(), b"keep");
        assert_eq!(
            fs::read_dir(temp.path()).unwrap().count(),
            sources.len() + 1
        );
    }
}

#[test]
fn parallel_compression_detects_changed_sources_without_replacing_archive() {
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        let temp = TempDir::new().unwrap();
        let sources = parallel_sources(temp.path());
        let destination = temp.path().join(format!("existing.{}", format.extension()));
        fs::write(&destination, b"keep").unwrap();
        let changed = sources[2].clone();
        let once = Arc::new(AtomicBool::new(false));
        let changed_once = once.clone();
        let control = JobControl::new(CancellationToken::default(), move |progress| {
            if progress.current_file == "file-2.bin" && !changed_once.swap(true, Ordering::SeqCst) {
                fs::write(&changed, b"changed").unwrap();
            }
        });
        let error = create_archive(
            &CreateOptions {
                sources: sources.clone(),
                destination: destination.clone(),
                format,
                password: None,
                overwrite: true,
            },
            &control,
        )
        .unwrap_err();
        assert!(once.load(Ordering::SeqCst));
        assert!(format!("{error:#}").contains("变化"), "{error:#}");
        assert_eq!(fs::read(destination).unwrap(), b"keep");
        assert_eq!(
            fs::read_dir(temp.path()).unwrap().count(),
            sources.len() + 1
        );
    }
}

#[test]
fn chunked_zip_detects_large_file_changes_without_replacing_archive() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("large.bin");
    File::create(&source)
        .unwrap()
        .set_len(LARGE_SEVENZIP_SIZE)
        .unwrap();
    let destination = temp.path().join("existing.zip");
    fs::write(&destination, b"keep").unwrap();
    let changed = source.clone();
    let once = Arc::new(AtomicBool::new(false));
    let changed_once = once.clone();
    let control = JobControl::new(CancellationToken::default(), move |progress| {
        if progress.current_file == "large.bin" && !changed_once.swap(true, Ordering::SeqCst) {
            let file = fs::OpenOptions::new().write(true).open(&changed).unwrap();
            file.set_len(LARGE_SEVENZIP_SIZE + 1).unwrap();
        }
    });
    let error = create_archive(
        &CreateOptions {
            sources: vec![source],
            destination: destination.clone(),
            format: ArchiveFormat::Zip,
            password: None,
            overwrite: true,
        },
        &control,
    )
    .unwrap_err();
    assert!(once.load(Ordering::SeqCst));
    assert!(format!("{error:#}").contains("变化"), "{error:#}");
    assert_eq!(fs::read(destination).unwrap(), b"keep");
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 2);
}

#[test]
fn parallel_compression_can_resume_or_cancel_while_paused() {
    for (format, large_file) in [
        (ArchiveFormat::Zip, false),
        (ArchiveFormat::Zip, true),
        (ArchiveFormat::SevenZip, false),
        (ArchiveFormat::SevenZip, true),
    ] {
        for cancel in [false, true] {
            let temp = TempDir::new().unwrap();
            let sources = if large_file {
                let path = temp.path().join("large.bin");
                File::create(&path)
                    .unwrap()
                    .set_len(LARGE_SEVENZIP_SIZE)
                    .unwrap();
                vec![path]
            } else {
                parallel_sources(temp.path())
            };
            let destination = temp.path().join(format!("existing.{}", format.extension()));
            fs::write(&destination, b"keep").unwrap();
            let token = CancellationToken::default();
            let worker_token = token.clone();
            let progress_token = token.clone();
            let worker_sources = sources.clone();
            let worker_destination = destination.clone();
            let (paused_tx, paused_rx) = mpsc::channel();
            let (done_tx, done_rx) = mpsc::channel();
            let worker = thread::spawn(move || {
                let once = AtomicBool::new(false);
                let control = JobControl::new(worker_token, move |progress| {
                    if progress.completed_bytes > 0 && !once.swap(true, Ordering::SeqCst) {
                        progress_token.set_paused(true);
                        paused_tx.send(()).unwrap();
                    }
                });
                done_tx
                    .send(create_archive(
                        &CreateOptions {
                            sources: worker_sources,
                            destination: worker_destination,
                            format,
                            password: None,
                            overwrite: true,
                        },
                        &control,
                    ))
                    .unwrap();
            });
            paused_rx.recv_timeout(Duration::from_secs(60)).unwrap();
            assert!(matches!(
                done_rx.recv_timeout(Duration::from_millis(100)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ));
            if cancel {
                token.cancel();
            } else {
                token.set_paused(false);
            }
            let result = done_rx.recv_timeout(Duration::from_secs(60)).unwrap();
            worker.join().unwrap();
            if cancel {
                let error = result.unwrap_err();
                assert!(error.downcast_ref::<Cancelled>().is_some(), "{error:#}");
                assert_eq!(fs::read(&destination).unwrap(), b"keep");
            } else {
                result.unwrap();
                let info = inspect_archive(&destination, None, &JobControl::default()).unwrap();
                assert_eq!(info.entries.len(), sources.len());
            }
            assert_eq!(
                fs::read_dir(temp.path()).unwrap().count(),
                sources.len() + 1
            );
        }
    }
}

#[test]
fn large_sevenzip_round_trips_with_and_without_password() {
    let temp = TempDir::new().unwrap();
    let before = temp.path().join("before.txt");
    let large = temp.path().join("large.bin");
    let after = temp.path().join("after.txt");
    fs::write(&before, b"before").unwrap();
    File::create(&large)
        .unwrap()
        .set_len(LARGE_SEVENZIP_SIZE)
        .unwrap();
    fs::write(&after, b"after").unwrap();
    for password in [None, Some("parallel-password")] {
        let suffix = if password.is_some() {
            "encrypted"
        } else {
            "plain"
        };
        let archive = temp.path().join(format!("{suffix}.7z"));
        let latest = Arc::new(Mutex::new(None));
        let captured = latest.clone();
        let control = JobControl::new(CancellationToken::default(), move |progress| {
            *captured.lock().unwrap() = Some(progress);
        });
        create_archive(
            &CreateOptions {
                sources: vec![before.clone(), large.clone(), after.clone()],
                destination: archive.clone(),
                format: ArchiveFormat::SevenZip,
                password: password.map(str::to_owned),
                overwrite: false,
            },
            &control,
        )
        .unwrap();
        let progress = latest.lock().unwrap().take().unwrap();
        assert_eq!(progress.completed_bytes, LARGE_SEVENZIP_SIZE + 11);
        assert_eq!(progress.total_bytes, LARGE_SEVENZIP_SIZE + 11);
        assert_eq!(
            progress.archive_size,
            Some(fs::metadata(&archive).unwrap().len())
        );
        let info = inspect_archive(&archive, password, &JobControl::default()).unwrap();
        assert_eq!(info.entries.len(), 3);
        assert_eq!(info.total_size, LARGE_SEVENZIP_SIZE + 11);
        let output = temp.path().join(format!("output-{suffix}"));
        extract_archive(
            &info,
            &ExtractOptions {
                destination: output.clone(),
                password: password.map(str::to_owned),
                selected: None,
                overwrite: false,
            },
            &JobControl::default(),
        )
        .unwrap();
        assert_eq!(fs::read(output.join("before.txt")).unwrap(), b"before");
        assert_eq!(fs::read(output.join("after.txt")).unwrap(), b"after");
        let extracted = fs::read(output.join("large.bin")).unwrap();
        assert_eq!(extracted.len() as u64, LARGE_SEVENZIP_SIZE);
        assert!(extracted.iter().all(|byte| *byte == 0));
    }
}

#[test]
fn grouped_sevenzip_blocks_preserve_order_folders_and_empty_files() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("tree");
    let mut expected = Vec::new();
    for folder in 0..6 {
        let directory = root.join(format!("dir-{folder}"));
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("empty.txt"), b"").unwrap();
        for file in 0..40 {
            let name = format!("dir-{folder}/file-{file:02}.txt");
            let contents = format!("{name} {}", "x".repeat(file * 97)).into_bytes();
            fs::write(root.join(&name), &contents).unwrap();
            expected.push((name, contents));
        }
        // Enough data for several solid blocks, which are then compressed in parallel.
        let name = format!("dir-{folder}/large.bin");
        let contents = (0..3 * 1024 * 1024u32)
            .map(|index| (index % 251) as u8 ^ folder)
            .collect::<Vec<_>>();
        fs::write(root.join(&name), &contents).unwrap();
        expected.push((name, contents));
    }
    fs::create_dir(root.join("empty-dir")).unwrap();
    for password in [None, Some("grouped-password")] {
        let suffix = if password.is_some() {
            "encrypted"
        } else {
            "plain"
        };
        let archive = temp.path().join(format!("{suffix}.7z"));
        create_archive(
            &CreateOptions {
                sources: vec![root.clone()],
                destination: archive.clone(),
                format: ArchiveFormat::SevenZip,
                password: password.map(str::to_owned),
                overwrite: false,
            },
            &JobControl::default(),
        )
        .unwrap();
        let reader =
            ArchiveReader::open(&archive, password.map(Password::from).unwrap_or_default())
                .unwrap();
        let files = &reader.archive().files;
        let streams = files.iter().filter(|entry| entry.has_stream).count();
        assert_eq!(streams, expected.len());
        // Small files share solid blocks instead of getting one each.
        let blocks = reader.archive().blocks.len();
        assert!(blocks < streams / 4);
        assert!(blocks >= 2, "{blocks}");
        assert!(
            files
                .iter()
                .any(|entry| entry.name == "tree/empty-dir" && entry.is_directory)
        );
        assert!(
            files
                .iter()
                .any(|entry| entry.name == "tree/dir-0/empty.txt" && !entry.has_stream)
        );

        let info = inspect_archive(&archive, password, &JobControl::default()).unwrap();
        let output = temp.path().join(format!("output-{suffix}"));
        extract_archive(
            &info,
            &ExtractOptions {
                destination: output.clone(),
                password: password.map(str::to_owned),
                selected: None,
                overwrite: false,
            },
            &JobControl::default(),
        )
        .unwrap();
        let output = output.join("tree");
        for (name, contents) in &expected {
            assert_eq!(&fs::read(output.join(name)).unwrap(), contents, "{name}");
        }
        assert!(output.join("dir-0/empty.txt").is_file());
        assert!(output.join("empty-dir").is_dir());
    }
}

#[test]
fn sevenzip_folders_and_empty_files_do_not_split_solid_blocks() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("tree");
    let mut expected = Vec::new();
    for folder in 0..50 {
        let directory = root.join(format!("dir-{folder:02}"));
        fs::create_dir_all(directory.join("nested")).unwrap();
        fs::write(directory.join("empty.txt"), b"").unwrap();
        for file in 0..3 {
            let name = format!("dir-{folder:02}/nested/file-{file}.txt");
            let contents = format!("{name} {}", "y".repeat(file * 131)).into_bytes();
            fs::write(root.join(&name), &contents).unwrap();
            expected.push((name, contents));
        }
    }
    fs::create_dir(root.join("empty-dir")).unwrap();
    let archive = temp.path().join("tree.7z");
    create_archive(
        &CreateOptions {
            sources: vec![root.clone()],
            destination: archive.clone(),
            format: ArchiveFormat::SevenZip,
            password: None,
            overwrite: false,
        },
        &JobControl::default(),
    )
    .unwrap();
    let reader = ArchiveReader::open(&archive, Password::default()).unwrap();
    assert_eq!(
        reader
            .archive()
            .files
            .iter()
            .filter(|entry| entry.has_stream)
            .count(),
        expected.len()
    );
    // All the data fits one block, so the many folders must not split it.
    assert_eq!(reader.archive().blocks.len(), 1);

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
        &JobControl::default(),
    )
    .unwrap();
    let output = output.join("tree");
    for (name, contents) in &expected {
        assert_eq!(&fs::read(output.join(name)).unwrap(), contents, "{name}");
    }
    assert!(output.join("dir-49/empty.txt").is_file());
    assert!(output.join("empty-dir").is_dir());
}

#[test]
fn sevenzip_similar_files_share_a_dictionary() {
    let temp = TempDir::new().unwrap();
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let contents = (0..1024 * 1024)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect::<Vec<_>>();
    let mut changed = contents.clone();
    changed[10 * 1024] ^= 0xff;
    let sources = (0..12)
        .map(|index| {
            let path = temp.path().join(format!("similar-{index:02}.bin"));
            fs::write(&path, if index == 5 { &changed } else { &contents }).unwrap();
            path
        })
        .collect::<Vec<_>>();

    for password in [None, Some("similar-password")] {
        let suffix = if password.is_some() {
            "encrypted"
        } else {
            "plain"
        };
        let archive = temp.path().join(format!("similar-{suffix}.7z"));
        create_archive(
            &CreateOptions {
                sources: sources.clone(),
                destination: archive.clone(),
                format: ArchiveFormat::SevenZip,
                password: password.map(str::to_owned),
                overwrite: false,
            },
            &JobControl::default(),
        )
        .unwrap();
        let reader =
            ArchiveReader::open(&archive, password.map(Password::from).unwrap_or_default())
                .unwrap();
        assert_eq!(reader.archive().blocks.len(), 1);
        let lzma = reader.archive().blocks[0]
            .coders
            .iter()
            .find(|coder| coder.encoder_method_id() == EncoderMethod::ID_LZMA2)
            .unwrap();
        assert_eq!(lzma.properties(), &[16]);
        assert!(fs::metadata(&archive).unwrap().len() < contents.len() as u64 + 50 * 1024);
        let info = inspect_archive(&archive, password, &JobControl::default()).unwrap();
        let output = temp.path().join(format!("similar-output-{suffix}"));
        extract_archive(
            &info,
            &ExtractOptions {
                destination: output.clone(),
                password: password.map(str::to_owned),
                selected: None,
                overwrite: false,
            },
            &JobControl::default(),
        )
        .unwrap();
        for (index, source) in sources.iter().enumerate() {
            let expected = if index == 5 {
                changed.as_slice()
            } else {
                contents.as_slice()
            };
            assert_eq!(
                fs::read(output.join(source.file_name().unwrap()))
                    .unwrap()
                    .as_slice(),
                expected
            );
        }
    }
}

#[test]
fn sevenzip_does_not_group_unrelated_files_with_matching_samples() {
    let temp = TempDir::new().unwrap();
    let size = 1024 * 1024;
    let sample_size = 4 * 1024;
    let sources = (0..9)
        .map(|index| {
            let path = temp.path().join(format!("collision-{index:02}.dat"));
            let mut contents = vec![index as u8 + 1; size];
            for offset in [0, (size - sample_size) / 2, size - sample_size] {
                contents[offset..offset + sample_size].fill(0);
            }
            fs::write(&path, contents).unwrap();
            path
        })
        .collect::<Vec<_>>();
    let archive = temp.path().join("collision.7z");
    create_archive(
        &CreateOptions {
            sources: sources.clone(),
            destination: archive.clone(),
            format: ArchiveFormat::SevenZip,
            password: None,
            overwrite: false,
        },
        &JobControl::default(),
    )
    .unwrap();
    let reader = ArchiveReader::open(&archive, Password::default()).unwrap();
    let lzma = reader.archive().blocks[0]
        .coders
        .iter()
        .find(|coder| coder.encoder_method_id() == EncoderMethod::ID_LZMA2)
        .unwrap();
    assert!(lzma.properties()[0] > 16);

    let info = inspect_archive(&archive, None, &JobControl::default()).unwrap();
    let output = temp.path().join("collision-output");
    extract_archive(
        &info,
        &ExtractOptions {
            destination: output.clone(),
            password: None,
            selected: None,
            overwrite: false,
        },
        &JobControl::default(),
    )
    .unwrap();
    for source in &sources {
        assert_eq!(
            fs::read(output.join(source.file_name().unwrap())).unwrap(),
            fs::read(source).unwrap()
        );
    }
}

#[test]
fn sevenzip_filters_valid_pe_files_without_changing_other_files() {
    let temp = TempDir::new().unwrap();
    let files = [
        ("code.EXE", support::pe_fixture(0x8664, 128 * 1024)),
        ("library.dll", support::pe_fixture(0x014c, 96 * 1024)),
        ("arm64.exe", support::pe_fixture(0xaa64, 80 * 1024)),
        ("not-a-pe.exe", vec![b'x'; 64 * 1024]),
        ("no-filter.dat", support::pe_fixture(0x8664, 128 * 1024)),
        ("small.exe", support::pe_fixture(0x8664, 4096)),
        ("renamed.bin", support::pe_fixture(0x8664, 32 * 1024)),
        ("program", support::pe_fixture(0x8664, 20 * 1024)),
    ];
    let sources = files
        .iter()
        .map(|(name, contents)| {
            let path = temp.path().join(name);
            fs::write(&path, contents).unwrap();
            path
        })
        .collect::<Vec<_>>();

    for password in [None, Some("filtered-password")] {
        let suffix = if password.is_some() {
            "encrypted"
        } else {
            "plain"
        };
        let archive = temp.path().join(format!("filtered-{suffix}.7z"));
        create_archive(
            &CreateOptions {
                sources: sources.clone(),
                destination: archive.clone(),
                format: ArchiveFormat::SevenZip,
                password: password.map(str::to_owned),
                overwrite: false,
            },
            &JobControl::default(),
        )
        .unwrap();
        let reader =
            ArchiveReader::open(&archive, password.map(Password::from).unwrap_or_default())
                .unwrap();
        let archive_info = reader.archive();
        for (name, expected) in [
            ("code.EXE", Some(EncoderMethod::ID_BCJ_X86)),
            ("library.dll", Some(EncoderMethod::ID_BCJ_X86)),
            ("arm64.exe", Some(EncoderMethod::ID_BCJ_ARM64)),
            ("not-a-pe.exe", None),
            ("no-filter.dat", None),
            ("small.exe", None),
            ("renamed.bin", Some(EncoderMethod::ID_BCJ_X86)),
            ("program", Some(EncoderMethod::ID_BCJ_X86)),
        ] {
            let file_index = archive_info
                .files
                .iter()
                .position(|entry| entry.name == name)
                .unwrap();
            let block_index = archive_info.stream_map.file_block_index[file_index].unwrap();
            let methods = archive_info.blocks[block_index]
                .coders
                .iter()
                .map(|coder| coder.encoder_method_id())
                .collect::<Vec<_>>();
            if let Some(expected) = expected {
                assert!(methods.contains(&expected), "{name}: {methods:?}");
            } else {
                assert!(!methods.contains(&EncoderMethod::ID_BCJ_X86), "{name}");
                assert!(!methods.contains(&EncoderMethod::ID_BCJ_ARM64), "{name}");
            }
        }
        let info = inspect_archive(&archive, password, &JobControl::default()).unwrap();
        let output = temp.path().join(format!("filtered-output-{suffix}"));
        extract_archive(
            &info,
            &ExtractOptions {
                destination: output.clone(),
                password: password.map(str::to_owned),
                selected: None,
                overwrite: false,
            },
            &JobControl::default(),
        )
        .unwrap();
        for (name, contents) in &files {
            assert_eq!(&fs::read(output.join(name)).unwrap(), contents, "{name}");
        }
    }

    let filtered = temp.path().join("filtered-only.7z");
    let unfiltered = temp.path().join("unfiltered-only.7z");
    for (source, destination) in [(&sources[0], &filtered), (&sources[4], &unfiltered)] {
        create_archive(
            &CreateOptions {
                sources: vec![source.clone()],
                destination: destination.clone(),
                format: ArchiveFormat::SevenZip,
                password: None,
                overwrite: false,
            },
            &JobControl::default(),
        )
        .unwrap();
    }
    assert!(
        fs::metadata(filtered).unwrap().len() * 2 < fs::metadata(unfiltered).unwrap().len(),
        "BCJ should improve the branch-heavy test executable"
    );
}
