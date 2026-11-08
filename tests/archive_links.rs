use std::{
    fs,
    path::{Path, PathBuf},
};

use hx6zip::archive::{
    ArchiveEdit, ArchiveFormat, CancellationToken, Cancelled, CreateOptions, ExtractOptions,
    IntegrityMode, JobControl, create_archive, create_archives_separately, edit_archive,
    extract_archive, extract_conflicts, import_conflicts, inspect_archive,
};
use tempfile::TempDir;

struct DirectoryLink(PathBuf);

impl DirectoryLink {
    fn new(path: &Path, target: &Path) -> Self {
        #[cfg(windows)]
        {
            use std::{os::windows::process::CommandExt, process::Command};
            let result = Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(path)
                .arg(target)
                .creation_flags(0x08000000)
                .output()
                .unwrap();
            assert!(result.status.success(), "{result:?}");
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, path).unwrap();
        Self(path.to_owned())
    }
}

impl Drop for DirectoryLink {
    fn drop(&mut self) {
        #[cfg(windows)]
        fs::remove_dir(&self.0).unwrap();
        #[cfg(unix)]
        fs::remove_file(&self.0).unwrap();
    }
}

fn options(source: &Path, destination: &Path, format: ArchiveFormat) -> CreateOptions {
    CreateOptions {
        sources: vec![source.to_owned()],
        destination: destination.to_owned(),
        format,
        password: None,
        overwrite: false,
    }
}

#[test]
fn directory_links_are_packed_as_contents_in_both_formats_and_modes() {
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        for mode in [IntegrityMode::Normal, IntegrityMode::Safe] {
            let temp = TempDir::new().unwrap();
            let source = temp.path().join("source");
            let target = temp.path().join("dependency");
            fs::create_dir_all(&source).unwrap();
            fs::create_dir_all(target.join("empty")).unwrap();
            fs::write(target.join("index.js"), b"package contents").unwrap();
            let _first = DirectoryLink::new(&source.join("first"), &target);
            let _second = DirectoryLink::new(&source.join("second"), &target);
            let control = JobControl::default().with_integrity(mode);
            for root in [&source, &source.join("first")] {
                let archive = temp.path().join(format!(
                    "{}.{}",
                    root.file_name().unwrap().to_string_lossy(),
                    format.extension()
                ));
                create_archive(&options(root, &archive, format), &control).unwrap();
                let info = inspect_archive(&archive, None, &control).unwrap();
                assert!(
                    info.entries
                        .iter()
                        .all(|e| !e.is_symbolic_link && !e.unsupported_kind)
                );
                let output = temp
                    .path()
                    .join(root.file_name().unwrap())
                    .with_extension("out");
                extract_archive(
                    &info,
                    &ExtractOptions {
                        destination: output.clone(),
                        password: None,
                        selected: None,
                        overwrite: false,
                    },
                    &control,
                )
                .unwrap();
                let unpacked = output.join(root.file_name().unwrap());
                let folders = if root == &source {
                    vec![unpacked.join("first"), unpacked.join("second")]
                } else {
                    vec![unpacked]
                };
                for folder in folders {
                    assert_eq!(
                        fs::read(folder.join("index.js")).unwrap(),
                        b"package contents"
                    );
                    assert!(folder.join("empty").is_dir());
                    assert!(
                        !fs::symlink_metadata(&folder)
                            .unwrap()
                            .file_type()
                            .is_symlink()
                    );
                }
            }
        }
    }
}

