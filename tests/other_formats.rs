use std::{
    collections::BTreeSet,
    fs,
    io::{Cursor, Read, Write},
    path::{Path, PathBuf},
};

use hx6zip::archive::{
    ArchiveKind, CancellationToken, Compression, ExtractOptions, JobControl, SUPPORTED_EXTENSIONS,
    extract_archive, inspect_archive, smart_destination,
};
use tempfile::TempDir;

#[path = "support/direct_drag.rs"]
mod direct_drag;

fn tar_sample() -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (name, contents) in [
        ("folder/alpha.txt", b"alpha".as_slice()),
        ("folder/nested/beta.txt", b"beta".as_slice()),
        ("sibling.txt", b"sibling".as_slice()),
    ] {
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(1_600_000_000);
        header.set_cksum();
        builder.append_data(&mut header, name, contents).unwrap();
    }
    builder.into_inner().unwrap()
}

#[test]
fn compressed_tar_write_queues_preserve_selected_implicit_directories() {
    let mut builder = tar::Builder::new(Vec::new());
    for index in 0..32 {
        let contents = vec![index as u8; 4096];
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(1_600_000_000);
        header.set_cksum();
        builder
            .append_data(
                &mut header,
                format!("implicit/nested/{index:03}.bin"),
                contents.as_slice(),
            )
            .unwrap();
    }
    let contents = builder.into_inner().unwrap();
    let temp = TempDir::new().unwrap();
    for (suffix, compression) in [
        ("tar", Compression::None),
        ("tar.gz", Compression::Gzip),
        ("tar.bz2", Compression::Bzip2),
        ("tar.xz", Compression::Xz),
        ("tar.zst", Compression::Zstd),
        ("tar.br", Compression::Brotli),
    ] {
        let archive = temp.path().join(format!("queue.{suffix}"));
        fs::write(&archive, compress(&contents, compression)).unwrap();
        let info = inspect_archive(&archive, None, &JobControl::default()).unwrap();
        let selected = (0..24).collect::<BTreeSet<_>>();
        for mode in [
            hx6zip::archive::IntegrityMode::Safe,
            hx6zip::archive::IntegrityMode::Normal,
        ] {
            let destination = temp.path().join(format!("{suffix}-{}", mode.label()));
            extract_archive(
                &info,
                &ExtractOptions {
                    destination: destination.clone(),
                    password: None,
                    selected: Some(selected.clone()),
                    overwrite: false,
                },
                &JobControl::default().with_integrity(mode),
            )
            .unwrap();
            for index in 0..24 {
                assert_eq!(
                    fs::read(
                        destination
                            .join("implicit")
                            .join("nested")
                            .join(format!("{index:03}.bin"))
                    )
                    .unwrap(),
                    vec![index as u8; 4096]
                );
            }
            assert!(
                !destination
                    .join("implicit")
                    .join("nested")
                    .join("024.bin")
                    .exists()
            );
        }
    }
}

fn compress(contents: &[u8], compression: Compression) -> Vec<u8> {
    match compression {
        Compression::None => contents.to_vec(),
        Compression::Gzip => {
            let mut writer =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            writer.write_all(contents).unwrap();
            writer.finish().unwrap()
        }
        Compression::Bzip2 => {
            let mut writer =
                bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::default());
            writer.write_all(contents).unwrap();
            writer.finish().unwrap()
        }
        Compression::Xz => {
            let mut writer =
                lzma_rust2::XzWriter::new(Vec::new(), lzma_rust2::XzOptions::default()).unwrap();
            writer.write_all(contents).unwrap();
            writer.finish().unwrap()
        }
        Compression::Zstd => zstd::stream::encode_all(contents, 0).unwrap(),
        Compression::Brotli => {
            let mut writer = brotli::CompressorWriter::new(Vec::new(), 4096, 5, 22);
            writer.write_all(contents).unwrap();
            writer.into_inner()
        }
    }
}

fn extract(
    path: &Path,
    destination: &Path,
    selected: Option<BTreeSet<usize>>,
) -> anyhow::Result<()> {
    let info = inspect_archive(path, None, &JobControl::default())?;
    extract_archive(
        &info,
        &ExtractOptions {
            destination: destination.to_owned(),
            password: None,
            selected,
            overwrite: false,
        },
        &JobControl::default(),
    )
}

fn write_archive(directory: &TempDir, name: &str, data: &[u8]) -> PathBuf {
    let path = directory.path().join(name);
    fs::write(&path, data).unwrap();
    path
}

