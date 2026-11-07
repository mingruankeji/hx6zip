use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{Cursor, Write},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use hx6zip::archive::{
    ArchiveFormat, CancellationToken, Cancelled, CreateOptions, ExtractOptions, JobControl,
    create_archive, extract_archive, extract_conflicts, inspect_archive, named_destination,
    smart_destination,
};
use tempfile::TempDir;
use zip::{ZipWriter, write::SimpleFileOptions};

fn write(path: &Path, contents: &[u8]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
}

fn create_sample(directory: &TempDir, format: ArchiveFormat, password: Option<&str>) -> PathBuf {
    let source = directory.path().join("资料");
    write(&source.join("alpha.txt"), b"alpha content");
    write(&source.join("nested").join("beta.txt"), b"beta content");
    fs::create_dir_all(source.join("empty")).unwrap();
    let archive = directory
        .path()
        .join(format!("sample.{}", format.extension()));
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

fn extract(
    archive: &Path,
    destination: &Path,
    password: Option<&str>,
    selected: Option<BTreeSet<usize>>,
    overwrite: bool,
) -> anyhow::Result<()> {
    let info = inspect_archive(archive, password, &JobControl::default())?;
    extract_archive(
        &info,
        &ExtractOptions {
            destination: destination.to_path_buf(),
            password: password.map(str::to_owned),
            selected,
            overwrite,
        },
        &JobControl::default(),
    )
}

#[test]
fn named_extraction_uses_archive_name_even_for_a_single_root() {
    let directory = TempDir::new().unwrap();
    let archive = create_sample(&directory, ArchiveFormat::Zip, None);
    let info = inspect_archive(&archive, None, &JobControl::default()).unwrap();
    let base = directory.path().join("named");
    let destination = named_destination(&info.path, &base).unwrap();
    assert_eq!(destination, base.join("sample"));
    extract(&archive, &destination, None, None, false).unwrap();
    assert_eq!(
        fs::read(destination.join("资料").join("alpha.txt")).unwrap(),
        b"alpha content"
    );
    for (name, stem) in [
        ("mxbgl.zip", "mxbgl"),
        ("资料.7z", "资料"),
        ("data.tar.gz", "data"),
        ("data.TAR.XZ", "data"),
        ("data.tgz", "data"),
        ("data.tar.zip", "data.tar"),
    ] {
        assert_eq!(
            named_destination(Path::new(name), &base).unwrap(),
            base.join(stem)
        );
    }
}

#[test]
fn zip_and_sevenzip_round_trip_unicode_directories_and_empty_folders() {
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        let directory = TempDir::new().unwrap();
        let archive = create_sample(&directory, format, None);
        let info = inspect_archive(&archive, None, &JobControl::default()).unwrap();
        assert!(
            info.entries
                .iter()
                .any(|entry| entry.name.ends_with("beta.txt"))
        );
        assert!(
            info.entries
                .iter()
                .any(|entry| entry.name.contains("empty"))
        );
        assert_eq!(info.total_size, 25);

        let destination = directory.path().join("output");
        extract(&archive, &destination, None, None, false).unwrap();
        assert_eq!(
            fs::read(destination.join("资料").join("alpha.txt")).unwrap(),
            b"alpha content"
        );
        assert_eq!(
            fs::read(destination.join("资料").join("nested").join("beta.txt")).unwrap(),
            b"beta content"
        );
        assert!(destination.join("资料").join("empty").is_dir());
    }
}

#[test]
fn encrypted_archives_require_the_correct_password_and_keep_existing_files() {
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        let directory = TempDir::new().unwrap();
        let archive = create_sample(&directory, format, Some("秘密-password"));
        let output = directory.path().join("output");
        write(&output.join("资料").join("alpha.txt"), b"original");

        let bad = extract(&archive, &output, Some("wrong"), None, true);
        assert!(bad.is_err(), "{format:?} accepted an incorrect password");
        assert_eq!(
            fs::read(output.join("资料").join("alpha.txt")).unwrap(),
            b"original"
        );

        extract(&archive, &output, Some("秘密-password"), None, true).unwrap();
        assert_eq!(
            fs::read(output.join("资料").join("alpha.txt")).unwrap(),
            b"alpha content"
        );
    }
}

