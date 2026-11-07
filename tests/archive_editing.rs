use std::{fs, io::Write, path::Path};

use hx6zip::archive::{
    ArchiveEdit, ArchiveFormat, ArchiveInfo, CancellationToken, CreateOptions, IntegrityMode,
    JobControl, archive_children, archive_selection, create_archive, edit_archive, export_entries,
    import_conflicts, inspect_archive,
};
use tempfile::TempDir;

fn write(path: &Path, data: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, data).unwrap();
}

fn fixture(root: &Path, format: ArchiveFormat, password: Option<&str>) -> ArchiveInfo {
    let source = root.join("tree");
    write(&source.join("nested").join("old.txt"), b"original");
    write(&source.join("keep.txt"), b"unchanged");
    fs::create_dir(source.join("empty")).unwrap();
    let path = root.join(format!("test.{}", format.extension()));
    create_archive(
        &CreateOptions {
            sources: vec![source],
            destination: path.clone(),
            format,
            password: password.map(str::to_owned),
            overwrite: false,
        },
        &JobControl::default(),
    )
    .unwrap();
    inspect_archive(&path, password, &JobControl::default()).unwrap()
}

#[test]
fn zip_and_sevenzip_edit_current_folder_replace_delete_and_export() {
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        for mode in [IntegrityMode::Normal, IntegrityMode::Safe] {
            let temp = TempDir::new().unwrap();
            let control = JobControl::default().with_integrity(mode);
            let mut info = fixture(temp.path(), format, None);
            let old_bytes = fs::read(&info.path).unwrap();
            let source = temp.path().join("incoming").join("old.txt");
            write(&source, b"replacement");
            assert_eq!(
                import_conflicts(&info, &[source.clone()], "tree/nested", &control)
                    .unwrap()
                    .len(),
                1
            );
            let mut edit = ArchiveEdit::Add {
                sources: vec![source],
                directory: "tree/nested".into(),
                overwrite: false,
            };
            assert!(edit_archive(&info, &edit, None, &control).is_err());
            assert_eq!(fs::read(&info.path).unwrap(), old_bytes);
            if let ArchiveEdit::Add { overwrite, .. } = &mut edit {
                *overwrite = true;
            }
            info = edit_archive(&info, &edit, None, &control).unwrap();
            let export = export_entries(
                &info,
                &[
                    "tree/nested".into(),
                    "tree/keep.txt".into(),
                    "tree/empty".into(),
                ],
                None,
                &control,
            )
            .unwrap();
            assert_eq!(export.paths.len(), 3);
            assert_eq!(
                fs::read(export.paths[0].join("old.txt")).unwrap(),
                b"replacement"
            );
            assert_eq!(fs::read(&export.paths[1]).unwrap(), b"unchanged");
            assert!(export.paths[2].is_dir());
            info = edit_archive(
                &info,
                &ArchiveEdit::Delete {
                    paths: vec!["tree/nested".into()],
                },
                None,
                &control,
            )
            .unwrap();
            assert!(
                !info
                    .entries
                    .iter()
                    .any(|e| e.name.starts_with("tree/nested"))
            );
            assert!(info.entries.iter().any(|e| e.name == "tree/keep.txt"));
            info = edit_archive(
                &info,
                &ArchiveEdit::Delete {
                    paths: vec!["tree".into()],
                },
                None,
                &control,
            )
            .unwrap();
            assert!(info.entries.is_empty());
            let source = temp.path().join("new.txt");
            write(&source, b"new");
            info = edit_archive(
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
            assert_eq!(archive_children(&info, "").unwrap()[0].name, "new.txt");
            assert!(!fs::read_dir(temp.path()).unwrap().any(|e| {
                e.unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".hx6zip-")
            }));
        }
    }
}

#[test]
fn encrypted_archives_remain_encrypted_after_updates() {
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        let temp = TempDir::new().unwrap();
        let info = fixture(temp.path(), format, Some("secret"));
        let original = fs::read(&info.path).unwrap();
        let edit = ArchiveEdit::Delete {
            paths: vec!["tree/nested".into()],
        };
        assert!(edit_archive(&info, &edit, Some("wrong"), &JobControl::default()).is_err());
        assert_eq!(fs::read(&info.path).unwrap(), original);
        assert!(edit_archive(&info, &edit, None, &JobControl::default()).is_err());
        assert_eq!(fs::read(&info.path).unwrap(), original);
        let info = edit_archive(&info, &edit, Some("secret"), &JobControl::default()).unwrap();
        assert!(
            export_entries(
                &info,
                &["tree/keep.txt".into()],
                None,
                &JobControl::default()
            )
            .is_err()
        );
        let exported = export_entries(
            &info,
            &["tree/keep.txt".into()],
            Some("secret"),
            &JobControl::default(),
        )
        .unwrap();
        assert_eq!(fs::read(&exported.paths[0]).unwrap(), b"unchanged");
    }
}

