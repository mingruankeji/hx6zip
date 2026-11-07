use std::{
    fs,
    path::{Path, PathBuf},
};

use hx6zip::archive::{
    ArchiveInfo, CancellationToken, ExtractOptions, IntegrityMode, JobControl, archive_selection,
    drag_conflicts, extract_archive, extract_conflicts, extract_drag_entries,
};

fn files(directory: &Path) -> Vec<PathBuf> {
    fs::read_dir(directory)
        .unwrap()
        .flat_map(|entry| {
            let entry = entry.unwrap();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                files(&entry.path())
            } else {
                assert!(kind.is_file());
                vec![entry.path()]
            }
        })
        .collect()
}

fn check_regular(
    info: &ArchiveInfo,
    paths: &[String],
    expected: &[(&str, &[u8])],
    password: Option<&str>,
    control: &JobControl,
) {
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("output");
    let selected = archive_selection(info, paths).unwrap();
    let mut options = ExtractOptions {
        destination: output.clone(),
        password: password.map(str::to_owned),
        selected: Some(selected.clone()),
        overwrite: false,
    };
    assert!(
        extract_conflicts(info, &options, control)
            .unwrap()
            .is_empty()
    );
    assert!(!output.exists());
    let entries = info
        .entries
        .iter()
        .filter(|entry| selected.contains(&entry.index) && !entry.is_directory)
        .collect::<Vec<_>>();
    assert_eq!(entries.len(), expected.len());
    let targets = entries
        .iter()
        .map(|entry| output.join(entry.name.split(['/', '\\']).collect::<PathBuf>()))
        .collect::<Vec<_>>();
    for target in &targets {
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(target, b"old content").unwrap();
    }
    let sentinel = output.join("unrelated-sentinel");
    fs::write(&sentinel, b"keep").unwrap();
    let mut conflicts = extract_conflicts(info, &options, control).unwrap();
    conflicts.sort();
    let mut expected_conflicts = targets.clone();
    expected_conflicts.sort();
    assert_eq!(conflicts, expected_conflicts);
    for target in &targets {
        assert_eq!(fs::read(target).unwrap(), b"old content");
    }
    options.overwrite = true;
    extract_archive(info, &options, control).unwrap();
    for (entry, target) in entries.iter().zip(&targets) {
        let name = entry.name.split(['/', '\\']).next_back().unwrap();
        let matches = expected
            .iter()
            .filter(|(path, _)| path.split('/').next_back() == Some(name))
            .collect::<Vec<_>>();
        assert_eq!(matches.len(), 1);
        assert_eq!(fs::read(target).unwrap(), matches[0].1);
    }
    let mut actual_files = files(&output);
    actual_files.sort();
    expected_conflicts.push(sentinel.clone());
    expected_conflicts.sort();
    assert_eq!(actual_files, expected_conflicts);
    assert_eq!(fs::read(sentinel).unwrap(), b"keep");
}

pub fn check(
    info: &ArchiveInfo,
    paths: &[&str],
    expected: &[(&str, &[u8])],
    password: Option<&str>,
) {
    let source = fs::read(&info.path).unwrap();
    let paths = paths
        .iter()
        .map(|path| (*path).to_owned())
        .collect::<Vec<_>>();
    for mode in [IntegrityMode::Normal, IntegrityMode::Safe] {
        let output = tempfile::tempdir().unwrap();
        let control = JobControl::default().with_integrity(mode);
        check_regular(info, &paths, expected, password, &control);
        let targets = expected
            .iter()
            .map(|(name, _)| output.path().join(name.split('/').collect::<PathBuf>()))
            .collect::<Vec<_>>();
        let sentinel = output.path().join("unrelated-sentinel");
        fs::write(&sentinel, b"keep").unwrap();
        assert!(
            drag_conflicts(info, &paths, output.path(), &control)
                .unwrap()
                .is_empty()
        );
        extract_drag_entries(info, &paths, output.path(), password, false, &control).unwrap();
        let mut actual_files = files(output.path());
        actual_files.sort();
        let mut expected_files = targets.clone();
        expected_files.push(sentinel.clone());
        expected_files.sort();
        assert_eq!(actual_files, expected_files);
        for (target, (_, contents)) in targets.iter().zip(expected) {
            assert_eq!(fs::read(target).unwrap(), *contents);
            fs::write(target, b"old content").unwrap();
        }
        let mut conflicts = drag_conflicts(info, &paths, output.path(), &control).unwrap();
        conflicts.sort();
        let mut expected_conflicts = targets.clone();
        expected_conflicts.sort();
        assert_eq!(conflicts, expected_conflicts);
        assert!(
            extract_drag_entries(info, &paths, output.path(), password, false, &control).is_err()
        );
        for target in &targets {
            assert_eq!(fs::read(target).unwrap(), b"old content");
        }
        extract_drag_entries(info, &paths, output.path(), password, true, &control).unwrap();
        for (target, (_, contents)) in targets.iter().zip(expected) {
            assert_eq!(fs::read(target).unwrap(), *contents);
        }
        if mode == IntegrityMode::Safe {
            for target in &targets {
                fs::write(target, b"old content").unwrap();
            }
            let token = CancellationToken::default();
            let cancel = token.clone();
            let multiple_files = expected.len() > 1;
            let control = JobControl::new(token, move |progress| {
                if (multiple_files
                    && progress.operation == "正在提交解压结果"
                    && progress.completed_bytes > 0)
                    || (!multiple_files && progress.operation == "正在解压")
                {
                    cancel.cancel();
                }
            })
            .with_integrity(mode);
            assert!(
                extract_drag_entries(info, &paths, output.path(), password, true, &control)
                    .is_err()
            );
            assert!(control.is_cancelled());
            for target in &targets {
                assert_eq!(fs::read(target).unwrap(), b"old content");
            }
        }
        assert_eq!(fs::read(sentinel).unwrap(), b"keep");
        assert_eq!(fs::read(&info.path).unwrap(), source);
    }
}
