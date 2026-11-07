use std::{
    collections::BTreeSet,
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use hx6zip::archive::{
    ArchiveEdit, ArchiveFormat, Cancelled, CreateOptions, ExtractOptions, IntegrityMode,
    JobControl, PasswordPrompt, ZipDragSource, create_archive, edit_archive, export_entries,
    extract_archive, extract_drag_entries, inspect_archive,
};

fn sample(root: &Path, format: ArchiveFormat, encrypted: bool) -> PathBuf {
    let source = root.join("tree");
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("hello.txt"), b"hello").unwrap();
    let path = root.join(format!("sample.{}", format.extension()));
    create_archive(
        &CreateOptions {
            sources: vec![source],
            destination: path.clone(),
            format,
            password: encrypted.then(|| "secret".to_owned()),
            overwrite: false,
        },
        &JobControl::default(),
    )
    .unwrap();
    path
}

fn rar(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("libarchive")
        .join(name)
}

fn extract(path: &Path, output: &Path, control: &JobControl) -> anyhow::Result<()> {
    let info = inspect_archive(path, None, control)?;
    extract_archive(
        &info,
        &ExtractOptions {
            destination: output.to_owned(),
            password: None,
            selected: None,
            overwrite: false,
        },
        control,
    )
}

fn prompt(password: &'static str) -> (PasswordPrompt, Arc<AtomicUsize>) {
    let count = Arc::new(AtomicUsize::new(0));
    let calls = count.clone();
    (
        PasswordPrompt::new(move |_, _, _| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(password.to_owned())
        }),
        count,
    )
}

#[test]
fn zip_and_sevenzip_prompt_once_and_reuse_password_for_drag_export_and_edit() {
    for mode in [IntegrityMode::Safe, IntegrityMode::Normal] {
        for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
            let temp = tempfile::tempdir().unwrap();
            let path = sample(temp.path(), format, true);
            let (prompt, calls) = prompt("secret");
            let control = JobControl::default()
                .with_integrity(mode)
                .with_password_prompt(prompt);
            let output = temp.path().join("output");
            extract(&path, &output, &control).unwrap();
            assert_eq!(
                fs::read(output.join("tree").join("hello.txt")).unwrap(),
                b"hello"
            );
            let info = inspect_archive(&path, None, &control).unwrap();
            assert!(
                info.entries
                    .iter()
                    .any(|entry| entry.is_encrypted == Some(true))
            );
            let drag = temp.path().join("drag");
            extract_drag_entries(&info, &["tree".into()], &drag, None, false, &control).unwrap();
            assert_eq!(
                fs::read(drag.join("tree").join("hello.txt")).unwrap(),
                b"hello"
            );
            let exported = export_entries(&info, &["tree".into()], None, &control).unwrap();
            assert_eq!(
                fs::read(exported.paths[0].join("hello.txt")).unwrap(),
                b"hello"
            );
            if format == ArchiveFormat::Zip {
                let source = ZipDragSource::new(&info, &["tree".into()], None, &control).unwrap();
                let index = info
                    .entries
                    .iter()
                    .find(|entry| !entry.is_directory)
                    .unwrap()
                    .index;
                let mut bytes = Vec::new();
                source.open(index).unwrap().read_to_end(&mut bytes).unwrap();
                assert_eq!(bytes, b"hello");
            }
            let incoming = temp.path().join("added.txt");
            fs::write(&incoming, b"added").unwrap();
            let updated = edit_archive(
                &info,
                &ArchiveEdit::Add {
                    sources: vec![incoming],
                    directory: String::new(),
                    overwrite: false,
                },
                None,
                &control,
            )
            .unwrap();
            assert!(
                updated
                    .entries
                    .iter()
                    .any(|entry| entry.name == "added.txt")
            );
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }
    }
}

#[test]
fn rar_header_and_data_encryption_prompt_and_support_direct_drag() {
    for mode in [IntegrityMode::Safe, IntegrityMode::Normal] {
        for (name, password) in [
            ("test_read_format_rar5_encrypted_filenames.rar", "password"),
            ("test_read_format_rar5_solid_encrypted.rar", "password"),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let (prompt, calls) = prompt(password);
            let control = JobControl::default()
                .with_integrity(mode)
                .with_password_prompt(prompt);
            let path = rar(name);
            extract(&path, &temp.path().join("output"), &control).unwrap();
            let info = inspect_archive(&path, None, &control).unwrap();
            let output = temp.path().join("drag");
            extract_drag_entries(&info, &["c.txt".into()], &output, None, false, &control).unwrap();
            assert_eq!(
                fs::read(output.join("c.txt")).unwrap(),
                b"This is from c.txt"
            );
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }
    }
}