#[test]
fn selecting_a_directory_extracts_its_contents_but_not_siblings() {
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        let directory = TempDir::new().unwrap();
        let archive = create_sample(&directory, format, None);
        let info = inspect_archive(&archive, None, &JobControl::default()).unwrap();
        let selected = info
            .entries
            .iter()
            .find(|entry| {
                entry.is_directory && entry.name.trim_end_matches('/').ends_with("nested")
            })
            .unwrap()
            .index;
        let output = directory.path().join("selected");
        extract(
            &archive,
            &output,
            None,
            Some(BTreeSet::from([selected])),
            false,
        )
        .unwrap();
        assert!(
            output
                .join("资料")
                .join("nested")
                .join("beta.txt")
                .is_file()
        );
        assert!(!output.join("资料").join("alpha.txt").exists());
    }
}

#[test]
fn directory_selection_matches_windows_case_insensitively() {
    let directory = TempDir::new().unwrap();
    let archive = directory.path().join("case.zip");
    let mut writer = ZipWriter::new(File::create(&archive).unwrap());
    writer
        .add_directory("Dir/", SimpleFileOptions::default())
        .unwrap();
    writer
        .start_file("dir/child.txt", SimpleFileOptions::default())
        .unwrap();
    writer.write_all(b"child").unwrap();
    writer.finish().unwrap();
    let info = inspect_archive(&archive, None, &JobControl::default()).unwrap();
    let directory_index = info
        .entries
        .iter()
        .find(|entry| entry.is_directory)
        .unwrap()
        .index;
    let output = directory.path().join("case-output");
    extract(
        &archive,
        &output,
        None,
        Some(BTreeSet::from([directory_index])),
        false,
    )
    .unwrap();
    assert_eq!(
        fs::read(output.join("dir").join("child.txt")).unwrap(),
        b"child"
    );
}

fn set_modified(path: &Path, time: SystemTime, directory: bool) {
    let mut options = fs::OpenOptions::new();
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.write(true);
        if directory {
            options.custom_flags(0x0200_0000);
        }
    }
    #[cfg(not(windows))]
    {
        let _ = directory;
        options.read(true);
    }
    options
        .open(path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(time))
        .unwrap();
}

#[test]
fn preserves_file_and_directory_modification_times() {
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        let directory = TempDir::new().unwrap();
        let source = directory.path().join("source");
        let file = source.join("file.txt");
        write(&file, b"timestamped");
        let timestamp = SystemTime::UNIX_EPOCH
            + Duration::from_secs(1_600_000_000)
            + Duration::from_nanos(123_456_700);
        let expected = if cfg!(windows) || format == ArchiveFormat::SevenZip {
            timestamp
        } else {
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000)
        };
        set_modified(&file, timestamp, false);
        set_modified(&source, timestamp, true);

        let archive = directory
            .path()
            .join(format!("time.{}", format.extension()));
        create_archive(
            &CreateOptions {
                sources: vec![source],
                destination: archive.clone(),
                format,
                password: None,
                overwrite: false,
            },
            &JobControl::default(),
        )
        .unwrap();
        let info = inspect_archive(&archive, None, &JobControl::default()).unwrap();
        let recorded = info
            .entries
            .iter()
            .find(|entry| entry.name.ends_with("file.txt"))
            .unwrap()
            .modified
            .unwrap();
        assert_eq!(recorded, expected, "{format:?} timestamp in archive");
        let output = directory.path().join("output");
        extract(&archive, &output, None, None, false).unwrap();
        assert_eq!(
            fs::metadata(output.join("source").join("file.txt"))
                .unwrap()
                .modified()
                .unwrap(),
            expected,
            "{format:?} file timestamp"
        );
        assert_eq!(
            fs::metadata(output.join("source"))
                .unwrap()
                .modified()
                .unwrap(),
            expected,
            "{format:?} directory timestamp"
        );
    }
}

#[test]
fn refuses_to_overwrite_unless_explicitly_requested() {
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        let directory = TempDir::new().unwrap();
        let archive = create_sample(&directory, format, None);
        let output = directory.path().join("output");
        let target = output.join("资料").join("alpha.txt");
        write(&target, b"old value");

        assert!(extract(&archive, &output, None, None, false).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"old value");
        extract(&archive, &output, None, None, true).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"alpha content");
    }
}

