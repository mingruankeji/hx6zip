use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use hx6zip::archive::{ArchiveInfo, ExtractOptions, JobControl, extract_archive, inspect_archive};
use tempfile::TempDir;

#[path = "support/direct_drag.rs"]
mod direct_drag;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("libarchive")
        .join(name)
}

fn inspect(path: &Path, password: Option<&str>) -> anyhow::Result<ArchiveInfo> {
    inspect_archive(path, password, &JobControl::default())
}

fn extract(
    info: &ArchiveInfo,
    output: &Path,
    password: Option<&str>,
    selected: Option<BTreeSet<usize>>,
) -> anyhow::Result<()> {
    extract_archive(
        info,
        &ExtractOptions {
            destination: output.to_owned(),
            password: password.map(str::to_owned),
            selected,
            overwrite: false,
        },
        &JobControl::default(),
    )
}

fn selected_entry(info: &ArchiveInfo, name: &str) -> BTreeSet<usize> {
    BTreeSet::from([info
        .entries
        .iter()
        .find(|entry| entry.name == name)
        .unwrap()
        .index])
}

fn expected_data(size: usize, magic: i32) -> Vec<u8> {
    (1..=size / 4)
        .flat_map(|index| {
            let index = index as i32;
            (index * index - 3 * index + 1 + magic).max(0).to_le_bytes()
        })
        .collect()
}

#[test]
fn compressed_rar5_is_viewed_and_extracted() {
    let path = fixture("test_read_format_rar5_compressed.rar");
    let info = inspect(&path, None).unwrap();
    assert_eq!(info.entries.len(), 1);
    assert_eq!(info.entries[0].name, "test.bin");
    assert_eq!(info.entries[0].size, 1200);
    let output = TempDir::new().unwrap();
    extract(&info, output.path(), None, None).unwrap();
    assert_eq!(
        fs::read(output.path().join("test.bin")).unwrap(),
        expected_data(1200, 0)
    );
}

#[test]
fn rar5_sfx_supports_compression_and_encrypted_filenames() {
    let directory = TempDir::new().unwrap();
    for (name, password, selected) in [
        ("test_read_format_rar5_compressed.rar", None, "test.bin"),
        (
            "test_read_format_rar5_encrypted_filenames.rar",
            Some("password"),
            "c.txt",
        ),
    ] {
        let mut data = b"MZ".to_vec();
        data.resize(65533, 0);
        data.extend(fs::read(fixture(name)).unwrap());
        let path = directory.path().join(format!("{name}.exe"));
        fs::write(&path, data).unwrap();
        if password.is_some() {
            assert!(inspect(&path, None).is_err());
            assert!(inspect(&path, Some("wrong")).is_err());
        }
        let info = inspect(&path, password).unwrap();
        let output = directory.path().join(name);
        extract(
            &info,
            &output,
            password,
            Some(selected_entry(&info, selected)),
        )
        .unwrap();
        let expected = if password.is_some() {
            b"This is from c.txt".to_vec()
        } else {
            expected_data(1200, 0)
        };
        assert_eq!(fs::read(output.join(selected)).unwrap(), expected);
    }
}

#[test]
fn solid_rar5_selectively_extracts_later_entries() {
    let path = fixture("test_read_format_rar5_multiple_files_solid.rar");
    let info = inspect(&path, None).unwrap();
    assert_eq!(info.entries.len(), 4);
    let output = TempDir::new().unwrap();
    extract(
        &info,
        output.path(),
        None,
        Some(selected_entry(&info, "test4.bin")),
    )
    .unwrap();
    assert_eq!(
        fs::read(output.path().join("test4.bin")).unwrap(),
        expected_data(4096, 4)
    );
    assert!(!output.path().join("test1.bin").exists());
    assert!(!output.path().join("test3.bin").exists());
}