#[test]
fn adding_directory_links_uses_contents_and_detects_conflicts() {
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        for mode in [IntegrityMode::Normal, IntegrityMode::Safe] {
            let temp = TempDir::new().unwrap();
            let seed = temp.path().join("seed.txt");
            fs::write(&seed, b"seed").unwrap();
            let archive = temp.path().join(format!("test.{}", format.extension()));
            let control = JobControl::default().with_integrity(mode);
            create_archive(&options(&seed, &archive, format), &control).unwrap();
            let info = inspect_archive(&archive, None, &control).unwrap();
            let target = temp.path().join("dependency");
            fs::create_dir_all(&target).unwrap();
            fs::write(target.join("index.js"), b"dependency").unwrap();
            let link = temp.path().join("package");
            let _link = DirectoryLink::new(&link, &target);
            let edit = ArchiveEdit::Add {
                sources: vec![link.clone()],
                directory: "node_modules".into(),
                overwrite: false,
            };
            assert!(
                import_conflicts(&info, &[link.clone()], "node_modules", &control)
                    .unwrap()
                    .is_empty()
            );
            let updated = edit_archive(&info, &edit, None, &control).unwrap();
            assert!(
                !import_conflicts(&updated, &[link], "node_modules", &control)
                    .unwrap()
                    .is_empty()
            );
            let output = temp.path().join("out");
            extract_archive(
                &updated,
                &ExtractOptions {
                    destination: output.clone(),
                    password: None,
                    selected: None,
                    overwrite: false,
                },
                &control,
            )
            .unwrap();
            assert_eq!(
                fs::read(output.join("node_modules").join("package").join("index.js")).unwrap(),
                b"dependency"
            );
        }
    }
}

#[test]
fn cyclic_links_do_not_replace_archives() {
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        for mode in [IntegrityMode::Normal, IntegrityMode::Safe] {
            let temp = TempDir::new().unwrap();
            let source = temp.path().join("source");
            fs::create_dir_all(&source).unwrap();
            let _link = DirectoryLink::new(&source.join("link"), &source);
            let archive = temp.path().join(format!("test.{}", format.extension()));
            fs::write(&archive, b"original archive").unwrap();
            let mut options = options(&source, &archive, format);
            options.overwrite = true;
            let control = JobControl::default().with_integrity(mode);
            let error = create_archive(&options, &control).unwrap_err();
            assert!(!format!("{error:#}").is_empty());
            assert_eq!(fs::read(&archive).unwrap(), b"original archive");
            assert!(!fs::read_dir(temp.path()).unwrap().any(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".hx6zip-")
            }));
        }
    }
}

#[test]
fn dangling_directory_links_use_manifest_without_warnings_and_keep_readable_contents() {
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        for mode in [IntegrityMode::Normal, IntegrityMode::Safe] {
            let temp = TempDir::new().unwrap();
            let source = temp.path().join("source");
            let target = temp.path().join("target");
            fs::create_dir(&source).unwrap();
            fs::create_dir(&target).unwrap();
            fs::write(source.join("ok.txt"), b"readable content").unwrap();
            let link = source.join("broken");
            let _link = DirectoryLink::new(&link, &target);
            fs::remove_dir(&target).unwrap();
            for root in [&source, &link] {
                let archive = temp.path().join(format!(
                    "{}.{}",
                    root.file_name().unwrap().to_string_lossy(),
                    format.extension()
                ));
                let control = JobControl::default().with_integrity(mode);
                create_archive(&options(root, &archive, format), &control).unwrap();
                assert!(control.warnings().is_empty(), "{:?}", control.warnings());
                let info = inspect_archive(&archive, None, &control).unwrap();
                assert!(info.entries.iter().all(|e| {
                    let name = if root == &source {
                        "source/broken"
                    } else {
                        "broken"
                    };
                    e.name.trim_end_matches('/') != name
                }));
                assert!(
                    info.entries
                        .iter()
                        .any(|e| e.name == "Special_Cases_List.mhx6zip")
                );
                if root == &source {
                    let output = temp.path().join("out");
                    extract_archive(
                        &info,
                        &ExtractOptions {
                            destination: output.clone(),
                            password: None,
                            selected: None,
                            overwrite: false,
                        },
                        &control,
                    )
                    .unwrap();
                    assert_eq!(
                        fs::read(output.join("source").join("ok.txt")).unwrap(),
                        b"readable content"
                    );
                    assert_eq!(
                        fs::read_link(output.join("source").join("broken")).unwrap(),
                        fs::read_link(&link).unwrap()
                    );
                }
            }
        }
    }
}