#[test]
fn extraction_conflict_preflight_checks_only_selected_files_without_writing() {
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        let directory = TempDir::new().unwrap();
        let archive = create_sample(&directory, format, None);
        let info = inspect_archive(&archive, None, &JobControl::default()).unwrap();
        let output = directory.path().join("output");
        let alpha = output.join("资料").join("alpha.txt");
        let beta = output.join("资料").join("nested").join("beta.txt");
        let selected = info
            .entries
            .iter()
            .find(|entry| {
                entry.is_directory && entry.name.trim_end_matches('/').ends_with("nested")
            })
            .unwrap()
            .index;
        let options = ExtractOptions {
            destination: output.clone(),
            password: None,
            selected: Some(BTreeSet::from([selected])),
            overwrite: false,
        };
        assert!(
            extract_conflicts(&info, &options, &JobControl::default())
                .unwrap()
                .is_empty()
        );
        assert!(!output.exists());
        write(&alpha, b"unselected");
        fs::create_dir_all(beta.parent().unwrap()).unwrap();
        assert!(
            extract_conflicts(&info, &options, &JobControl::default())
                .unwrap()
                .is_empty()
        );
        write(&beta, b"existing");
        assert_eq!(
            extract_conflicts(&info, &options, &JobControl::default()).unwrap(),
            vec![beta.clone()]
        );
        assert_eq!(fs::read(&alpha).unwrap(), b"unselected");
        assert_eq!(fs::read(&beta).unwrap(), b"existing");
        assert!(!output.join("资料").join("empty").exists());
        let all_options = ExtractOptions {
            selected: None,
            ..options
        };
        let mut conflicts = extract_conflicts(&info, &all_options, &JobControl::default()).unwrap();
        conflicts.sort();
        let mut expected = vec![alpha, beta];
        expected.sort();
        assert_eq!(conflicts, expected);
    }
}

#[test]
fn extraction_conflict_preflight_rejects_file_directory_collisions() {
    let directory = TempDir::new().unwrap();
    let archive = create_sample(&directory, ArchiveFormat::Zip, None);
    let info = inspect_archive(&archive, None, &JobControl::default()).unwrap();
    let output = directory.path().join("output");
    let sentinel = output.join("资料").join("alpha.txt").join("keep");
    write(&sentinel, b"keep");
    let options = ExtractOptions {
        destination: output,
        password: None,
        selected: None,
        overwrite: false,
    };
    assert!(extract_conflicts(&info, &options, &JobControl::default()).is_err());
    assert_eq!(fs::read(sentinel).unwrap(), b"keep");
}

#[test]
fn extraction_cannot_replace_the_archive_it_is_reading() {
    let directory = TempDir::new().unwrap();
    let archive = directory.path().join("archive.zip");
    malicious_zip(&archive, &["archive.zip"]);
    let original = fs::read(&archive).unwrap();
    let info = inspect_archive(&archive, None, &JobControl::default()).unwrap();
    let options = ExtractOptions {
        destination: directory.path().to_path_buf(),
        password: None,
        selected: None,
        overwrite: false,
    };
    assert!(
        format!(
            "{:#}",
            extract_conflicts(&info, &options, &JobControl::default()).unwrap_err()
        )
        .contains("不能用解压结果覆盖正在读取的压缩包")
    );

    let error = extract(&archive, directory.path(), None, None, true).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("不能用解压结果覆盖正在读取的压缩包")
    );
    assert_eq!(fs::read(&archive).unwrap(), original);
}

fn malicious_zip(path: &Path, names: &[&str]) {
    let mut writer = ZipWriter::new(File::create(path).unwrap());
    for name in names {
        writer
            .start_file(*name, SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"unsafe").unwrap();
    }
    writer.finish().unwrap();
}

#[test]
fn rejects_traversal_absolute_paths_device_names_and_conflicting_paths() {
    for names in [
        vec!["../outside.txt"],
        vec!["folder\\..\\outside.txt"],
        vec!["C:\\outside.txt"],
        vec!["folder/file:stream"],
        vec!["CON.txt"],
        vec!["a.txt", "A.TXT"],
    ] {
        let directory = TempDir::new().unwrap();
        let archive = directory.path().join("unsafe.zip");
        malicious_zip(&archive, &names);
        let output = directory.path().join("output");
        assert!(
            extract(&archive, &output, None, None, true).is_err(),
            "{names:?}"
        );
        assert!(!directory.path().join("outside.txt").exists());
    }
}

