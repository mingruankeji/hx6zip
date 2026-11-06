use std::{fs, path::Path};

use mzip::archive::{
    ArchiveEdit, ArchiveFormat, CancellationToken, CreateOptions, ExtractOptions, IntegrityMode,
    JobControl, archive_selection, create_archive, edit_archive, extract_archive,
    extract_drag_entries, inspect_archive, smart_destination,
};
use tempfile::TempDir;

const MANIFEST: &str = "Special_Cases_List.mmzip";

#[test]
fn diagnostic_manifests_are_kept_after_extraction() {
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        for mode in [IntegrityMode::Normal, IntegrityMode::Safe] {
            let temp = TempDir::new().unwrap();
            let archive = temp.path().join(format!("test.{}", format.extension()));
            let control = JobControl::default().with_integrity(mode);
            create_archive(
                &CreateOptions {
                    sources: vec![temp.path().join("missing.txt")],
                    destination: archive.clone(),
                    format,
                    password: None,
                    overwrite: false,
                },
                &control,
            )
            .unwrap();
            let info = inspect_archive(&archive, None, &control).unwrap();
            let output = temp.path().join("output");
            let control = JobControl::default().with_integrity(mode);
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
            assert!(output.join(MANIFEST).is_file());
            assert!(
                control
                    .warnings()
                    .iter()
                    .any(|warning| warning.contains("未备份"))
            );
        }
    }
}

#[test]
fn partially_processed_manifests_are_kept() {
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        for mode in [IntegrityMode::Normal, IntegrityMode::Safe] {
            let temp = TempDir::new().unwrap();
            let source = temp.path().join("source");
            let target = temp.path().join("target");
            fs::create_dir_all(source.join("first")).unwrap();
            fs::create_dir_all(source.join("second")).unwrap();
            fs::create_dir(&target).unwrap();
            directory_link(&source.join("first/link"), &target);
            directory_link(&source.join("second/link"), &target);
            fs::remove_dir(&target).unwrap();
            let archive = temp.path().join(format!("test.{}", format.extension()));
            create(&source, &archive, format, None, mode);
            let control = JobControl::default().with_integrity(mode);
            let info = inspect_archive(&archive, None, &control).unwrap();
            let selected =
                archive_selection(&info, &[MANIFEST.into(), "source/first".into()]).unwrap();
            let output = temp.path().join("output");
            extract_archive(
                &info,
                &ExtractOptions {
                    destination: output.clone(),
                    password: None,
                    selected: Some(selected),
                    overwrite: false,
                },
                &control,
            )
            .unwrap();
            assert!(fs::read_link(output.join("source/first/link")).is_ok());
            assert!(!output.join("source/second").exists());
            assert!(output.join(MANIFEST).is_file());
            assert!(control.warnings().is_empty());
        }
    }
}

#[test]
fn safe_readback_rejects_changed_special_link_targets() {
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&first).unwrap();
        fs::create_dir(&second).unwrap();
        let link = source.join("link");
        directory_link(&link, &first);
        fs::remove_dir(&first).unwrap();
        let archive = temp.path().join(format!("test.{}", format.extension()));
        fs::write(&archive, b"existing archive").unwrap();
        let changed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let triggered = changed.clone();
        let control = JobControl::new(CancellationToken::default(), move |progress| {
            if progress.operation == "正在校验压缩包"
                && !triggered.swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                #[cfg(windows)]
                fs::remove_dir(&link).unwrap();
                #[cfg(unix)]
                fs::remove_file(&link).unwrap();
                directory_link(&link, &second);
                fs::remove_dir(&second).unwrap();
            }
        })
        .with_integrity(IntegrityMode::Safe);
        let error = create_archive(
            &CreateOptions {
                sources: vec![source],
                destination: archive.clone(),
                format,
                password: None,
                overwrite: true,
            },
            &control,
        )
        .unwrap_err();
        assert!(changed.load(std::sync::atomic::Ordering::SeqCst));
        assert!(format!("{error:#}").contains("变化"), "{error:#}");
        assert_eq!(fs::read(archive).unwrap(), b"existing archive");
    }
}

