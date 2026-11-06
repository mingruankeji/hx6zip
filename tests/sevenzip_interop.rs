use std::{
    fs,
    path::Path,
    process::Command,
    time::{Duration, SystemTime},
};

use mzip::archive::{
    ArchiveEdit, ArchiveFormat, CreateOptions, ExtractOptions, JobControl, create_archive,
    edit_archive, extract_archive, inspect_archive,
};
use sevenz_rust2::{ArchiveReader, EncoderMethod, Password};
use tempfile::TempDir;

mod support;

const PASSWORD: &str = "MZip-interop-test";

#[test]
#[ignore = "requires a 7-Zip-compatible 7z command on PATH"]
fn incremental_updates_of_official_archives_remain_readable_by_7zip() {
    let cases: &[(&str, &[&str], Option<&str>)] = &[
        ("zip", &[], None),
        ("zip", &["-mem=ZipCrypto", "-psecret"], Some("secret")),
        ("zip", &["-mem=AES256", "-psecret"], Some("secret")),
        ("7z", &["-ms=on"], None),
        ("7z", &["-ms=on", "-psecret", "-mhe=on"], Some("secret")),
        ("7z", &["-ms=on", "-psecret", "-mhe=off"], Some("secret")),
    ];
    for &(extension, flags, password) in cases {
        eprintln!("Incremental interop: {extension} {flags:?}");
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("keep.txt"), b"unchanged").unwrap();
        fs::write(temp.path().join("change.txt"), b"original").unwrap();
        fs::write(temp.path().join("remove.txt"), b"deleted").unwrap();
        let name = format!("foreign.{extension}");
        let format = format!("-t{extension}");
        let mut args = vec!["a", &format, &name, "keep.txt", "change.txt", "remove.txt"];
        args.extend_from_slice(flags);
        run_7z(temp.path(), &args);
        run_7z(temp.path(), &["t", "-psecret", &name]);
        let control = JobControl::default();
        let info = inspect_archive(&temp.path().join(&name), password, &control).unwrap();
        fs::write(temp.path().join("change.txt"), b"replacement").unwrap();
        let info = edit_archive(
            &info,
            &ArchiveEdit::Add {
                sources: vec![temp.path().join("change.txt")],
                directory: String::new(),
                overwrite: true,
            },
            password,
            &control,
        )
        .unwrap();
        run_7z(temp.path(), &["t", "-psecret", &name]);
        edit_archive(
            &info,
            &ArchiveEdit::Delete {
                paths: vec!["remove.txt".into()],
            },
            password,
            &control,
        )
        .unwrap();
        run_7z(temp.path(), &["t", "-psecret", &name]);
        run_7z(temp.path(), &["x", "-y", "-psecret", "-oexternal", &name]);
        let output = temp.path().join("external");
        assert_eq!(fs::read(output.join("keep.txt")).unwrap(), b"unchanged");
        assert_eq!(fs::read(output.join("change.txt")).unwrap(), b"replacement");
        assert!(!output.join("remove.txt").exists());
    }
}