#[test]
fn rejects_unsafe_paths_in_sevenzip() {
    let directory = TempDir::new().unwrap();
    let archive = directory.path().join("unsafe.7z");
    let mut writer = sevenz_rust2::ArchiveWriter::create(&archive).unwrap();
    writer
        .push_archive_entry(
            sevenz_rust2::ArchiveEntry::new_file("../outside.txt"),
            Some(Cursor::new(b"unsafe")),
        )
        .unwrap();
    writer.finish().unwrap();
    assert!(extract(&archive, &directory.path().join("output"), None, None, true).is_err());
    assert!(!directory.path().join("outside.txt").exists());
}

#[cfg(windows)]
#[test]
fn follows_a_junction_inside_the_destination() {
    use std::{os::windows::process::CommandExt, process::Command};

    let directory = TempDir::new().unwrap();
    let outside = directory.path().join("outside");
    let output = directory.path().join("output");
    fs::create_dir_all(&outside).unwrap();
    fs::create_dir_all(&output).unwrap();
    let link = output.join("link");
    let result = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&link)
        .arg(&outside)
        .creation_flags(0x08000000)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );

    let archive = directory.path().join("unsafe.zip");
    malicious_zip(&archive, &["link/escaped.txt"]);
    let info = inspect_archive(&archive, None, &JobControl::default()).unwrap();
    let options = ExtractOptions {
        destination: output.clone(),
        password: None,
        selected: None,
        overwrite: false,
    };
    assert!(
        extract_conflicts(&info, &options, &JobControl::default())
            .unwrap()
            .is_empty()
    );
    extract(&archive, &output, None, None, true).unwrap();
    assert_eq!(fs::read(outside.join("escaped.txt")).unwrap(), b"unsafe");
    fs::remove_dir(&link).unwrap();
}

#[test]
fn cancellation_does_not_replace_an_existing_archive() {
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        let directory = TempDir::new().unwrap();
        let first = directory.path().join("first.txt");
        let second = directory.path().join("second.txt");
        write(&first, b"first");
        write(&second, b"second");
        let destination = directory
            .path()
            .join(format!("target.{}", format.extension()));
        write(&destination, b"existing archive");
        let cancellation = CancellationToken::default();
        let triggered = cancellation.clone();
        let control = JobControl::new(cancellation, move |progress| {
            if progress.current_file == "second.txt" {
                triggered.cancel();
            }
        });
        let error = create_archive(
            &CreateOptions {
                sources: vec![first, second],
                destination: destination.clone(),
                format,
                password: None,
                overwrite: true,
            },
            &control,
        )
        .unwrap_err();
        assert!(error.downcast_ref::<Cancelled>().is_some(), "{error:#}");
        assert_eq!(fs::read(&destination).unwrap(), b"existing archive");
    }
}

#[test]
fn cancellation_keeps_files_from_previous_successful_extractions() {
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        let directory = TempDir::new().unwrap();
        let source = directory.path().join("input");
        write(&source.join("a.txt"), b"completed");
        write(&source.join("b.txt"), b"new value");
        let archive = directory
            .path()
            .join(format!("cancel.{}", format.extension()));
        create_archive(
            &CreateOptions {
                sources: vec![source],
                destination: archive.clone(),
                format,
                password: None,
                overwrite: false,
            },
            &JobControl::default(),
        )
        .unwrap();
        let info = inspect_archive(&archive, None, &JobControl::default()).unwrap();
        let output = directory.path().join("output");
        let target = output.join("input").join("b.txt");
        write(&target, b"old value");
        let first = info
            .entries
            .iter()
            .find(|entry| entry.name.ends_with("a.txt"))
            .unwrap()
            .index;
        extract(
            &archive,
            &output,
            None,
            Some(BTreeSet::from([first])),
            false,
        )
        .unwrap();
        let cancellation = CancellationToken::default();
        let triggered = cancellation.clone();
        let control = JobControl::new(cancellation, move |progress| {
            if progress.current_file.ends_with("b.txt") {
                triggered.cancel();
            }
        });
        let error = extract_archive(
            &info,
            &ExtractOptions {
                destination: output.clone(),
                password: None,
                selected: Some(BTreeSet::from([info
                    .entries
                    .iter()
                    .find(|entry| entry.name.ends_with("b.txt"))
                    .unwrap()
                    .index])),
                overwrite: true,
            },
            &control,
        )
        .unwrap_err();
        assert!(error.downcast_ref::<Cancelled>().is_some(), "{error:#}");
        assert_eq!(
            fs::read(output.join("input").join("a.txt")).unwrap(),
            b"completed"
        );
        assert_eq!(fs::read(target).unwrap(), b"old value");
        assert_eq!(fs::read_dir(output.join("input")).unwrap().count(), 2);
    }
}