#[test]
fn safe_extraction_never_restores_links_before_successful_commit() {
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        let target = temp.path().join("target");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&target).unwrap();
        directory_link(&source.join("link"), &target);
        fs::remove_dir(&target).unwrap();
        let archive = temp.path().join(format!("test.{}", format.extension()));
        create(&source, &archive, format, None, IntegrityMode::Safe);
        let info = inspect_archive(&archive, None, &JobControl::default()).unwrap();
        let output = temp.path().join("output");
        let token = CancellationToken::default();
        let cancel = token.clone();
        let control = JobControl::new(token, move |progress| {
            if progress.operation == "正在提交解压结果" {
                cancel.cancel();
            }
        })
        .with_integrity(IntegrityMode::Safe);
        assert!(
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
            .is_err()
        );
        assert!(fs::symlink_metadata(output.join("source/link")).is_err());
        assert!(!output.join(MANIFEST).exists());
    }
}

fn directory_link(path: &Path, target: &Path) {
    #[cfg(windows)]
    {
        use std::{os::windows::process::CommandExt, process::Command};
        let output = Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(path.components().collect::<std::path::PathBuf>())
            .arg(target.components().collect::<std::path::PathBuf>())
            .creation_flags(0x08000000)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, path).unwrap();
}

fn create(
    source: &Path,
    archive: &Path,
    format: ArchiveFormat,
    password: Option<&str>,
    mode: IntegrityMode,
) {
    let control = JobControl::default().with_integrity(mode);
    create_archive(
        &CreateOptions {
            sources: vec![source.to_owned()],
            destination: archive.to_owned(),
            format,
            password: password.map(str::to_owned),
            overwrite: false,
        },
        &control,
    )
    .unwrap();
    assert!(control.warnings().is_empty(), "{:?}", control.warnings());
}

#[test]
fn editing_merges_updates_and_removes_special_records() {
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        for mode in [IntegrityMode::Normal, IntegrityMode::Safe] {
            let temp = TempDir::new().unwrap();
            let seed = temp.path().join("seed.txt");
            fs::write(&seed, b"seed").unwrap();
            let archive = temp.path().join(format!("test.{}", format.extension()));
            let password = Some("secret");
            create(&seed, &archive, format, password, mode);
            let control = JobControl::default().with_integrity(mode);
            let mut info = inspect_archive(&archive, password, &control).unwrap();
            for name in ["first", "second"] {
                let source = temp.path().join(name);
                let target = temp.path().join(format!("{name}-target"));
                fs::create_dir(&source).unwrap();
                fs::create_dir(&target).unwrap();
                directory_link(&source.join("link"), &target);
                fs::remove_dir(&target).unwrap();
                info = edit_archive(
                    &info,
                    &ArchiveEdit::Add {
                        sources: vec![source],
                        directory: String::new(),
                        overwrite: false,
                    },
                    password,
                    &control,
                )
                .unwrap();
            }
            let output = temp.path().join("out");
            extract_archive(
                &info,
                &ExtractOptions {
                    destination: output.clone(),
                    password: password.map(str::to_owned),
                    selected: None,
                    overwrite: false,
                },
                &control,
            )
            .unwrap();
            assert!(fs::read_link(output.join("first/link")).is_ok());
            assert!(fs::read_link(output.join("second/link")).is_ok());
            info = edit_archive(
                &info,
                &ArchiveEdit::Delete {
                    paths: vec!["first".into()],
                },
                password,
                &control,
            )
            .unwrap();
            let output = temp.path().join("after-delete");
            extract_archive(
                &info,
                &ExtractOptions {
                    destination: output.clone(),
                    password: password.map(str::to_owned),
                    selected: None,
                    overwrite: false,
                },
                &control,
            )
            .unwrap();
            assert!(!output.join("first").exists());
            assert!(fs::read_link(output.join("second/link")).is_ok());
            #[cfg(windows)]
            fs::remove_dir(temp.path().join("second/link")).unwrap();
            #[cfg(unix)]
            fs::remove_file(temp.path().join("second/link")).unwrap();
            fs::write(temp.path().join("second/link"), b"ordinary replacement").unwrap();
            info = edit_archive(
                &info,
                &ArchiveEdit::Add {
                    sources: vec![temp.path().join("second")],
                    directory: String::new(),
                    overwrite: true,
                },
                password,
                &control,
            )
            .unwrap();
            assert!(!info.entries.iter().any(|entry| entry.name == MANIFEST));
        }
    }
}