#[test]
fn all_advertised_extensions_are_registered_and_detected() {
    let manifest = include_str!(r"..\installer\Package.wxs");
    for extension in SUPPORTED_EXTENSIONS {
        let path = PathBuf::from(format!("sample.{extension}"));
        assert!(ArchiveKind::from_path(&path).is_ok(), "{extension}");
        assert!(
            manifest.contains(&format!("Name=\".{extension}\" Value=\"Hx6Zip.Archive.1\"")),
            "missing default-app registration for {extension}"
        );
        assert!(
            manifest.contains(&format!("Software\\Classes\\.{extension}\\OpenWithProgids")),
            "missing Open With registration for {extension}"
        );
        assert!(hx6zip_shell_protocol::can_attempt_extraction(&path));
    }
    assert_eq!(
        ArchiveKind::from_path(Path::new("example.TAR.GZ")).unwrap(),
        ArchiveKind::Tar(Compression::Gzip)
    );
    assert!(ArchiveKind::from_path(Path::new("example.exe")).is_err());
    assert!(manifest.contains(r"Software\RegisteredApplications"));
    assert!(manifest.contains(r"Software\Hx6Zip\Capabilities"));
    assert!(!manifest.contains("UserChoice"));
}

#[test]
fn tar_variants_browse_extract_and_preserve_selection() {
    let directory = TempDir::new().unwrap();
    for (name, compression) in [
        ("plain.tar", Compression::None),
        ("package.tar.gz", Compression::Gzip),
        ("package.tgz", Compression::Gzip),
        ("package.tar.bz2", Compression::Bzip2),
        ("package.tbz2", Compression::Bzip2),
        ("package.tar.xz", Compression::Xz),
        ("package.txz", Compression::Xz),
        ("package.tar.zst", Compression::Zstd),
        ("package.tzst", Compression::Zstd),
        ("package.tar.br", Compression::Brotli),
        ("package.tbr", Compression::Brotli),
    ] {
        let path = write_archive(&directory, name, &compress(&tar_sample(), compression));
        let info = inspect_archive(&path, None, &JobControl::default()).unwrap();
        assert_eq!(info.entries.len(), 3, "{name}");
        assert_eq!(info.total_size, 16, "{name}");
        direct_drag::check(
            &info,
            &["folder/nested", "sibling.txt"],
            &[("nested/beta.txt", b"beta"), ("sibling.txt", b"sibling")],
            None,
        );
        if name == "package.tar.gz" {
            assert_eq!(
                smart_destination(&info, directory.path()).unwrap(),
                directory.path().join("package")
            );
        }
        let index = info
            .entries
            .iter()
            .find(|entry| entry.name.ends_with("beta.txt"))
            .unwrap()
            .index;
        let output = directory.path().join(format!("{name}-selected"));
        extract(&path, &output, Some(BTreeSet::from([index]))).unwrap();
        assert_eq!(
            fs::read(output.join("folder").join("nested").join("beta.txt")).unwrap(),
            b"beta"
        );
        assert!(!output.join("folder").join("alpha.txt").exists());
        assert!(!output.join("sibling.txt").exists());
        let output = directory.path().join(format!("{name}-all"));
        extract(&path, &output, None).unwrap();
        assert_eq!(fs::read(output.join("sibling.txt")).unwrap(), b"sibling");
    }
}

#[test]
fn standalone_compressed_files_are_viewed_and_extracted() {
    let directory = TempDir::new().unwrap();
    for (extension, compression) in [
        ("gz", Compression::Gzip),
        ("bz2", Compression::Bzip2),
        ("xz", Compression::Xz),
        ("zst", Compression::Zstd),
        ("zstd", Compression::Zstd),
        ("br", Compression::Brotli),
        ("brotli", Compression::Brotli),
    ] {
        let path = write_archive(
            &directory,
            &format!("content.txt.{extension}"),
            &compress(b"compressed contents", compression),
        );
        let info = inspect_archive(&path, None, &JobControl::default()).unwrap();
        assert_eq!(info.entries[0].name, "content.txt");
        assert_eq!(info.entries[0].size, 19);
        direct_drag::check(
            &info,
            &["content.txt"],
            &[("content.txt", b"compressed contents")],
            None,
        );
        let output = directory.path().join(format!("{extension}-output"));
        extract(&path, &output, None).unwrap();
        assert_eq!(
            fs::read(output.join("content.txt")).unwrap(),
            b"compressed contents"
        );
    }
}

