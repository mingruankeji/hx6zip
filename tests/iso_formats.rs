use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use mzip::archive::{
    ArchiveKind, CancellationToken, Cancelled, ExtractOptions, IntegrityMode, JobControl,
    archive_selection, drag_conflicts, export_entries, extract_archive, extract_conflicts,
    extract_drag_entries, inspect_archive,
};
use tempfile::TempDir;

#[path = "support/direct_drag.rs"]
mod direct_drag;

const SECTOR: usize = 2048;
const ROOT: usize = 24;
const NESTED: usize = 25;

#[derive(Clone, Copy)]
enum Names {
    Plain,
    Joliet,
    RockRidge,
    RockRidgeLink,
}

fn both_u16(buffer: &mut [u8], offset: usize, value: u16) {
    buffer[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    buffer[offset + 2..offset + 4].copy_from_slice(&value.to_be_bytes());
}

fn both_u32(buffer: &mut [u8], offset: usize, value: u32) {
    buffer[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    buffer[offset + 4..offset + 8].copy_from_slice(&value.to_be_bytes());
}

fn record(name: &[u8], lba: u32, size: u32, flags: u8, system_use: &[u8]) -> Vec<u8> {
    let length = 33 + name.len() + usize::from(name.len() % 2 == 0) + system_use.len();
    let length = length + length % 2;
    let mut entry = vec![0; length];
    entry[0] = length as u8;
    both_u32(&mut entry, 2, lba);
    both_u32(&mut entry, 10, size);
    entry[18..25].copy_from_slice(&[124, 1, 2, 3, 4, 5, 0]);
    entry[25] = flags;
    both_u16(&mut entry, 28, 1);
    entry[32] = name.len() as u8;
    entry[33..33 + name.len()].copy_from_slice(name);
    let use_offset = 33 + name.len() + usize::from(name.len() % 2 == 0);
    entry[use_offset..use_offset + system_use.len()].copy_from_slice(system_use);
    entry
}

fn name_record(name: &str) -> Vec<u8> {
    let mut data = vec![b'N', b'M', (5 + name.len()) as u8, 1, 0];
    data.extend_from_slice(name.as_bytes());
    data
}

fn joliet(name: &str) -> Vec<u8> {
    name.encode_utf16().flat_map(u16::to_be_bytes).collect()
}

fn directory(image: &mut [u8], sector: usize, entries: &[Vec<u8>]) {
    let mut offset = sector * SECTOR;
    for entry in entries {
        image[offset..offset + entry.len()].copy_from_slice(entry);
        offset += entry.len();
    }
    assert!(offset <= (sector + 1) * SECTOR);
}

fn descriptor(image: &mut [u8], sector: usize, kind: u8, root: u32, table: u32) {
    let descriptor = &mut image[sector * SECTOR..(sector + 1) * SECTOR];
    descriptor[0] = kind;
    descriptor[1..7].copy_from_slice(b"CD001\x01");
    descriptor[8..72].fill(b' ');
    descriptor[40..49].copy_from_slice(b"MZIP_TEST");
    both_u32(descriptor, 80, 40);
    if kind == 2 {
        descriptor[88..91].copy_from_slice(b"%/E");
    }
    both_u16(descriptor, 120, 1);
    both_u16(descriptor, 124, 1);
    both_u16(descriptor, 128, SECTOR as u16);
    both_u32(descriptor, 132, 10);
    descriptor[140..144].copy_from_slice(&table.to_le_bytes());
    descriptor[148..152].copy_from_slice(&(table + 1).to_be_bytes());
    let root = record(&[0], root, SECTOR as u32, 2, &[]);
    descriptor[156..156 + root.len()].copy_from_slice(&root);
    descriptor[881] = 1;
}

fn path_table(image: &mut [u8], sector: usize, root: u32, big_endian: bool) {
    let table = &mut image[sector * SECTOR..(sector + 1) * SECTOR];
    table[0] = 1;
    table[2..6].copy_from_slice(&if big_endian {
        root.to_be_bytes()
    } else {
        root.to_le_bytes()
    });
    table[6..8].copy_from_slice(&if big_endian {
        1u16.to_be_bytes()
    } else {
        1u16.to_le_bytes()
    });
}

fn sample(names: Names) -> Vec<u8> {
    let mut image = vec![0; 40 * SECTOR];
    descriptor(&mut image, 16, 1, ROOT as u32, 20);
    path_table(&mut image, 20, ROOT as u32, false);
    path_table(&mut image, 21, ROOT as u32, true);
    let has_joliet = matches!(names, Names::Joliet);
    if has_joliet {
        descriptor(&mut image, 17, 2, 30, 22);
        path_table(&mut image, 22, 30, false);
        path_table(&mut image, 23, 30, true);
    }
    let terminator = if has_joliet { 18 } else { 17 };
    image[terminator * SECTOR] = 255;
    image[terminator * SECTOR + 1..terminator * SECTOR + 7].copy_from_slice(b"CD001\x01");

    let rock_ridge = matches!(names, Names::RockRidge | Names::RockRidgeLink);
    let sp = if rock_ridge {
        b"SP\x07\x01\xbe\xef\x00".as_slice()
    } else {
        &[]
    };
    let folder_name = if rock_ridge {
        name_record("资料")
    } else {
        Vec::new()
    };
    let mut readme_use = Vec::new();
    if rock_ridge {
        let alternate = name_record("说明.txt");
        let continuation = 32 * SECTOR;
        image[continuation..continuation + alternate.len()].copy_from_slice(&alternate);
        readme_use = vec![b'C', b'E', 28, 1];
        readme_use.resize(28, 0);
        both_u32(&mut readme_use, 4, 32);
        both_u32(&mut readme_use, 12, 0);
        both_u32(&mut readme_use, 20, alternate.len() as u32);
        if matches!(names, Names::RockRidgeLink) {
            readme_use.extend_from_slice(b"SL\x04\x01");
        }
    }
    directory(
        &mut image,
        ROOT,
        &[
            record(&[0], ROOT as u32, SECTOR as u32, 2, sp),
            record(&[1], ROOT as u32, SECTOR as u32, 2, &[]),
            record(b"FOLDER", NESTED as u32, SECTOR as u32, 2, &folder_name),
            record(b"README.TXT;1", 26, 5, 0, &readme_use),
            record(b"MULTI.BIN;1", 27, SECTOR as u32, 0x80, &[]),
            record(b"MULTI.BIN;1", 28, 3, 0, &[]),
        ],
    );
    directory(
        &mut image,
        NESTED,
        &[
            record(&[0], NESTED as u32, SECTOR as u32, 2, &[]),
            record(&[1], ROOT as u32, SECTOR as u32, 2, &[]),
            record(b"NESTED.TXT;1", 29, 4, 0, &[]),
        ],
    );
    if has_joliet {
        directory(
            &mut image,
            30,
            &[
                record(&[0], 30, SECTOR as u32, 2, &[]),
                record(&[1], 30, SECTOR as u32, 2, &[]),
                record(&joliet("资料"), 31, SECTOR as u32, 2, &[]),
                record(&joliet("说明.txt;1"), 26, 5, 0, &[]),
                record(&joliet("组合.bin;1"), 27, SECTOR as u32, 0x80, &[]),
                record(&joliet("组合.bin;1"), 28, 3, 0, &[]),
            ],
        );
        directory(
            &mut image,
            31,
            &[
                record(&[0], 31, SECTOR as u32, 2, &[]),
                record(&[1], 30, SECTOR as u32, 2, &[]),
                record(&joliet("内文.txt;1"), 29, 4, 0, &[]),
            ],
        );
    }
    image[27 * SECTOR..28 * SECTOR].fill(b'a');
    for (sector, contents) in [(26, b"alpha".as_slice()), (28, b"two"), (29, b"beta")] {
        image[sector * SECTOR..sector * SECTOR + contents.len()].copy_from_slice(contents);
    }
    image
}

fn save(temp: &TempDir, name: &str, image: &[u8]) -> PathBuf {
    let path = temp.path().join(name);
    fs::write(&path, image).unwrap();
    path
}

fn options(destination: &Path) -> ExtractOptions {
    ExtractOptions {
        destination: destination.to_owned(),
        password: None,
        selected: None,
        overwrite: false,
    }
}

fn find_record(image: &[u8], sector: usize, name: &[u8], occurrence: usize) -> usize {
    let mut offset = sector * SECTOR;
    let mut found = 0;
    while image[offset] != 0 {
        let length = usize::from(image[offset]);
        let name_length = usize::from(image[offset + 32]);
        if &image[offset + 33..offset + 33 + name_length] == name {
            if found == occurrence {
                return offset;
            }
            found += 1;
        }
        offset += length;
    }
    panic!("record not found");
}

#[test]
fn iso_variants_extract_multiextent_unicode_and_selected_directories() {
    let temp = TempDir::new().unwrap();
    for (label, names, folder, readme, multi, nested) in [
        (
            "plain",
            Names::Plain,
            "FOLDER",
            "README.TXT",
            "MULTI.BIN",
            "NESTED.TXT",
        ),
        (
            "joliet",
            Names::Joliet,
            "资料",
            "说明.txt",
            "组合.bin",
            "内文.txt",
        ),
        (
            "rock-ridge",
            Names::RockRidge,
            "资料",
            "说明.txt",
            "MULTI.BIN",
            "NESTED.TXT",
        ),
    ] {
        let path = save(&temp, &format!("{label}.iso"), &sample(names));
        let info = inspect_archive(&path, None, &JobControl::default()).unwrap();
        assert_eq!(info.format, ArchiveKind::Iso);
        assert_eq!(info.entries.len(), 4);
        assert_eq!(info.total_size, 2060);
        let multi_entry = info
            .entries
            .iter()
            .find(|entry| entry.name == multi)
            .unwrap();
        assert_eq!(multi_entry.size, 2051);
        let output = temp.path().join(format!("{label}-all"));
        let reports = Arc::new(Mutex::new(Vec::new()));
        let captured = reports.clone();
        let control = JobControl::new(CancellationToken::default(), move |progress| {
            captured.lock().unwrap().push(progress);
        });
        extract_archive(&info, &options(&output), &control).unwrap();
        assert_eq!(fs::read(output.join(readme)).unwrap(), b"alpha");
        let mut multi_contents = vec![b'a'; SECTOR];
        multi_contents.extend_from_slice(b"two");
        direct_drag::check(
            &info,
            &[&format!("{folder}/{nested}"), multi],
            &[(nested, b"beta"), (multi, &multi_contents)],
            None,
        );
        assert_eq!(fs::read(output.join(multi)).unwrap(), multi_contents);
        assert_eq!(fs::read(output.join(folder).join(nested)).unwrap(), b"beta");
        let reports = reports.lock().unwrap();
        // Safe-mode verification and publication have separate progress counters.
        let extraction_reports = reports
            .iter()
            .filter(|progress| progress.operation == "正在解压")
            .collect::<Vec<_>>();
        assert!(
            extraction_reports
                .windows(2)
                .all(|pair| pair[0].completed_bytes <= pair[1].completed_bytes)
        );
        assert_eq!(extraction_reports.last().unwrap().completed_bytes, 2060);

        let selected = info
            .entries
            .iter()
            .find(|entry| entry.name == folder)
            .unwrap()
            .index;
        let output = temp.path().join(format!("{label}-selected"));
        let mut selected_options = options(&output);
        selected_options.selected = Some(BTreeSet::from([selected]));
        extract_archive(&info, &selected_options, &JobControl::default()).unwrap();
        assert_eq!(fs::read(output.join(folder).join(nested)).unwrap(), b"beta");
        assert!(!output.join(readme).exists());
        assert!(!output.join(multi).exists());
    }
}

#[test]
fn iso_rejects_bad_extents_loops_endianness_and_incomplete_multiextents() {
    let temp = TempDir::new().unwrap();
    let valid = sample(Names::Plain);
    let folder = find_record(&valid, ROOT, b"FOLDER", 0);
    let readme = find_record(&valid, ROOT, b"README.TXT;1", 0);
    let last_multi = find_record(&valid, ROOT, b"MULTI.BIN;1", 1);
    let mut cases = Vec::new();
    let mut image = valid.clone();
    both_u32(&mut image, readme + 2, 40);
    cases.push(("outside-image", image));
    let mut image = valid.clone();
    both_u32(&mut image, 16 * SECTOR + 80, 39);
    both_u32(&mut image, readme + 2, 39);
    cases.push(("outside-volume", image));
    let mut image = valid.clone();
    both_u32(&mut image, folder + 2, ROOT as u32);
    cases.push(("directory-cycle", image));
    let mut image = valid.clone();
    image[readme + 6] ^= 1;
    cases.push(("mismatched-endian", image));
    let mut image = valid.clone();
    image[last_multi + 25] |= 0x80;
    cases.push(("missing-final-extent", image));
    let mut image = valid;
    image[ROOT * SECTOR + 33] = 1;
    cases.push(("invalid-root-dot", image));
    for (name, image) in cases {
        let path = save(&temp, &format!("{name}.iso"), &image);
        assert!(
            inspect_archive(&path, None, &JobControl::default()).is_err(),
            "{name}"
        );
    }
}

#[test]
fn iso_skips_symbolic_links_and_reports_only_selected_skips() {
    let temp = TempDir::new().unwrap();
    let link = save(&temp, "link.iso", &sample(Names::RockRidgeLink));
    let info = inspect_archive(&link, None, &JobControl::default()).unwrap();
    assert!(
        info.entries.iter().any(|entry| entry.name == "说明.txt"
            && entry.unsupported_kind
            && entry.is_symbolic_link)
    );
    for mode in [IntegrityMode::Safe, IntegrityMode::Normal] {
        let output = temp.path().join(format!("link-output-{mode:?}"));
        let control = JobControl::default().with_integrity(mode);
        assert!(
            extract_conflicts(&info, &options(&output), &control)
                .unwrap()
                .is_empty()
        );
        extract_archive(&info, &options(&output), &control).unwrap();
        assert!(!output.join("说明.txt").exists());
        assert_eq!(
            fs::read(output.join("资料").join("NESTED.TXT")).unwrap(),
            b"beta"
        );
        assert_eq!(control.warnings(), vec!["已跳过 ISO 符号链接：说明.txt"]);

        let selected = archive_selection(&info, &["资料".to_owned()]).unwrap();
        let mut selected_options = options(&temp.path().join(format!("selected-{mode:?}")));
        selected_options.selected = Some(selected);
        let control = JobControl::default().with_integrity(mode);
        extract_archive(&info, &selected_options, &control).unwrap();
        assert!(control.warnings().is_empty());

        selected_options.destination = temp.path().join(format!("link-only-{mode:?}"));
        selected_options.selected =
            Some(archive_selection(&info, &["说明.txt".to_owned()]).unwrap());
        let error = extract_archive(&info, &selected_options, &control).unwrap_err();
        assert!(error.to_string().contains("没有可解压的条目"));
        assert!(!selected_options.destination.exists());
    }

    let paths = vec!["资料".to_owned(), "说明.txt".to_owned()];
    let output = temp.path().join("drag-output");
    let control = JobControl::default();
    assert!(
        drag_conflicts(&info, &paths, &output, &control)
            .unwrap()
            .is_empty()
    );
    extract_drag_entries(&info, &paths, &output, None, false, &control).unwrap();
    assert_eq!(
        fs::read(output.join("资料").join("NESTED.TXT")).unwrap(),
        b"beta"
    );
    assert!(!output.join("说明.txt").exists());
    assert_eq!(control.warnings().len(), 1);
    let exported = export_entries(&info, &paths, None, &JobControl::default()).unwrap();
    assert_eq!(exported.paths, vec![exported.directory.path().join("资料")]);
}

#[test]
fn iso_selected_directory_skips_nested_links_without_following_them() {
    let temp = TempDir::new().unwrap();
    let mut image = sample(Names::RockRidgeLink);
    directory(
        &mut image,
        NESTED,
        &[
            record(&[0], NESTED as u32, SECTOR as u32, 2, &[]),
            record(&[1], ROOT as u32, SECTOR as u32, 2, &[]),
            record(b"NESTED.TXT;1", 29, 4, 0, &[]),
            record(b"ALIAS;1", 0, 0, 0, b"SL\x04\x01"),
        ],
    );
    let path = save(&temp, "nested-link.iso", &image);
    let info = inspect_archive(&path, None, &JobControl::default()).unwrap();
    let selected = info
        .entries
        .iter()
        .find(|entry| entry.name == "资料")
        .unwrap()
        .index;
    for mode in [IntegrityMode::Safe, IntegrityMode::Normal] {
        let output = temp.path().join(format!("nested-{mode:?}"));
        let mut options = options(&output);
        options.selected = Some(BTreeSet::from([selected]));
        let control = JobControl::default().with_integrity(mode);
        extract_archive(&info, &options, &control).unwrap();
        assert_eq!(
            fs::read(output.join("资料").join("NESTED.TXT")).unwrap(),
            b"beta"
        );
        assert!(!output.join("资料").join("ALIAS").exists());
        assert_eq!(control.warnings(), vec!["已跳过 ISO 符号链接：资料/ALIAS"]);
    }
}

#[test]
fn iso_rejects_special_entries_and_unsafe_names_before_writing() {
    let temp = TempDir::new().unwrap();
    let mut image = sample(Names::RockRidge);
    let readme = find_record(&image, ROOT, b"README.TXT;1", 0);
    let mut px = vec![b'P', b'X', 36, 1];
    px.resize(36, 0);
    both_u32(&mut px, 4, 0x1000);
    both_u32(&mut px, 12, 1);
    let mut file_use = name_record("说明.txt");
    file_use.extend_from_slice(&px);
    image[32 * SECTOR..32 * SECTOR + file_use.len()].copy_from_slice(&file_use);
    let use_offset = readme + 33 + b"README.TXT;1".len() + 1;
    both_u32(&mut image, use_offset + 20, file_use.len() as u32);
    let path = save(&temp, "special.iso", &image);
    let info = inspect_archive(&path, None, &JobControl::default()).unwrap();
    let output = temp.path().join("special-output");
    assert!(extract_archive(&info, &options(&output), &JobControl::default()).is_err());
    assert!(!output.exists());

    let mut image = sample(Names::Plain);
    let folder = find_record(&image, ROOT, b"FOLDER", 0);
    image[folder + 33..folder + 39].copy_from_slice(b"../BAD");
    let path = save(&temp, "unsafe.iso", &image);
    assert!(inspect_archive(&path, None, &JobControl::default()).is_err());
}

#[test]
fn iso_cancellation_preserves_existing_files_and_cleans_temporary_files() {
    let temp = TempDir::new().unwrap();
    let path = save(&temp, "cancel.iso", &sample(Names::Plain));
    let info = inspect_archive(&path, None, &JobControl::default()).unwrap();
    let output = temp.path().join("output");
    fs::create_dir(&output).unwrap();
    fs::write(output.join("README.TXT"), b"keep").unwrap();
    let token = CancellationToken::default();
    let trigger = token.clone();
    let control = JobControl::new(token, move |_| trigger.cancel());
    let mut extract_options = options(&output);
    extract_options.overwrite = true;
    let error = extract_archive(&info, &extract_options, &control).unwrap_err();
    assert!(error.downcast_ref::<Cancelled>().is_some(), "{error:#}");
    assert_eq!(fs::read(output.join("README.TXT")).unwrap(), b"keep");
    assert!(fs::read_dir(&output).unwrap().all(|entry| {
        let name = entry.unwrap().file_name();
        name == "README.TXT" || name == "FOLDER"
    }));
}

#[test]
#[ignore = "requires an external ISO specified by MZIP_TEST_EXTERNAL_ISO"]
fn external_iso_can_be_extracted() {
    let path = PathBuf::from(std::env::var_os("MZIP_TEST_EXTERNAL_ISO").unwrap());
    let info = inspect_archive(&path, None, &JobControl::default()).unwrap();
    for mode in [IntegrityMode::Safe, IntegrityMode::Normal] {
        let temp = TempDir::new().unwrap();
        let control = JobControl::default().with_integrity(mode);
        extract_archive(&info, &options(temp.path()), &control).unwrap();
        let mut skipped = Vec::new();
        for entry in &info.entries {
            let path = temp.path().join(entry.name.split('/').collect::<PathBuf>());
            if info.skips_iso_symlink(entry) {
                assert!(!path.exists(), "{}", entry.name);
                skipped.push(format!("已跳过 ISO 符号链接：{}", entry.name));
            } else {
                let metadata = fs::symlink_metadata(&path).unwrap();
                assert_eq!(metadata.is_dir(), entry.is_directory, "{}", entry.name);
                if !entry.is_directory {
                    assert!(metadata.is_file(), "{}", entry.name);
                    assert_eq!(metadata.len(), entry.size, "{}", entry.name);
                }
            }
        }
        assert_eq!(control.warnings(), skipped);
        eprintln!(
            "{mode:?}: verified {} entries, {} bytes",
            info.entries.len(),
            info.total_size
        );
    }
}

#[test]
#[ignore = "requires an externally generated ISO specified by MZIP_TEST_EXTERNAL_ISO"]
fn external_joliet_iso_can_be_extracted() {
    let path = PathBuf::from(std::env::var_os("MZIP_TEST_EXTERNAL_ISO").unwrap());
    let info = inspect_archive(&path, None, &JobControl::default()).unwrap();
    assert!(
        info.entries
            .iter()
            .any(|entry| entry.name == "资料/hello.txt")
    );
    let temp = TempDir::new().unwrap();
    extract_archive(&info, &options(temp.path()), &JobControl::default()).unwrap();
    assert_eq!(
        fs::read(temp.path().join("资料").join("hello.txt")).unwrap(),
        b"ISO sample"
    );
}
