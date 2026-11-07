use std::{
    fs,
    sync::{
        Arc, Barrier, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use hx6zip::{
    archive::{
        ArchiveFormat, CancellationToken, Cancelled, JobControl, create_archive_auto,
        create_archives_separately, inspect_archive,
    },
    cli::automatic_archive_path,
};
use tempfile::TempDir;

#[test]
fn separate_zip_selection_keeps_each_item_in_its_own_directory_without_overwriting() {
    use std::io::Read;

    let temp = TempDir::new().unwrap();
    let other = temp.path().join("other");
    fs::create_dir(&other).unwrap();
    let sources = vec![
        temp.path().join("same.txt"),
        temp.path().join("same.bin"),
        other.join("资料.zip"),
    ];
    let existing = temp.path().join("same.zip");
    fs::write(&existing, b"existing").unwrap();
    for (index, source) in sources.iter().enumerate() {
        fs::write(source, format!("payload {index}")).unwrap();
    }
    let outputs = create_archives_separately(sources.clone(), &JobControl::default()).unwrap();
    assert_eq!(
        outputs,
        vec![
            fs::canonicalize(temp.path().join("same (1).zip")).unwrap(),
            fs::canonicalize(temp.path().join("same (2).zip")).unwrap(),
            fs::canonicalize(other.join("资料.zip.zip")).unwrap(),
        ]
    );
    for (index, (source, output)) in sources.iter().zip(outputs).enumerate() {
        let mut archive = zip::ZipArchive::new(fs::File::open(output).unwrap()).unwrap();
        assert_eq!(archive.len(), 1);
        let mut entry = archive.by_index(0).unwrap();
        assert_eq!(entry.name(), source.file_name().unwrap().to_str().unwrap());
        let mut data = String::new();
        entry.read_to_string(&mut data).unwrap();
        assert_eq!(data, format!("payload {index}"));
        assert_eq!(fs::read_to_string(source).unwrap(), data);
    }
    assert_eq!(fs::read(existing).unwrap(), b"existing");
}

#[test]
fn separate_zip_handles_empty_files_folders_and_nested_selection() {
    let temp = TempDir::new().unwrap();
    let folder = temp.path().join("folder");
    let empty = folder.join("empty");
    fs::create_dir_all(&empty).unwrap();
    let file = folder.join("zero.txt");
    fs::write(&file, []).unwrap();
    let outputs =
        create_archives_separately(vec![file, empty, folder], &JobControl::default()).unwrap();
    assert_eq!(outputs.len(), 3);
    let info = inspect_archive(&outputs[0], None, &JobControl::default()).unwrap();
    assert_eq!(
        info.entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        ["folder/", "folder/empty/", "folder/zero.txt"]
    );
    for output in &outputs[1..] {
        let info = inspect_archive(output, None, &JobControl::default()).unwrap();
        assert_eq!(info.entries.len(), 1);
        assert_eq!(info.total_size, 0);
    }
}

#[test]
fn separate_zip_preserves_completed_outputs_on_error_or_cancellation() {
    for cancel in [false, true] {
        let temp = TempDir::new().unwrap();
        let first = temp.path().join("first.txt");
        let second = temp.path().join("second.txt");
        fs::write(&first, b"first").unwrap();
        let token = CancellationToken::default();
        let progress_token = token.clone();
        fs::write(&second, vec![7; 1024 * 1024]).unwrap();
        let changed_source = second.clone();
        let control = JobControl::new(token, move |progress| {
            if progress.current_file.starts_with("[2 / 2]") {
                if cancel {
                    progress_token.cancel();
                } else {
                    fs::write(&changed_source, b"changed during compression").unwrap();
                }
            }
        });
        let error = create_archives_separately(vec![first, second], &control).unwrap_err();
        assert_eq!(error.downcast_ref::<Cancelled>().is_some(), cancel);
        assert!(format!("{error:#}").contains("已生成 1 个 ZIP"));
        let output = temp.path().join("first.zip");
        assert_eq!(
            inspect_archive(&output, None, &JobControl::default())
                .unwrap()
                .entries
                .len(),
            1
        );
        assert!(!temp.path().join("second.zip").exists());
        assert_eq!(
            fs::read_dir(temp.path()).unwrap().count(),
            3
        );
    }
    assert!(
        create_archives_separately(vec![], &JobControl::default())
            .unwrap_err()
            .to_string()
            .contains("请先选择")
    );
}

#[test]
fn automatic_selection_combines_files_and_folders_without_overwriting() {
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        let temp = TempDir::new().unwrap();
        let folder = temp.path().join("selection");
        fs::create_dir(&folder).unwrap();
        let first = folder.join("first.txt");
        let directory = folder.join("nested");
        fs::write(&first, b"first").unwrap();
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join("second.txt"), b"second").unwrap();
        let sources = vec![first, directory];
        let suggested = automatic_archive_path(&sources, format).unwrap();
        assert_eq!(
            suggested,
            folder.join(format!("selection.{}", format.extension()))
        );
        fs::write(&suggested, b"existing archive").unwrap();
        let output = create_archive_auto(sources, format, &JobControl::default()).unwrap();
        assert_eq!(
            output.file_name().unwrap(),
            format!("selection (1).{}", format.extension()).as_str()
        );
        let info = inspect_archive(&output, None, &JobControl::default()).unwrap();
        assert_eq!(info.entries.len(), 3);
        assert!(info.entries.iter().any(|entry| entry.name == "first.txt"));
        assert!(
            info.entries
                .iter()
                .any(|entry| entry.name == "nested/second.txt")
        );
        assert_eq!(fs::read(suggested).unwrap(), b"existing archive");
    }
}