fn run_7z(directory: &Path, args: &[&str]) {
    let output = Command::new("7z")
        .current_dir(directory)
        .args(args)
        .output()
        .expect("install 7-Zip or NanaZip and make its 7z command available on PATH");
    assert!(
        output.status.success(),
        "7z failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[ignore = "requires a 7-Zip-compatible 7z command on PATH"]
fn archives_are_interoperable_with_7zip_in_both_directions() {
    let directory = TempDir::new().unwrap();
    let source = directory.path().join("input");
    fs::create_dir_all(source.join("nested")).unwrap();
    fs::write(source.join("nested").join("alpha.txt"), b"alpha").unwrap();
    fs::write(source.join("beta.txt"), b"beta").unwrap();
    let modified = SystemTime::UNIX_EPOCH
        + Duration::from_secs(1_600_000_000)
        + Duration::from_nanos(123_456_700);
    let zip_modified = if cfg!(windows) {
        modified
    } else {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000)
    };
    fs::OpenOptions::new()
        .write(true)
        .open(source.join("nested").join("alpha.txt"))
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(modified))
        .unwrap();

    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        let archive = directory
            .path()
            .join(format!("mzip.{}", format.extension()));
        create_archive(
            &CreateOptions {
                sources: vec![source.clone()],
                destination: archive.clone(),
                format,
                password: Some(PASSWORD.to_owned()),
                overwrite: false,
            },
            &JobControl::default(),
        )
        .unwrap();
        let name = archive.file_name().unwrap().to_str().unwrap();
        run_7z(directory.path(), &["t", "-pMZip-interop-test", name]);
        let output = format!("from-mzip-{}", format.extension());
        run_7z(
            directory.path(),
            &["x", "-pMZip-interop-test", &format!("-o{output}"), name],
        );
        let extracted = directory
            .path()
            .join(output)
            .join("input")
            .join("nested")
            .join("alpha.txt");
        assert_eq!(fs::read(&extracted).unwrap(), b"alpha");
        assert_eq!(
            fs::metadata(extracted).unwrap().modified().unwrap(),
            if format == ArchiveFormat::Zip {
                zip_modified
            } else {
                modified
            },
            "{format:?} timestamp when extracted by 7z"
        );
    }

    run_7z(
        directory.path(),
        &[
            "a",
            "-t7z",
            "-ms=on",
            "-mhe=on",
            "-pMZip-interop-test",
            "external.7z",
            "input",
        ],
    );
    run_7z(
        directory.path(),
        &[
            "a",
            "-tzip",
            "-mem=AES256",
            "-pMZip-interop-test",
            "external.zip",
            "input",
        ],
    );

    for extension in ["zip", "7z"] {
        let archive = directory.path().join(format!("external.{extension}"));
        let info = inspect_archive(&archive, Some(PASSWORD), &JobControl::default()).unwrap();
        let destination = directory.path().join(format!("output-{extension}"));
        extract_archive(
            &info,
            &ExtractOptions {
                destination: destination.clone(),
                password: Some(PASSWORD.to_owned()),
                selected: None,
                overwrite: false,
            },
            &JobControl::default(),
        )
        .unwrap();
        assert_eq!(
            fs::read(destination.join("input").join("nested").join("alpha.txt")).unwrap(),
            b"alpha"
        );
        assert_eq!(
            fs::metadata(destination.join("input").join("nested").join("alpha.txt"))
                .unwrap()
                .modified()
                .unwrap(),
            if extension == "zip" {
                zip_modified
            } else {
                modified
            },
            "{extension} timestamp when extracted by MZip"
        );
        assert_eq!(
            fs::read(destination.join("input").join("beta.txt")).unwrap(),
            b"beta"
        );
    }
}

#[test]
#[ignore = "requires a 7-Zip-compatible 7z command on PATH"]
fn filtered_sevenzip_archives_are_interoperable_with_7zip() {
    let directory = TempDir::new().unwrap();
    let repeated_code = support::pe_fixture(0x8664, 256 * 1024);
    let files = [
        ("x86.exe", support::pe_fixture(0x8664, 128 * 1024)),
        ("arm64.dll", support::pe_fixture(0xaa64, 96 * 1024)),
        ("repeat-1.bin", vec![b'r'; 256 * 1024]),
        ("repeat-2.bin", vec![b'r'; 256 * 1024]),
        ("repeat-code-1.exe", repeated_code.clone()),
        ("repeat-code-2.exe", repeated_code),
    ];
    let sources = files
        .iter()
        .map(|(name, bytes)| {
            let path = directory.path().join(name);
            fs::write(&path, bytes).unwrap();
            path
        })
        .collect::<Vec<_>>();
    for password in [None, Some(PASSWORD)] {
        let suffix = if password.is_some() {
            "encrypted"
        } else {
            "plain"
        };
        let archive_name = format!("filtered-{suffix}.7z");
        create_archive(
            &CreateOptions {
                sources: sources.clone(),
                destination: directory.path().join(&archive_name),
                format: ArchiveFormat::SevenZip,
                password: password.map(str::to_owned),
                overwrite: false,
            },
            &JobControl::default(),
        )
        .unwrap();
        let reader = ArchiveReader::open(
            directory.path().join(&archive_name),
            password.map(Password::from).unwrap_or_default(),
        )
        .unwrap();
        let metadata = reader.archive();
        let block_for = |name| {
            let file_index = metadata
                .files
                .iter()
                .position(|entry| entry.name == name)
                .unwrap();
            metadata.stream_map.file_block_index[file_index].unwrap()
        };
        let block = block_for("repeat-code-1.exe");
        assert_eq!(block, block_for("repeat-code-2.exe"));
        assert!(
            metadata.blocks[block]
                .coders
                .iter()
                .any(|coder| coder.encoder_method_id() == EncoderMethod::ID_BCJ_X86)
        );
        let output = format!("filtered-output-{suffix}");
        let output_arg = format!("-o{output}");
        let mut test_args = vec!["t"];
        let mut extract_args = vec!["x"];
        if password.is_some() {
            test_args.push("-pMZip-interop-test");
            extract_args.push("-pMZip-interop-test");
        }
        test_args.push(&archive_name);
        extract_args.extend([output_arg.as_str(), archive_name.as_str()]);
        run_7z(directory.path(), &test_args);
        run_7z(directory.path(), &extract_args);
        for (name, bytes) in &files {
            assert_eq!(
                &fs::read(directory.path().join(&output).join(name)).unwrap(),
                bytes
            );
        }
    }
}