#[test]
fn zip_based_formats_browse_and_extract() {
    let directory = TempDir::new().unwrap();
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    writer
        .start_file(
            "folder/content.txt",
            zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated),
        )
        .unwrap();
    writer.write_all(b"zip container").unwrap();
    let data = writer.finish().unwrap().into_inner();
    for extension in [
        "zipx", "jar", "odt", "ods", "odp", "odg", "odf", "odb", "sxw", "sxc", "sxi", "sxd",
    ] {
        let path = write_archive(&directory, &format!("document.{extension}"), &data);
        let info = inspect_archive(&path, None, &JobControl::default()).unwrap();
        assert_eq!(info.format, ArchiveKind::Zip);
        assert_eq!(info.entries[0].name, "folder/content.txt");
        let output = directory.path().join(format!("{extension}-output"));
        extract(&path, &output, None).unwrap();
        assert_eq!(
            fs::read(output.join("folder").join("content.txt")).unwrap(),
            b"zip container"
        );
    }
}

#[test]
fn brotli_large_window_is_rejected_before_decoder_allocation() {
    let directory = TempDir::new().unwrap();
    let mut params = brotli::enc::BrotliEncoderParams::default();
    params.quality = 2;
    params.lgwin = 25;
    params.large_window = true;
    let mut compressed = Vec::new();
    brotli::BrotliCompress(
        &mut Cursor::new(b"large-window sample"),
        &mut compressed,
        &params,
    )
    .unwrap();
    assert_eq!(compressed[0] & 0x7f, 0x11);
    let mut decoded = Vec::new();
    brotli::Decompressor::new(compressed.as_slice(), 4096)
        .read_to_end(&mut decoded)
        .unwrap();
    assert_eq!(decoded, b"large-window sample");
    for extension in ["br", "tar.br"] {
        let path = write_archive(&directory, &format!("large.{extension}"), &compressed);
        let error = inspect_archive(&path, None, &JobControl::default()).unwrap_err();
        assert!(format!("{error:#}").contains("大窗口 Brotli"), "{error:#}");
    }
}

#[test]
fn cab_contents_can_be_viewed_and_extracted() {
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("sample.cab");
    let mut builder = cab::CabinetBuilder::new();
    let folder = builder.add_folder(cab::CompressionType::MsZip);
    folder.add_file("资料/first.txt");
    folder.add_file("other.txt");
    let mut writer = builder.build(fs::File::create(&path).unwrap()).unwrap();
    while let Some(mut file) = writer.next_file().unwrap() {
        let data = if file.file_name().ends_with("first.txt") {
            b"first".as_slice()
        } else {
            b"other".as_slice()
        };
        file.write_all(data).unwrap();
    }
    writer.finish().unwrap();
    let info = inspect_archive(&path, None, &JobControl::default()).unwrap();
    assert_eq!(info.entries.len(), 2);
    assert!(info.entries.iter().any(|entry| entry.name.contains("资料")));
    let output = directory.path().join("cab-output");
    extract(&path, &output, None).unwrap();
    assert_eq!(
        fs::read(output.join("资料").join("first.txt")).unwrap(),
        b"first"
    );
    assert_eq!(fs::read(output.join("other.txt")).unwrap(), b"other");
    direct_drag::check(
        &info,
        &["资料/first.txt", "other.txt"],
        &[("first.txt", b"first"), ("other.txt", b"other")],
        None,
    );
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (0u32.wrapping_sub(crc & 1)));
        }
    }
    !crc
}

fn rar_header(body: &[u8]) -> Vec<u8> {
    let mut header = (crc32(body) as u16).to_le_bytes().to_vec();
    header.extend_from_slice(body);
    header
}

fn rar_sample(second_declared_size: u64) -> Vec<u8> {
    let mut archive = b"Rar!\x1a\x07\x00".to_vec();
    archive.extend(rar_header(&[0x73, 0, 0, 13, 0, 0, 0, 0, 0, 0, 0]));
    for (name, contents, declared_size) in [
        ("folder\\first.txt", b"first".as_slice(), 5),
        ("other.txt", b"other".as_slice(), second_declared_size),
    ] {
        let large = declared_size > u64::from(u32::MAX);
        let flags: u16 = 0x8000 | if large { 0x0100 } else { 0 };
        let mut header = vec![0x74];
        header.extend_from_slice(&flags.to_le_bytes());
        header.extend_from_slice(
            &(32u16 + name.len() as u16 + if large { 8 } else { 0 }).to_le_bytes(),
        );
        header.extend_from_slice(&(contents.len() as u32).to_le_bytes());
        header.extend_from_slice(&(declared_size as u32).to_le_bytes());
        header.push(2);
        header.extend_from_slice(&crc32(contents).to_le_bytes());
        header.extend_from_slice(&0x4a21_0000u32.to_le_bytes());
        header.extend_from_slice(&[20, 0x30]);
        header.extend_from_slice(&(name.len() as u16).to_le_bytes());
        header.extend_from_slice(&0x20u32.to_le_bytes());
        if large {
            header.extend_from_slice(&0u32.to_le_bytes());
            header.extend_from_slice(&((declared_size >> 32) as u32).to_le_bytes());
        }
        header.extend_from_slice(name.as_bytes());
        archive.extend(rar_header(&header));
        archive.extend_from_slice(contents);
    }
    archive.extend(rar_header(&[0x7b, 0, 0, 7, 0]));
    archive
}