#[test]
fn implicit_directories_and_unicode_are_browsed_and_deleted_as_a_tree() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("implicit.zip");
    let mut zip = zip::ZipWriter::new(fs::File::create(&path).unwrap());
    zip.start_file(
        "资料/nested/文件.txt",
        zip::write::SimpleFileOptions::default(),
    )
    .unwrap();
    zip.write_all(b"hello").unwrap();
    zip.start_file("资料/sibling.txt", zip::write::SimpleFileOptions::default())
        .unwrap();
    zip.write_all(b"sibling").unwrap();
    zip.finish().unwrap();
    let info = inspect_archive(&path, None, &JobControl::default()).unwrap();
    let root = archive_children(&info, "").unwrap();
    assert_eq!(root.len(), 1);
    assert!(root[0].is_directory);
    assert_eq!(root[0].name, "资料");
    assert_eq!(archive_children(&info, "资料").unwrap().len(), 2);
    assert_eq!(
        archive_selection(&info, &["资料/nested".into()])
            .unwrap()
            .len(),
        1
    );
    let info = edit_archive(
        &info,
        &ArchiveEdit::Delete {
            paths: vec!["资料/nested".into()],
        },
        None,
        &JobControl::default(),
    )
    .unwrap();
    assert!(!info.entries.iter().any(|e| e.name.contains("nested")));
    assert!(info.entries.iter().any(|e| e.name == "资料/sibling.txt"));
}

#[test]
fn replacement_handles_file_directory_conflicts_and_merges_folders() {
    let temp = TempDir::new().unwrap();
    let mut info = fixture(temp.path(), ArchiveFormat::Zip, None);
    let source = temp.path().join("incoming").join("keep.txt");
    write(&source.join("child.txt"), b"child");
    assert_eq!(
        import_conflicts(&info, &[source.clone()], "tree", &JobControl::default())
            .unwrap()
            .len(),
        1
    );
    info = edit_archive(
        &info,
        &ArchiveEdit::Add {
            sources: vec![source],
            directory: "tree".into(),
            overwrite: true,
        },
        None,
        &JobControl::default(),
    )
    .unwrap();
    let export = export_entries(
        &info,
        &["tree/keep.txt".into()],
        None,
        &JobControl::default(),
    )
    .unwrap();
    assert_eq!(
        fs::read(export.paths[0].join("child.txt")).unwrap(),
        b"child"
    );
    let nested = temp.path().join("incoming").join("nested");
    write(&nested, b"now a file");
    info = edit_archive(
        &info,
        &ArchiveEdit::Add {
            sources: vec![nested],
            directory: "tree".into(),
            overwrite: true,
        },
        None,
        &JobControl::default(),
    )
    .unwrap();
    assert!(!info.entries.iter().any(|e| e.name == "tree/nested/old.txt"));
    let export =
        export_entries(&info, &["tree/nested".into()], None, &JobControl::default()).unwrap();
    assert_eq!(fs::read(&export.paths[0]).unwrap(), b"now a file");
}

#[test]
fn errors_cancellation_and_changed_archive_never_replace_original() {
    let temp = TempDir::new().unwrap();
    let info = fixture(temp.path(), ArchiveFormat::Zip, None);
    let original = fs::read(&info.path).unwrap();
    let cancelled = CancellationToken::default();
    cancelled.cancel();
    let edit = ArchiveEdit::Delete {
        paths: vec!["tree".into()],
    };
    assert!(edit_archive(&info, &edit, None, &JobControl::new(cancelled, |_| {})).is_err());
    assert_eq!(fs::read(&info.path).unwrap(), original);
    let token = CancellationToken::default();
    let cancel = token.clone();
    let control = JobControl::new(token, move |_| cancel.cancel());
    assert!(edit_archive(&info, &edit, None, &control).is_err());
    assert_eq!(fs::read(&info.path).unwrap(), original);
    for edit in [
        ArchiveEdit::Add {
            sources: vec![info.path.clone()],
            directory: String::new(),
            overwrite: true,
        },
        ArchiveEdit::Delete {
            paths: vec!["../outside".into()],
        },
        ArchiveEdit::Delete {
            paths: vec!["nonexistent".into()],
        },
    ] {
        assert!(edit_archive(&info, &edit, None, &JobControl::default()).is_err());
        assert_eq!(fs::read(&info.path).unwrap(), original);
    }
    fs::OpenOptions::new()
        .append(true)
        .open(&info.path)
        .unwrap()
        .write_all(b"changed")
        .unwrap();
    let changed = fs::read(&info.path).unwrap();
    assert!(edit_archive(&info, &edit, None, &JobControl::default()).is_err());
    assert_eq!(fs::read(&info.path).unwrap(), changed);
}