#[test]
fn adding_dangling_directory_links_uses_manifest_without_warnings_or_losing_entries() {
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        for mode in [IntegrityMode::Normal, IntegrityMode::Safe] {
            let temp = TempDir::new().unwrap();
            let seed = temp.path().join("seed.txt");
            fs::write(&seed, b"existing").unwrap();
            let archive = temp.path().join(format!("test.{}", format.extension()));
            let control = JobControl::default().with_integrity(mode);
            create_archive(&options(&seed, &archive, format), &control).unwrap();
            let info = inspect_archive(&archive, None, &control).unwrap();
            let source = temp.path().join("source");
            fs::create_dir(&source).unwrap();
            fs::write(source.join("ok.txt"), b"new content").unwrap();
            let target = temp.path().join("target");
            fs::create_dir(&target).unwrap();
            let _link = DirectoryLink::new(&source.join("broken"), &target);
            fs::remove_dir(&target).unwrap();
            assert!(
                import_conflicts(&info, &[source.clone()], "", &control)
                    .unwrap()
                    .is_empty()
            );
            let info = edit_archive(
                &info,
                &ArchiveEdit::Add {
                    sources: vec![source],
                    directory: String::new(),
                    overwrite: false,
                },
                None,
                &control,
            )
            .unwrap();
            assert!(control.warnings().is_empty(), "{:?}", control.warnings());
            let output = temp.path().join("out");
            extract_archive(
                &info,
                &ExtractOptions {
                    destination: output.clone(),
                    password: None,
                    selected: None,
                    overwrite: false,
                },
                &control,
            )
            .unwrap();
            assert_eq!(fs::read(output.join("seed.txt")).unwrap(), b"existing");
            assert_eq!(
                fs::read(output.join("source").join("ok.txt")).unwrap(),
                b"new content"
            );
            assert_eq!(
                fs::read_link(output.join("source").join("broken")).unwrap(),
                fs::read_link(_link.0.clone()).unwrap()
            );
        }
    }
}

#[test]
fn missing_sources_warn_including_separate_archive_batches() {
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        for mode in [IntegrityMode::Normal, IntegrityMode::Safe] {
            let temp = TempDir::new().unwrap();
            let missing = temp.path().join("missing.txt");
            let archive = temp.path().join(format!("test.{}", format.extension()));
            let control = JobControl::default().with_integrity(mode);
            create_archive(&options(&missing, &archive, format), &control).unwrap();
            let info = inspect_archive(&archive, None, &control).unwrap();
            let names = info
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>();
            assert_eq!(names.len(), 2);
            assert!(names.contains(&"Special_Cases_List.mhx6zip"));
            assert!(names.contains(&"Ignored_Items.mmzip"));
            assert_eq!(control.warnings().len(), 1);
            assert!(control.warnings()[0].contains("missing.txt"));
        }
    }
    let temp = TempDir::new().unwrap();
    let good = temp.path().join("good.txt");
    fs::write(&good, b"good").unwrap();
    let control = JobControl::default();
    let outputs =
        create_archives_separately(vec![temp.path().join("missing.txt"), good], &control).unwrap();
    assert_eq!(outputs.len(), 2);
    assert!(outputs.iter().all(|p| p.is_file()));
    assert_eq!(control.warnings().len(), 1);
    assert!(control.warnings()[0].contains("missing.txt"));
}