#[test]
fn automatic_compression_reports_final_input_and_archive_sizes() {
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        for contents in [
            Vec::new(),
            b"a small source".to_vec(),
            vec![42; 1024 * 1024],
        ] {
            let temp = TempDir::new().unwrap();
            let source = temp.path().join("source.bin");
            fs::write(&source, &contents).unwrap();
            let reports = Arc::new(Mutex::new(Vec::new()));
            let captured = reports.clone();
            let control = JobControl::new(CancellationToken::default(), move |progress| {
                captured.lock().unwrap().push(progress);
            });

            let output = create_archive_auto(vec![source], format, &control).unwrap();
            let reports = reports.lock().unwrap();
            assert!(!reports.is_empty());
            assert!(
                reports
                    .iter()
                    .all(|progress| progress.archive_size.is_some())
            );
            let final_progress = reports.last().unwrap();
            assert_eq!(final_progress.total_bytes, contents.len() as u64);
            assert_eq!(final_progress.completed_bytes, contents.len() as u64);
            assert_eq!(
                final_progress.archive_size,
                Some(fs::metadata(output).unwrap().len())
            );
        }
    }
}

#[test]
fn concurrent_automatic_tasks_publish_distinct_archives() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source.zip");
    fs::write(&source, b"keep source").unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let workers: Vec<_> = (0..2)
        .map(|_| {
            let source = source.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                let waited = AtomicBool::new(false);
                let control = JobControl::new(CancellationToken::default(), move |_| {
                    if !waited.swap(true, Ordering::SeqCst) {
                        barrier.wait();
                    }
                });
                create_archive_auto(vec![source], ArchiveFormat::Zip, &control).unwrap()
            })
        })
        .collect();
    let outputs: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();
    assert_ne!(outputs[0], outputs[1]);
    for output in outputs {
        assert_eq!(
            inspect_archive(&output, None, &JobControl::default())
                .unwrap()
                .entries
                .len(),
            1
        );
    }
    assert_eq!(fs::read(source).unwrap(), b"keep source");
}

#[test]
fn compression_can_resume_or_cancel_while_paused() {
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        for cancel in [false, true] {
            let temp = TempDir::new().unwrap();
            let source = temp.path().join("source.bin");
            fs::write(&source, vec![42; 1024 * 1024]).unwrap();
            let token = CancellationToken::default();
            let worker_token = token.clone();
            let progress_token = token.clone();
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
                    .send(create_archive_auto(vec![source], format, &control))
                    .unwrap();
            });
            paused_rx.recv_timeout(Duration::from_secs(15)).unwrap();
            assert!(token.is_paused());
            assert!(matches!(
                done_rx.recv_timeout(Duration::from_millis(100)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ));
            if cancel {
                token.cancel();
            } else {
                token.set_paused(false);
            }
            let result = done_rx.recv_timeout(Duration::from_secs(15)).unwrap();
            worker.join().unwrap();
            if cancel {
                assert!(result.unwrap_err().downcast_ref::<Cancelled>().is_some());
                assert_eq!(
                    fs::read_dir(temp.path()).unwrap().count(),
                    1,
                    "cancel left a temporary archive"
                );
            } else {
                let path = result.unwrap();
                assert_eq!(
                    inspect_archive(&path, None, &JobControl::default())
                        .unwrap()
                        .total_size,
                    1024 * 1024
                );
            }
        }
    }
}