#[test]
fn adding_only_missing_sources_warns_and_preserves_original_bytes() {
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        let temp = TempDir::new().unwrap();
        let info = fixture(temp.path(), format, None);
        let original = fs::read(&info.path).unwrap();
        let control = JobControl::default();
        let updated = edit_archive(
            &info,
            &ArchiveEdit::Add {
                sources: vec![temp.path().join("missing")],
                directory: String::new(),
                overwrite: false,
            },
            None,
            &control,
        )
        .unwrap();
        assert_eq!(updated.entries, info.entries);
        assert_eq!(fs::read(&info.path).unwrap(), original);
        assert_eq!(control.warnings().len(), 1);
        assert!(control.warnings()[0].contains("missing"));
    }
}

#[test]
fn read_only_formats_reject_changes_but_allow_export() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("test.tar");
    let mut tar = tar::Builder::new(fs::File::create(&path).unwrap());
    let mut header = tar::Header::new_gnu();
    header.set_size(5);
    header.set_mode(0o644);
    header.set_cksum();
    tar.append_data(&mut header, "hello.txt", &b"hello"[..])
        .unwrap();
    tar.finish().unwrap();
    drop(tar);
    let info = inspect_archive(&path, None, &JobControl::default()).unwrap();
    assert!(
        edit_archive(
            &info,
            &ArchiveEdit::Delete {
                paths: vec!["hello.txt".into()]
            },
            None,
            &JobControl::default()
        )
        .is_err()
    );
    let exported =
        export_entries(&info, &["hello.txt".into()], None, &JobControl::default()).unwrap();
    assert_eq!(fs::read(&exported.paths[0]).unwrap(), b"hello");
}

#[test]
fn directory_case_aliases_share_one_browser_folder_and_selection() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("aliases.zip");
    let mut zip = zip::ZipWriter::new(fs::File::create(&path).unwrap());
    zip.add_directory("Folder/", zip::write::SimpleFileOptions::default())
        .unwrap();
    zip.start_file("folder/a.txt", zip::write::SimpleFileOptions::default())
        .unwrap();
    zip.write_all(b"a").unwrap();
    zip.start_file("FOLDER/b.txt", zip::write::SimpleFileOptions::default())
        .unwrap();
    zip.write_all(b"b").unwrap();
    zip.finish().unwrap();
    let info = inspect_archive(&path, None, &JobControl::default()).unwrap();
    assert_eq!(archive_children(&info, "").unwrap().len(), 1);
    assert_eq!(archive_children(&info, "Folder").unwrap().len(), 2);
    assert_eq!(
        archive_selection(&info, &["Folder".into()]).unwrap().len(),
        3
    );
}

#[test]
fn cancellation_while_publishing_an_update_preserves_original_bytes() {
    let temp = TempDir::new().unwrap();
    let info = fixture(temp.path(), ArchiveFormat::Zip, None);
    let original = fs::read(&info.path).unwrap();
    let token = CancellationToken::default();
    let cancel = token.clone();
    let control = JobControl::new(token, move |progress| {
        if progress.operation == "正在保存更新的压缩包" {
            cancel.cancel();
        }
    });
    assert!(
        edit_archive(
            &info,
            &ArchiveEdit::Delete {
                paths: vec!["tree/nested".into()]
            },
            None,
            &control
        )
        .is_err()
    );
    assert_eq!(fs::read(&info.path).unwrap(), original);
}

#[test]
fn adding_a_folder_merges_new_children_without_removing_existing_children() {
    let temp = TempDir::new().unwrap();
    let info = fixture(temp.path(), ArchiveFormat::SevenZip, None);
    let folder = temp.path().join("incoming").join("nested");
    write(&folder.join("new.txt"), b"new");
    fs::create_dir(folder.join("empty")).unwrap();
    assert!(
        import_conflicts(&info, &[folder.clone()], "tree", &JobControl::default())
            .unwrap()
            .is_empty()
    );
    let info = edit_archive(
        &info,
        &ArchiveEdit::Add {
            sources: vec![folder],
            directory: "tree".into(),
            overwrite: false,
        },
        Some("unused-password"),
        &JobControl::default(),
    )
    .unwrap();
    let export =
        export_entries(&info, &["tree/nested".into()], None, &JobControl::default()).unwrap();
    assert_eq!(
        fs::read(export.paths[0].join("old.txt")).unwrap(),
        b"original"
    );
    assert_eq!(fs::read(export.paths[0].join("new.txt")).unwrap(), b"new");
    assert!(export.paths[0].join("empty").is_dir());
}