#[test]
fn dangling_links_round_trip_with_passwords_selection_and_drag() {
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        for mode in [IntegrityMode::Normal, IntegrityMode::Safe] {
            for password in [None, Some("secret")] {
                let temp = TempDir::new().unwrap();
                let source = temp.path().join("source");
                fs::create_dir_all(source.join("nested")).unwrap();
                fs::write(source.join("ok.txt"), b"ordinary contents").unwrap();
                fs::write(source.join("nested/file.txt"), b"nested contents").unwrap();
                let target = temp.path().join("missing-target");
                fs::create_dir(&target).unwrap();
                directory_link(&source.join("nested/link"), &target);
                let original_target = fs::read_link(source.join("nested/link")).unwrap();
                fs::remove_dir(&target).unwrap();
                let archive = temp.path().join(format!("test.{}", format.extension()));
                create(&source, &archive, format, password, mode);
                let control = JobControl::default().with_integrity(mode);
                let info = inspect_archive(&archive, password, &control).unwrap();
                let original_archive = fs::read(&archive).unwrap();
                assert!(info.entries.iter().any(|entry| entry.name == MANIFEST));
                assert!(
                    !info
                        .entries
                        .iter()
                        .any(|entry| entry.name.starts_with("source/nested/link"))
                );
                assert_eq!(smart_destination(&info, temp.path()).unwrap(), temp.path());
                for selected in [
                    None,
                    Some(archive_selection(&info, &["source/nested".into()]).unwrap()),
                ] {
                    let output = TempDir::new().unwrap();
                    extract_archive(
                        &info,
                        &ExtractOptions {
                            destination: output.path().to_owned(),
                            password: password.map(str::to_owned),
                            selected: selected.clone(),
                            overwrite: false,
                        },
                        &control,
                    )
                    .unwrap();
                    assert_eq!(
                        fs::read_link(output.path().join("source/nested/link")).unwrap(),
                        original_target
                    );
                    assert_eq!(
                        fs::read(output.path().join("source/nested/file.txt")).unwrap(),
                        b"nested contents"
                    );
                    assert_eq!(
                        output.path().join("source/ok.txt").exists(),
                        selected.is_none()
                    );
                    assert!(!output.path().join(MANIFEST).exists());
                    assert_eq!(fs::read(&archive).unwrap(), original_archive);
                }
                let output = TempDir::new().unwrap();
                extract_drag_entries(
                    &info,
                    &["source/nested".into()],
                    output.path(),
                    password,
                    false,
                    &control,
                )
                .unwrap();
                assert_eq!(
                    fs::read_link(output.path().join("nested/link")).unwrap(),
                    original_target
                );
                assert!(control.warnings().is_empty(), "{:?}", control.warnings());
            }
        }
    }
}

#[test]
fn restoration_does_not_replace_existing_items_or_follow_parent_links() {
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        for mode in [IntegrityMode::Normal, IntegrityMode::Safe] {
            let temp = TempDir::new().unwrap();
            let source = temp.path().join("source");
            let missing = temp.path().join("missing");
            fs::create_dir_all(source.join("nested")).unwrap();
            fs::create_dir(&missing).unwrap();
            directory_link(&source.join("nested/link"), &missing);
            fs::remove_dir(&missing).unwrap();
            let archive = temp.path().join(format!("test.{}", format.extension()));
            create(&source, &archive, format, None, mode);
            let info = inspect_archive(&archive, None, &JobControl::default()).unwrap();
            for linked_parent in [false, true] {
                let output = TempDir::new().unwrap();
                let outside = TempDir::new().unwrap();
                if linked_parent {
                    fs::create_dir(output.path().join("source")).unwrap();
                    directory_link(&output.path().join("source/nested"), outside.path());
                } else {
                    fs::create_dir_all(output.path().join("source/nested/link")).unwrap();
                    fs::write(output.path().join("source/nested/link/keep.txt"), b"keep").unwrap();
                }
                let control = JobControl::default().with_integrity(mode);
                extract_archive(
                    &info,
                    &ExtractOptions {
                        destination: output.path().to_owned(),
                        password: None,
                        selected: None,
                        overwrite: true,
                    },
                    &control,
                )
                .unwrap();
                assert!(
                    control
                        .warnings()
                        .iter()
                        .any(|warning| warning.contains("未恢复"))
                );
                assert!(output.path().join(MANIFEST).is_file());
                if linked_parent {
                    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
                } else {
                    assert_eq!(
                        fs::read(output.path().join("source/nested/link/keep.txt")).unwrap(),
                        b"keep"
                    );
                }
            }
        }
    }
}