#[test]
fn safe_extraction_reprompts_after_wrong_password_without_replacing_existing_data() {
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        let temp = tempfile::tempdir().unwrap();
        let path = sample(temp.path(), format, true);
        let output = temp.path().join("output");
        fs::create_dir_all(output.join("tree")).unwrap();
        fs::write(output.join("tree").join("hello.txt"), b"old").unwrap();
        let original = output.join("tree").join("hello.txt");
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let prompt = PasswordPrompt::new(move |_, retry, _| {
            assert_eq!(fs::read(&original).unwrap(), b"old");
            let call = count.fetch_add(1, Ordering::SeqCst);
            assert_eq!(retry, call > 0);
            Ok(if call == 0 { "wrong" } else { "secret" }.to_owned())
        });
        let control = JobControl::default().with_password_prompt(prompt);
        let info = inspect_archive(&path, None, &control).unwrap();
        extract_archive(
            &info,
            &ExtractOptions {
                destination: output.clone(),
                password: None,
                selected: None,
                overwrite: true,
            },
            &control,
        )
        .unwrap();
        assert_eq!(
            fs::read(output.join("tree").join("hello.txt")).unwrap(),
            b"hello"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}

#[test]
fn data_encrypted_sevenzip_and_rar_retry_wrong_passwords_before_publishing() {
    let temp = tempfile::tempdir().unwrap();
    let sevenzip = temp.path().join("data.7z");
    let mut writer = sevenz_rust2::ArchiveWriter::create(&sevenzip).unwrap();
    writer.set_encrypt_header(false);
    writer.set_content_methods(vec![
        sevenz_rust2::EncoderMethod::LZMA2.into(),
        sevenz_rust2::encoder_options::AesEncoderOptions::new(sevenz_rust2::Password::from(
            "secret",
        ))
        .into(),
    ]);
    writer
        .push_archive_entry(
            sevenz_rust2::ArchiveEntry::new_file("hello.txt"),
            Some(std::io::Cursor::new(b"hello")),
        )
        .unwrap();
    writer.finish().unwrap();
    for (path, password) in [
        (sevenzip, "secret"),
        (
            rar("test_read_format_rar5_encrypted_filenames.rar"),
            "password",
        ),
        (rar("test_read_format_rar5_solid_encrypted.rar"), "password"),
    ] {
        let output = tempfile::tempdir().unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let calls = count.clone();
        let control =
            JobControl::default().with_password_prompt(PasswordPrompt::new(move |_, retry, _| {
                let call = calls.fetch_add(1, Ordering::SeqCst);
                assert!(call < 2, "wrong passwords must not cause an endless retry");
                assert_eq!(retry, call > 0);
                Ok(if call == 0 { "wrong" } else { password }.to_owned())
            }));
        extract(&path, &output.path().join("result"), &control).unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }
}

#[test]
fn cancellation_never_writes_or_changes_the_source_and_plain_archives_do_not_prompt() {
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        for encrypted in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let path = sample(temp.path(), format, encrypted);
            let source = fs::read(&path).unwrap();
            let output = temp.path().join("output");
            let prompt = PasswordPrompt::new(|_, _, _| Err(Cancelled.into()));
            let control = JobControl::default().with_password_prompt(prompt);
            let result = extract(&path, &output, &control);
            if encrypted {
                assert!(result.unwrap_err().is::<Cancelled>());
                assert!(!output.exists());
            } else {
                result.unwrap();
            }
            assert_eq!(fs::read(&path).unwrap(), source);
        }
    }
}

#[test]
fn selecting_plain_zip_content_does_not_prompt_for_unselected_encrypted_entries() {
    use std::io::Write;
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("mixed.zip");
    let mut zip = zip::ZipWriter::new(fs::File::create(&path).unwrap());
    zip.start_file("plain.txt", zip::write::SimpleFileOptions::default())
        .unwrap();
    zip.write_all(b"plain").unwrap();
    zip.start_file(
        "encrypted.txt",
        zip::write::SimpleFileOptions::default()
            .with_aes_encryption(zip::AesMode::Aes256, "secret"),
    )
    .unwrap();
    zip.write_all(b"encrypted").unwrap();
    zip.finish().unwrap();
    let control = JobControl::default().with_password_prompt(PasswordPrompt::new(|_, _, _| {
        panic!("unselected encrypted entries must not prompt")
    }));
    let info = inspect_archive(&path, None, &control).unwrap();
    extract_archive(
        &info,
        &ExtractOptions {
            destination: temp.path().join("output"),
            password: None,
            selected: Some(BTreeSet::from([0])),
            overwrite: false,
        },
        &control,
    )
    .unwrap();
}

#[test]
#[ignore = "reads encrypted user samples from HX6ZIP_PASSWORD_PROBE_DIR without extracting their contents"]
fn supplied_archives_request_password_before_writing() {
    let root =
        PathBuf::from(std::env::var_os("HX6ZIP_PASSWORD_PROBE_DIR").expect("set sample directory"));
    for extension in ["rar", "zip", "7z"] {
        let path = root.join(format!("{extension}密码.{extension}"));
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("output");
        let count = Arc::new(AtomicUsize::new(0));
        let calls = count.clone();
        let control =
            JobControl::default().with_password_prompt(PasswordPrompt::new(move |_, _, _| {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(Cancelled.into())
            }));
        let source = fs::read(&path).unwrap();
        let error = extract(&path, &output, &control).unwrap_err();
        assert!(error.is::<Cancelled>(), "{extension}: {error:#}");
        assert_eq!(count.load(Ordering::SeqCst), 1, "{extension}");
        assert!(!output.exists(), "{extension}");
        assert_eq!(fs::read(path).unwrap(), source);
    }
}