#[test]
fn archives_can_be_saved_inside_their_source_folder() {
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        let directory = TempDir::new().unwrap();
        let source = directory.path().join("source");
        write(&source.join("file.txt"), b"data");
        let inside_source = source.join(format!("archive.{}", format.extension()));
        for overwrite in [false, true] {
            create_archive(
                &CreateOptions {
                    sources: vec![source.clone()],
                    destination: inside_source.clone(),
                    format,
                    password: None,
                    overwrite,
                },
                &JobControl::default(),
            )
            .unwrap();
            let info = inspect_archive(&inside_source, None, &JobControl::default()).unwrap();
            let names = info
                .entries
                .iter()
                .map(|entry| entry.name.trim_end_matches('/').to_owned())
                .collect::<BTreeSet<_>>();
            assert_eq!(
                names,
                BTreeSet::from(["source".to_owned(), "source/file.txt".to_owned()]),
                "{format:?}"
            );
        }
    }
}

#[test]
fn an_archive_can_replace_a_selected_source_file() {
    let directory = TempDir::new().unwrap();
    let archive = create_sample(&directory, ArchiveFormat::Zip, None);
    let original = fs::read(&archive).unwrap();
    create_archive(
        &CreateOptions {
            sources: vec![archive.clone()],
            destination: archive.clone(),
            format: ArchiveFormat::Zip,
            password: None,
            overwrite: true,
        },
        &JobControl::default(),
    )
    .unwrap();
    let info = inspect_archive(&archive, None, &JobControl::default()).unwrap();
    assert_eq!(info.entries.len(), 1);
    let output = directory.path().join("output");
    extract(&archive, &output, None, None, false).unwrap();
    assert_eq!(fs::read(output.join("sample.zip")).unwrap(), original);
}

#[test]
fn source_and_archive_changes_are_reported() {
    let directory = TempDir::new().unwrap();
    let archive = create_sample(&directory, ArchiveFormat::Zip, None);
    let info = inspect_archive(&archive, None, &JobControl::default()).unwrap();
    malicious_zip(&archive, &["different.txt"]);
    let error = extract_archive(
        &info,
        &ExtractOptions {
            destination: directory.path().join("out"),
            password: None,
            selected: None,
            overwrite: false,
        },
        &JobControl::default(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("发生了变化"), "{error:#}");
}

#[test]
fn changes_during_creation_do_not_replace_the_destination() {
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        for remove_directory in [false, true] {
            let directory = TempDir::new().unwrap();
            let source = if remove_directory {
                let path = directory.path().join("source");
                fs::create_dir(&path).unwrap();
                path
            } else {
                let path = directory.path().join("source.txt");
                write(&path, b"before");
                path
            };
            let destination = directory
                .path()
                .join(format!("target.{}", format.extension()));
            write(&destination, b"existing archive");
            let changed = source.clone();
            let control = JobControl::new(CancellationToken::default(), move |_| {
                if remove_directory {
                    fs::remove_dir(&changed).unwrap();
                } else {
                    fs::write(&changed, b"after!").unwrap();
                    set_modified(
                        &changed,
                        SystemTime::UNIX_EPOCH + Duration::from_secs(1_500_000_000),
                        false,
                    );
                }
            });
            let error = create_archive(
                &CreateOptions {
                    sources: vec![source],
                    destination: destination.clone(),
                    format,
                    password: None,
                    overwrite: true,
                },
                &control,
            )
            .unwrap_err();
            assert!(
                error.to_string().contains("变化") || error.to_string().contains("不存在"),
                "{error:#}"
            );
            assert_eq!(fs::read(destination).unwrap(), b"existing archive");
        }
    }
}

#[test]
fn smart_destination_avoids_an_extra_folder_for_a_single_root() {
    let directory = TempDir::new().unwrap();
    let archive = create_sample(&directory, ArchiveFormat::Zip, None);
    let info = inspect_archive(&archive, None, &JobControl::default()).unwrap();
    assert_eq!(
        smart_destination(&info, directory.path()).unwrap(),
        directory.path()
    );

    let multi = directory.path().join("multi.zip");
    malicious_zip(&multi, &["first.txt", "second.txt"]);
    let info = inspect_archive(&multi, None, &JobControl::default()).unwrap();
    assert_eq!(
        smart_destination(&info, directory.path()).unwrap(),
        directory.path().join("multi")
    );
}