#[test]
fn malformed_manifest_warns_without_undoing_ordinary_extraction() {
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        for mode in [IntegrityMode::Normal, IntegrityMode::Safe] {
            let temp = TempDir::new().unwrap();
            let manifest = temp.path().join(MANIFEST);
            fs::write(&manifest, b"{\"not\":\"an mzip manifest\"}").unwrap();
            let archive = temp.path().join(format!("test.{}", format.extension()));
            create(&manifest, &archive, format, None, mode);
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
            assert_eq!(
                fs::read(output.join(MANIFEST)).unwrap(),
                fs::read(&manifest).unwrap()
            );
            assert!(
                control
                    .warnings()
                    .iter()
                    .any(|warning| warning.contains("清单"))
            );
        }
    }
}

#[test]
fn manifest_paths_cannot_escape_or_redirect_later_restoration() {
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source");
        let missing = temp.path().join("missing");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&missing).unwrap();
        directory_link(&source.join("link"), &missing);
        fs::remove_dir(&missing).unwrap();
        let original = temp.path().join(format!("original.{}", format.extension()));
        create(&source, &original, format, None, IntegrityMode::Normal);
        let info = inspect_archive(&original, None, &JobControl::default()).unwrap();
        let extracted = temp.path().join("extracted");
        extract_archive(
            &info,
            &ExtractOptions {
                destination: extracted.clone(),
                password: None,
                selected: Some(archive_selection(&info, &[MANIFEST.into()]).unwrap()),
                overwrite: false,
            },
            &JobControl::default(),
        )
        .unwrap();
        let template: serde_json::Value =
            serde_json::from_slice(&fs::read(extracted.join(MANIFEST)).unwrap()).unwrap();
        for path in [
            "../escape",
            "C:/escape",
            "redirect/child",
            "duplicate",
            "version",
        ] {
            let mut manifest = template.clone();
            manifest["entries"][0]["path"] = path.into();
            if path == "version" {
                manifest["version"] = 999.into();
            }
            if path == "duplicate" {
                let entry = manifest["entries"][0].clone();
                manifest["entries"].as_array_mut().unwrap().push(entry);
            }
            let outside = TempDir::new().unwrap();
            if path == "redirect/child" {
                let mut parent = manifest["entries"][0].clone();
                parent["path"] = "redirect".into();
                parent["target"] = outside.path().to_str().unwrap().into();
                manifest["entries"]
                    .as_array_mut()
                    .unwrap()
                    .insert(0, parent);
            }
            let file = extracted.join(MANIFEST);
            fs::write(&file, serde_json::to_vec(&manifest).unwrap()).unwrap();
            let archive = temp.path().join(format!("forged.{}", format.extension()));
            if archive.exists() {
                fs::remove_file(&archive).unwrap();
            }
            create(&file, &archive, format, None, IntegrityMode::Normal);
            let info = inspect_archive(&archive, None, &JobControl::default()).unwrap();
            let output = TempDir::new().unwrap();
            let control = JobControl::default();
            extract_archive(
                &info,
                &ExtractOptions {
                    destination: output.path().to_owned(),
                    password: None,
                    selected: None,
                    overwrite: false,
                },
                &control,
            )
            .unwrap();
            assert!(!control.warnings().is_empty());
            assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
            assert!(fs::symlink_metadata(output.path().join("escape")).is_err());
        }
    }
}

#[test]
fn generated_manifest_never_overwrites_a_user_source() {
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        let temp = TempDir::new().unwrap();
        let reserved = temp.path().join(MANIFEST);
        fs::write(&reserved, b"user document").unwrap();
        let source = temp.path().join("source");
        let target = temp.path().join("target");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&target).unwrap();
        directory_link(&source.join("link"), &target);
        fs::remove_dir(&target).unwrap();
        let archive = temp.path().join(format!("test.{}", format.extension()));
        fs::write(&archive, b"old archive").unwrap();
        let error = create_archive(
            &CreateOptions {
                sources: vec![source, reserved.clone()],
                destination: archive.clone(),
                format,
                password: None,
                overwrite: true,
            },
            &JobControl::default(),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains(MANIFEST));
        assert_eq!(fs::read(reserved).unwrap(), b"user document");
        assert_eq!(fs::read(archive).unwrap(), b"old archive");
    }
}