#[cfg(windows)]
#[test]
fn locked_source_files_warn_and_do_not_discard_readable_files() {
    use hx6zip::archive::SourceFilters;
    use std::{io::Read, os::windows::fs::OpenOptionsExt};

    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        for mode in [IntegrityMode::Normal, IntegrityMode::Safe] {
            let temp = TempDir::new().unwrap();
            let source = temp.path().join("source");
            fs::create_dir(&source).unwrap();
            fs::write(source.join("ok.txt"), b"readable").unwrap();
            let locked = source.join("locked.txt");
            fs::write(&locked, b"locked").unwrap();
            let _lock = fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(&locked)
                .unwrap();
            let archive = temp.path().join(format!("test.{}", format.extension()));
            let control = JobControl::default()
                .with_integrity(mode)
                .with_source_filters(SourceFilters {
                    ignore_locked_files: true,
                    ..SourceFilters::default()
                });
            create_archive(&options(&source, &archive, format), &control).unwrap();
            assert!(control.warnings().iter().any(|w| w.contains("locked.txt")));
            let info = inspect_archive(&archive, None, &control).unwrap();
            assert!(info.entries.iter().any(|e| e.name == "source/ok.txt"));
            assert!(!info.entries.iter().any(|e| e.name.contains("locked.txt")));
            assert!(info.entries.iter().any(|e| e.name == "Ignored_Items.mmzip"));
            assert!(
                !info
                    .entries
                    .iter()
                    .any(|e| e.name == "Special_Cases_List.mhx6zip")
            );
            if format == ArchiveFormat::Zip {
                let mut reader = zip::ZipArchive::new(fs::File::open(&archive).unwrap()).unwrap();
                let mut contents = String::new();
                reader
                    .by_name("Ignored_Items.mmzip")
                    .unwrap()
                    .read_to_string(&mut contents)
                    .unwrap();
                assert!(contents.starts_with("Hx6Zip 忽略项与错误记录\n"));
                assert!(contents.contains("类型：忽略文件\n"));
                assert!(contents.contains("名称：source/locked.txt\n"));
                assert!(!contents.starts_with('{'));
            }
        }
    }
}

#[cfg(windows)]
#[test]
#[ignore = "requires official 7-Zip on PATH"]
fn dangling_junction_manifests_are_readable_by_official_7zip() {
    use std::{os::windows::process::CommandExt, process::Command};

    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("ok.txt"), b"readable content").unwrap();
    let target = temp.path().join("target");
    fs::create_dir(&target).unwrap();
    let _link = DirectoryLink::new(&source.join("broken"), &target);
    fs::remove_dir(&target).unwrap();
    let entries = |path: &Path| {
        let info = inspect_archive(path, None, &JobControl::default()).unwrap();
        let mut entries = info
            .entries
            .iter()
            .map(|e| {
                assert!(!e.is_symbolic_link && !e.unsupported_kind);
                (
                    e.name.trim_end_matches('/').to_owned(),
                    e.is_directory,
                    e.size,
                )
            })
            .collect::<Vec<_>>();
        entries.sort();
        entries
    };
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        let official = temp.path().join(format!("official.{}", format.extension()));
        let output = Command::new("7z")
            .current_dir(temp.path())
            .arg("a")
            .arg(&official)
            .arg("source")
            .creation_flags(0x08000000)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        for mode in [IntegrityMode::Normal, IntegrityMode::Safe] {
            let archive = temp.path().join(format!("hx6zip.{}", format.extension()));
            let control = JobControl::default().with_integrity(mode);
            create_archive(&options(&source, &archive, format), &control).unwrap();
            assert!(control.warnings().is_empty(), "{:?}", control.warnings());
            let ours = entries(&archive);
            assert!(
                ours.iter()
                    .any(|(name, _, _)| name == "Special_Cases_List.mhx6zip")
            );
            let ordinary = ours
                .into_iter()
                .filter(|(name, _, _)| name != "Special_Cases_List.mhx6zip")
                .collect::<Vec<_>>();
            let official_entries = entries(&official)
                .into_iter()
                .filter(|(name, _, _)| name != "source/broken")
                .collect::<Vec<_>>();
            assert_eq!(ordinary, official_entries);
            let output = Command::new("7z")
                .arg("t")
                .arg(&archive)
                .creation_flags(0x08000000)
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            fs::remove_file(archive).unwrap();
        }
    }
}