#[test]
fn rar4_contents_can_be_viewed_and_selectively_extracted() {
    let directory = TempDir::new().unwrap();
    let path = write_archive(&directory, "sample.rar", &rar_sample(5));
    let info = inspect_archive(&path, None, &JobControl::default()).unwrap();
    assert_eq!(info.entries.len(), 2);
    assert_eq!(info.total_size, 10);
    let first = info
        .entries
        .iter()
        .find(|entry| entry.name.ends_with("first.txt"))
        .unwrap()
        .index;
    let output = directory.path().join("rar-output");
    extract(&path, &output, Some(BTreeSet::from([first]))).unwrap();
    assert_eq!(
        fs::read(output.join("folder").join("first.txt")).unwrap(),
        b"first"
    );
    assert!(!output.join("other.txt").exists());
    direct_drag::check(
        &info,
        &["folder/first.txt", "other.txt"],
        &[("first.txt", b"first"), ("other.txt", b"other")],
        None,
    );
}

#[test]
fn rar4_self_extracting_file_can_be_viewed_and_extracted() {
    let directory = TempDir::new().unwrap();
    let mut data = b"MZ".to_vec();
    data.resize(65533, 0);
    data.extend(rar_sample(5));
    let path = write_archive(&directory, "sample.exe", &data);
    let info = inspect_archive(&path, None, &JobControl::default()).unwrap();
    assert_eq!(info.format, ArchiveKind::Rar);
    let output = directory.path().join("output");
    extract(&path, &output, None).unwrap();
    assert_eq!(
        fs::read(output.join("folder").join("first.txt")).unwrap(),
        b"first"
    );
    assert_eq!(fs::read(output.join("other.txt")).unwrap(), b"other");
    direct_drag::check(
        &info,
        &["folder/first.txt"],
        &[("first.txt", b"first")],
        None,
    );
}

#[test]
fn rar4_selected_extraction_accepts_over_100_gib_declared_size() {
    const DECLARED_SIZE: u64 = 100 * 1024 * 1024 * 1024 + 1;
    let directory = TempDir::new().unwrap();
    // The oversized entry has a short payload so the test does not write 100 GiB.
    let path = write_archive(&directory, "large.rar", &rar_sample(DECLARED_SIZE));
    let info = inspect_archive(&path, None, &JobControl::default()).unwrap();
    assert_eq!(info.entries[1].size, DECLARED_SIZE);
    assert_eq!(info.total_size, DECLARED_SIZE + 5);

    let output = directory.path().join("selected");
    extract(&path, &output, Some(BTreeSet::from([0]))).unwrap();
    assert_eq!(
        fs::read(output.join("folder").join("first.txt")).unwrap(),
        b"first"
    );
    assert!(!output.join("other.txt").exists());

    let output = directory.path().join("incomplete");
    assert!(extract(&path, &output, Some(BTreeSet::from([1]))).is_err());
    assert!(!output.join("other.txt").exists());
}

#[test]
fn unsafe_tar_entries_and_corrupt_streams_are_rejected() {
    let directory = TempDir::new().unwrap();
    for (name, kind) in [
        ("../escape.txt", tar::EntryType::Regular),
        ("link", tar::EntryType::Symlink),
    ] {
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(0);
        header.set_mode(0o644);
        header.set_mtime(1_600_000_000);
        header.set_entry_type(kind);
        header.as_mut_bytes()[..name.len()].copy_from_slice(name.as_bytes());
        header.set_cksum();
        builder.append(&header, Cursor::new([])).unwrap();
        let path = write_archive(
            &directory,
            &format!("{kind:?}.tar"),
            &builder.into_inner().unwrap(),
        );
        let output = directory.path().join(format!("{kind:?}-output"));
        assert!(extract(&path, &output, None).is_err());
        assert!(!directory.path().join("escape.txt").exists());
    }
    let mut gzip = compress(b"hello", Compression::Gzip);
    gzip.pop();
    let path = write_archive(&directory, "broken.gz", &gzip);
    assert!(inspect_archive(&path, None, &JobControl::default()).is_err());
}