#[test]
#[ignore = "requires a 7-Zip-compatible 7z command on PATH"]
fn filtered_large_sevenzip_is_interoperable_with_7zip() {
    let directory = TempDir::new().unwrap();
    let contents = support::pe_fixture(0x8664, 32 * 1024 * 1024 + 7);
    let source = directory.path().join("large.exe");
    fs::write(&source, &contents).unwrap();
    let archive_name = "filtered-large.7z";
    create_archive(
        &CreateOptions {
            sources: vec![source],
            destination: directory.path().join(archive_name),
            format: ArchiveFormat::SevenZip,
            password: Some(PASSWORD.to_owned()),
            overwrite: false,
        },
        &JobControl::default(),
    )
    .unwrap();
    run_7z(
        directory.path(),
        &["t", "-pMZip-interop-test", archive_name],
    );
    run_7z(
        directory.path(),
        &[
            "x",
            "-pMZip-interop-test",
            "-ofiltered-large-output",
            archive_name,
        ],
    );
    assert_eq!(
        fs::read(
            directory
                .path()
                .join("filtered-large-output")
                .join("large.exe")
        )
        .unwrap(),
        contents
    );
}

#[test]
#[ignore = "requires a 7-Zip-compatible 7z command on PATH"]
fn parallel_archives_are_interoperable_with_7zip() {
    let directory = TempDir::new().unwrap();
    let sources = (0..4)
        .map(|index| {
            let path = directory.path().join(format!("file-{index}.bin"));
            fs::write(&path, vec![index as u8 + 1; 1024 * 1024]).unwrap();
            path
        })
        .collect::<Vec<_>>();
    let before = directory.path().join("before.txt");
    let after = directory.path().join("after.txt");
    fs::write(&before, b"before").unwrap();
    fs::write(&after, b"after").unwrap();
    let mut mixed_sources = vec![before];
    mixed_sources.extend(sources);
    // Small files share one solid block; the empty file has no stream.
    for index in 0..5 {
        let path = directory.path().join(format!("small-{index}.txt"));
        fs::write(&path, format!("small {index} ").repeat(1000)).unwrap();
        mixed_sources.push(path);
    }
    let empty = directory.path().join("empty.txt");
    fs::write(&empty, b"").unwrap();
    mixed_sources.push(empty);
    mixed_sources.push(after);
    let large = directory.path().join("large.bin");
    fs::File::create(&large)
        .unwrap()
        .set_len(32 * 1024 * 1024)
        .unwrap();
    for (name, format, inputs) in [
        ("parallel.zip", ArchiveFormat::Zip, mixed_sources.clone()),
        ("parallel-files.7z", ArchiveFormat::SevenZip, mixed_sources),
        (
            "parallel-large.7z",
            ArchiveFormat::SevenZip,
            vec![large.clone()],
        ),
    ] {
        let archive = directory.path().join(name);
        create_archive(
            &CreateOptions {
                sources: inputs.clone(),
                destination: archive.clone(),
                format,
                password: Some(PASSWORD.to_owned()),
                overwrite: false,
            },
            &JobControl::default(),
        )
        .unwrap();
        let name = archive.file_name().unwrap().to_str().unwrap();
        run_7z(directory.path(), &["t", "-pMZip-interop-test", name]);
        let output = format!("parallel-out-{name}");
        run_7z(
            directory.path(),
            &["x", "-pMZip-interop-test", &format!("-o{output}"), name],
        );
        for source in inputs {
            assert_eq!(
                fs::read(
                    directory
                        .path()
                        .join(&output)
                        .join(source.file_name().unwrap())
                )
                .unwrap(),
                fs::read(source).unwrap()
            );
        }
    }

    run_7z(
        directory.path(),
        &[
            "a",
            "-t7z",
            "-m0=lzma2",
            "-mmt=4",
            "-ms=off",
            "-mhe=on",
            "-pMZip-interop-test",
            "external-large.7z",
            "large.bin",
        ],
    );
    let archive = directory.path().join("external-large.7z");
    let info = inspect_archive(&archive, Some(PASSWORD), &JobControl::default()).unwrap();
    let output = directory.path().join("external-large-output");
    extract_archive(
        &info,
        &ExtractOptions {
            destination: output.clone(),
            password: Some(PASSWORD.to_owned()),
            selected: None,
            overwrite: false,
        },
        &JobControl::default(),
    )
    .unwrap();
    assert_eq!(
        fs::read(output.join("large.bin")).unwrap(),
        fs::read(large).unwrap()
    );
}