#[test]
fn extraction_follows_existing_directory_links_in_both_modes() {
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        for mode in [IntegrityMode::Normal, IntegrityMode::Safe] {
            let temp = TempDir::new().unwrap();
            let source = temp.path().join("source");
            fs::create_dir_all(source.join("nested")).unwrap();
            fs::write(source.join("nested").join("file.txt"), b"new contents").unwrap();
            let archive = temp.path().join(format!("test.{}", format.extension()));
            let control = JobControl::default().with_integrity(mode);
            create_archive(&options(&source, &archive, format), &control).unwrap();
            let info = inspect_archive(&archive, None, &control).unwrap();
            let outside = temp.path().join("outside");
            fs::create_dir_all(outside.join("nested")).unwrap();
            fs::write(outside.join("nested").join("file.txt"), b"old contents").unwrap();
            fs::write(outside.join("unrelated.txt"), b"keep").unwrap();
            let output = temp.path().join("out");
            fs::create_dir(&output).unwrap();
            let link = output.join("source");
            let _link = DirectoryLink::new(&link, &outside);
            let mut options = ExtractOptions {
                destination: output.clone(),
                password: None,
                selected: None,
                overwrite: false,
            };
            assert_eq!(
                extract_conflicts(&info, &options, &control).unwrap().len(),
                1
            );
            assert!(extract_archive(&info, &options, &control).is_err());
            assert_eq!(
                fs::read(outside.join("nested").join("file.txt")).unwrap(),
                b"old contents"
            );
            options.overwrite = true;
            extract_archive(&info, &options, &control).unwrap();
            assert_eq!(
                fs::read(outside.join("nested").join("file.txt")).unwrap(),
                b"new contents"
            );
            assert_eq!(fs::read(outside.join("unrelated.txt")).unwrap(), b"keep");
            assert_eq!(
                fs::canonicalize(&link).unwrap(),
                fs::canonicalize(&outside).unwrap()
            );
        }
    }
}

#[test]
fn safe_extraction_rolls_back_real_link_targets_including_on_other_volumes() {
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        let temp = TempDir::new().unwrap();
        let target_temp = tempfile::Builder::new()
            .prefix(".hx6zip-link-test-")
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("first.txt"), b"new first").unwrap();
        fs::write(source.join("second.txt"), b"new second").unwrap();
        let archive = temp.path().join(format!("test.{}", format.extension()));
        create_archive(&options(&source, &archive, format), &JobControl::default()).unwrap();
        let info = inspect_archive(&archive, None, &JobControl::default()).unwrap();
        let outside = target_temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("first.txt"), b"old first").unwrap();
        fs::write(outside.join("second.txt"), b"old second").unwrap();
        let modified = fs::metadata(&outside).unwrap().modified().unwrap();
        let output = temp.path().join("out");
        fs::create_dir(&output).unwrap();
        let _link = DirectoryLink::new(&output.join("source"), &outside);
        let cancel_at = info
            .entries
            .iter()
            .filter(|e| !e.is_directory)
            .nth(1)
            .unwrap()
            .name
            .clone();
        let token = CancellationToken::default();
        let cancel = token.clone();
        let control = JobControl::new(token, move |progress| {
            if progress.operation == "正在提交解压结果" && progress.current_file == cancel_at
            {
                cancel.cancel();
            }
        })
        .with_integrity(IntegrityMode::Safe);
        let options = ExtractOptions {
            destination: output.clone(),
            password: None,
            selected: None,
            overwrite: true,
        };
        let error = extract_archive(&info, &options, &control).unwrap_err();
        assert!(error.downcast_ref::<Cancelled>().is_some(), "{error:#}");
        assert_eq!(fs::read(outside.join("first.txt")).unwrap(), b"old first");
        assert_eq!(fs::read(outside.join("second.txt")).unwrap(), b"old second");
        assert_eq!(
            fs::metadata(&outside).unwrap().modified().unwrap(),
            modified
        );
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 2);
        extract_archive(
            &info,
            &options,
            &JobControl::default().with_integrity(IntegrityMode::Safe),
        )
        .unwrap();
        assert_eq!(fs::read(outside.join("first.txt")).unwrap(), b"new first");
        assert_eq!(fs::read(outside.join("second.txt")).unwrap(), b"new second");
    }
}