#[test]
fn rar5_passwords_and_encrypted_filenames_are_checked() {
    let path = fixture("test_read_format_rar5_encrypted_filenames.rar");
    assert!(inspect(&path, None).is_err());
    assert!(inspect(&path, Some("wrong")).is_err());
    let info = inspect(&path, Some("password")).unwrap();
    assert_eq!(info.entries.len(), 4);
    let output = TempDir::new().unwrap();
    let selected = Some(selected_entry(&info, "c.txt"));
    assert!(extract(&info, output.path(), Some("wrong"), selected.clone()).is_err());
    assert!(!output.path().join("c.txt").exists());
    extract(&info, output.path(), Some("password"), selected).unwrap();
    assert_eq!(
        fs::read(output.path().join("c.txt")).unwrap(),
        b"This is from c.txt"
    );

    let path = fixture("test_read_format_rar5_encrypted.rar");
    let info = inspect(&path, None).unwrap();
    let selected = Some(selected_entry(&info, "d.txt"));
    assert!(extract(&info, output.path(), None, selected.clone()).is_err());
    extract(&info, output.path(), Some("password2"), selected).unwrap();
    assert_eq!(
        fs::read(output.path().join("d.txt")).unwrap(),
        b"This is from d.txt"
    );
}

#[test]
fn solid_encrypted_rar5_can_skip_to_a_selected_file() {
    let path = fixture("test_read_format_rar5_solid_encrypted.rar");
    let info = inspect(&path, None).unwrap();
    let output = TempDir::new().unwrap();
    extract(
        &info,
        output.path(),
        Some("password"),
        Some(selected_entry(&info, "d.txt")),
    )
    .unwrap();
    assert_eq!(
        fs::read(output.path().join("d.txt")).unwrap(),
        b"This is from d.txt"
    );
    assert!(!output.path().join("a.txt").exists());
    direct_drag::check(
        &info,
        &["d.txt"],
        &[("d.txt", b"This is from d.txt")],
        Some("password"),
    );
}

#[test]
fn rar5_direct_drag_preserves_password_and_solid_decoding() {
    for (file, password, entry, contents) in [
        (
            "test_read_format_rar5_compressed.rar",
            None,
            "test.bin",
            expected_data(1200, 0),
        ),
        (
            "test_read_format_rar5_multiple_files_solid.rar",
            None,
            "test4.bin",
            expected_data(4096, 4),
        ),
        (
            "test_read_format_rar5_encrypted_filenames.rar",
            Some("password"),
            "c.txt",
            b"This is from c.txt".to_vec(),
        ),
    ] {
        let info = inspect(&fixture(file), password).unwrap();
        direct_drag::check(&info, &[entry], &[(entry, &contents)], password);
        if password.is_some() {
            let output = TempDir::new().unwrap();
            assert!(
                hx6zip::archive::extract_drag_entries(
                    &info,
                    &[entry.to_owned()],
                    output.path(),
                    Some("wrong"),
                    false,
                    &JobControl::default(),
                )
                .is_err()
            );
            assert_eq!(fs::read_dir(output.path()).unwrap().count(), 0);
        }
    }
}

#[test]
fn multivolume_links_and_corrupt_rar5_are_rejected() {
    let multi = fixture("test_read_format_rar5_multiarchive.part01.rar");
    let error = inspect(&multi, None).unwrap_err();
    assert!(format!("{error:#}").contains("分卷"));

    let links = fixture("test_read_format_rar5_hardlink.rar");
    let info = inspect(&links, None).unwrap();
    assert!(info.entries.iter().any(|entry| entry.unsupported_kind));
    let output = TempDir::new().unwrap();
    assert!(extract(&info, output.path(), None, None).is_err());
    assert!(!output.path().join("hardlink.txt").exists());

    let mut broken = fs::read(fixture("test_read_format_rar5_compressed.rar")).unwrap();
    broken[200] ^= 0x7f;
    let path = output.path().join("broken.rar");
    fs::write(&path, broken).unwrap();
    if let Ok(info) = inspect(&path, None) {
        assert!(extract(&info, output.path(), None, None).is_err());
        assert!(!output.path().join("test.bin").exists());
    }
}