#[test]
fn tar_extended_metadata_limit_is_enforced_at_the_boundary() {
    let directory = TempDir::new().unwrap();
    for (size, accepted) in [(1024 * 1024, true), (1024 * 1024 + 1, false)] {
        let prefix = format!("{size} comment=");
        let mut metadata = prefix.into_bytes();
        metadata.resize(size - 1, b'a');
        metadata.push(b'\n');
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::XHeader);
        header.set_size(size as u64);
        header.set_mode(0o644);
        header.set_mtime(1_600_000_000);
        header.set_cksum();
        builder
            .append_data(&mut header, "metadata", metadata.as_slice())
            .unwrap();
        let mut header = tar::Header::new_gnu();
        header.set_size(1);
        header.set_mode(0o644);
        header.set_mtime(1_600_000_000);
        header.set_cksum();
        builder
            .append_data(&mut header, "content.txt", b"x".as_slice())
            .unwrap();
        let path = write_archive(
            &directory,
            &format!("metadata-{size}.tar"),
            &builder.into_inner().unwrap(),
        );
        let result = inspect_archive(&path, None, &JobControl::default());
        if accepted {
            assert_eq!(result.unwrap().entries[0].name, "content.txt");
        } else {
            assert!(format!("{:#}", result.unwrap_err()).contains("元数据超过"));
        }
    }
}

#[test]
fn zstd_window_limit_is_enforced_at_the_boundary() {
    let directory = TempDir::new().unwrap();
    for (window_descriptor, accepted) in [(0x90, true), (0x98, false)] {
        let frame = [0x28, 0xb5, 0x2f, 0xfd, 0, window_descriptor, 1, 0, 0];
        let path = write_archive(
            &directory,
            &format!("window-{window_descriptor:x}.zst"),
            &frame,
        );
        let result = inspect_archive(&path, None, &JobControl::default());
        if accepted {
            assert_eq!(result.unwrap().entries[0].size, 0);
        } else {
            assert!(
                result.is_err(),
                "Zstandard window above 256 MiB was accepted"
            );
        }
    }
}

#[test]
fn cancelled_new_format_reads_stop_without_creating_output() {
    let directory = TempDir::new().unwrap();
    let path = write_archive(
        &directory,
        "cancel.tar.gz",
        &compress(&tar_sample(), Compression::Gzip),
    );
    let token = CancellationToken::default();
    token.cancel();
    let control = JobControl::new(token, |_| {});
    assert!(inspect_archive(&path, None, &control).is_err());
    assert!(!directory.path().join("output").exists());
}

#[test]
#[ignore = "requires Windows tar.exe and a 7-Zip-compatible tool with Zstandard support"]
fn external_tar_and_compressors_are_interoperable() {
    use std::process::Command;

    fn run(command: &mut Command) {
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "external archiver failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let directory = TempDir::new().unwrap();
    let source = directory.path().join("sources");
    let long_directory = format!("资料-{}", "long".repeat(26));
    fs::create_dir_all(source.join(&long_directory)).unwrap();
    fs::write(
        source.join(&long_directory).join("nested.txt"),
        b"linux tar",
    )
    .unwrap();
    fs::write(source.join("other.txt"), b"other").unwrap();

    let tar = directory.path().join("external.tar");
    run(Command::new("tar.exe")
        .arg("-cf")
        .arg(&tar)
        .arg("-C")
        .arg(&source)
        .arg("."));
    for (compression, extension) in [
        ("gzip", "gz"),
        ("bzip2", "bz2"),
        ("xz", "xz"),
        ("zstd", "zst"),
    ] {
        let path = directory.path().join(format!("external.tar.{extension}"));
        run(Command::new("7z")
            .arg("a")
            .arg(format!("-t{compression}"))
            .arg(&path)
            .arg(&tar));
        let info = inspect_archive(&path, None, &JobControl::default()).unwrap();
        assert!(
            info.entries
                .iter()
                .any(|entry| entry.name.ends_with("nested.txt"))
        );
        let output = directory
            .path()
            .join(format!("{extension}-external-output"));
        extract(&path, &output, None).unwrap();
        assert_eq!(
            fs::read(output.join(&long_directory).join("nested.txt")).unwrap(),
            b"linux tar"
        );
        assert_eq!(fs::read(output.join("other.txt")).unwrap(), b"other");
    }
}