#[test]
fn linked_source_excludes_destination_and_work_directory() {
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        let target = temp.path().join("target");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&target).unwrap();
        fs::write(target.join("file.txt"), b"contents").unwrap();
        let _link = DirectoryLink::new(&source.join("linked"), &target);
        let archive = target.join(format!("test.{}", format.extension()));
        fs::write(&archive, b"old archive").unwrap();
        let mut options = options(&source, &archive, format);
        options.overwrite = true;
        create_archive(
            &options,
            &JobControl::default().with_integrity(IntegrityMode::Safe),
        )
        .unwrap();
        let info = inspect_archive(&archive, None, &JobControl::default()).unwrap();
        assert_eq!(info.entries.len(), 3);
        assert!(
            info.entries
                .iter()
                .any(|entry| entry.name == "source/linked/file.txt")
        );
        assert!(
            info.entries
                .iter()
                .all(|entry| !entry.name.contains(".hx6zip-")
                    && !entry.name.ends_with(format.extension()))
        );
    }
}

#[test]
fn safe_readback_detects_changes_to_linked_contents() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        let target = temp.path().join("target");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&target).unwrap();
        let file = target.join("file.txt");
        fs::write(&file, b"before").unwrap();
        let modified = fs::metadata(&file).unwrap().modified().unwrap();
        let _link = DirectoryLink::new(&source.join("linked"), &target);
        let archive = temp.path().join(format!("test.{}", format.extension()));
        fs::write(&archive, b"old archive").unwrap();
        let changed = Arc::new(AtomicBool::new(false));
        let updated = changed.clone();
        let control = JobControl::new(CancellationToken::default(), move |progress| {
            if progress.operation == "正在校验压缩包" && !updated.swap(true, Ordering::SeqCst)
            {
                fs::write(&file, b"after!").unwrap();
                fs::File::options()
                    .write(true)
                    .open(&file)
                    .unwrap()
                    .set_modified(modified)
                    .unwrap();
            }
        })
        .with_integrity(IntegrityMode::Safe);
        let mut options = options(&source, &archive, format);
        options.overwrite = true;
        let error = create_archive(&options, &control).unwrap_err();
        assert!(changed.load(Ordering::SeqCst));
        assert!(format!("{error:#}").contains("不一致"), "{error:#}");
        assert_eq!(fs::read(&archive).unwrap(), b"old archive");
    }
}

#[test]
fn sevenzip_empty_reparse_directory_extracts_as_a_regular_directory() {
    for mode in [IntegrityMode::Normal, IntegrityMode::Safe] {
        let temp = TempDir::new().unwrap();
        let archive = temp.path().join("junction.7z");
        let mut writer = sevenz_rust2::ArchiveWriter::create(&archive).unwrap();
        let mut entry = sevenz_rust2::ArchiveEntry::new_directory("junction");
        entry.has_windows_attributes = true;
        entry.windows_attributes = 0x410;
        writer.push_archive_entry(entry, None::<&[u8]>).unwrap();
        writer.finish().unwrap();
        let control = JobControl::default().with_integrity(mode);
        let info = inspect_archive(&archive, None, &control).unwrap();
        let output = temp.path().join("out");
        extract_archive(
            &info,
            &ExtractOptions {
                destination: output.clone(),
                password: None,
                selected: None,
                overwrite: false,
            },
            &control,
        )
        .unwrap();
        let metadata = fs::symlink_metadata(output.join("junction")).unwrap();
        assert!(metadata.is_dir());
        assert!(!metadata.file_type().is_symlink());
    }
}
