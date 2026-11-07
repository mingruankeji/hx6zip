use std::{
    fs,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use hx6zip::archive::{CancellationToken, Cancelled, JobControl, merge_files, split_file};
use hx6zip_shell_protocol::{SPLIT_SIZES_MB, can_merge, can_split, merge_parts};
use tempfile::TempDir;

#[test]
fn split_and_merge_round_trip_in_numeric_not_selection_order() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("资料.bin");
    let payload: Vec<_> = (0..=255).cycle().take(10003).collect();
    fs::write(&source, &payload).unwrap();
    let output = temp.path().join("parts");
    let snapshots = Arc::new(Mutex::new(Vec::new()));
    let reported = snapshots.clone();
    let control = JobControl::new(CancellationToken::default(), move |progress| {
        reported.lock().unwrap().push(progress);
    });
    let mut parts = split_file(&source, &output, 1000, &control).unwrap();
    assert_eq!(parts.len(), 11);
    assert!(parts[0].ends_with("资料.bin.001"));
    for (index, path) in parts.iter().enumerate() {
        assert_eq!(
            fs::read(path).unwrap(),
            payload[index * 1000..((index + 1) * 1000).min(payload.len())]
        );
    }
    let last = snapshots.lock().unwrap().last().unwrap().clone();
    assert_eq!(last.completed_bytes, payload.len() as u64);
    assert_eq!(last.total_bytes, payload.len() as u64);
    assert_eq!(fs::read(&source).unwrap(), payload);
    assert!(can_split(&[source]));
    assert!(can_merge(&parts));
    parts.reverse();
    let result = merge_files(&parts, &control).unwrap();
    assert_eq!(result, fs::canonicalize(&output).unwrap().join("资料.bin"));
    assert_eq!(fs::read(result).unwrap(), payload);
    assert_eq!(fs::read_dir(output).unwrap().count(), 12);
}

#[test]
fn split_handles_exact_multiple_small_and_empty_files() {
    for (length, volume, expected) in [(12, 4, vec![4, 4, 4]), (3, 10, vec![3]), (0, 10, vec![0])] {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("file");
        fs::write(&source, vec![7; length]).unwrap();
        let parts = split_file(&source, temp.path(), volume, &JobControl::default()).unwrap();
        let lengths: Vec<_> = parts
            .iter()
            .map(|path| fs::metadata(path).unwrap().len())
            .collect();
        assert_eq!(lengths, expected);
    }
}

#[test]
fn split_rejects_zero_directory_and_conflicting_or_stale_parts() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("file");
    fs::write(&source, b"abcdef").unwrap();
    assert!(split_file(&source, temp.path(), 0, &JobControl::default()).is_err());
    assert!(split_file(temp.path(), temp.path(), 2, &JobControl::default()).is_err());
    for suffix in ["001", "002", "1000"] {
        let existing = temp.path().join(format!("file.{suffix}"));
        fs::write(&existing, b"old").unwrap();
        assert!(split_file(&source, temp.path(), 2, &JobControl::default()).is_err());
        assert_eq!(fs::read(&existing).unwrap(), b"old");
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 2);
        fs::remove_file(existing).unwrap();
    }
}

#[test]
fn merge_rejects_missing_duplicate_mixed_or_noncanonical_numbers() {
    let paths = |names: &[&str]| names.iter().map(PathBuf::from).collect::<Vec<_>>();
    for names in [
        vec!["file.001"],
        vec!["file.002", "file.003"],
        vec!["file.001", "file.003"],
        vec!["file.001", "file.001"],
        vec!["file.001", "other.002"],
        vec![r"a\file.001", r"b\file.002"],
        vec!["file.01", "file.002"],
        vec!["file.0001", "file.002"],
        vec!["file.000", "file.001"],
        vec!["file.001", "file.txt"],
    ] {
        assert!(merge_parts(&paths(&names)).is_none(), "{names:?}");
        assert!(merge_files(&paths(&names), &JobControl::default()).is_err());
    }
    let mut large = (1..=1001)
        .map(|i| PathBuf::from(format!("file.{i:03}")))
        .collect::<Vec<_>>();
    large.reverse();
    let (_, sorted) = merge_parts(&large).unwrap();
    assert_eq!(sorted[998], PathBuf::from("file.999"));
    assert_eq!(sorted[999], PathBuf::from("file.1000"));
}

#[test]
fn merge_does_not_overwrite_original_and_rejects_missing_or_directory_parts() {
    let temp = TempDir::new().unwrap();
    let first = temp.path().join("file.001");
    let second = temp.path().join("file.002");
    let original = temp.path().join("file");
    fs::write(&first, b"abc").unwrap();
    fs::write(&second, b"def").unwrap();
    let parts = vec![first.clone(), second.clone()];
    fs::write(&original, b"original").unwrap();
    assert!(merge_files(&parts, &JobControl::default()).is_err());
    assert_eq!(fs::read(&original).unwrap(), b"original");
    fs::remove_file(&original).unwrap();
    fs::remove_file(&second).unwrap();
    assert!(!can_merge(&parts));
    assert!(merge_files(&parts, &JobControl::default()).is_err());
    fs::create_dir(&second).unwrap();
    assert!(!can_merge(&parts));
    assert!(!can_split(&[second]));
    assert!(merge_files(&parts, &JobControl::default()).is_err());
    assert!(!original.exists());
}

#[test]
fn cancelling_split_keeps_completed_parts_and_cleans_current_temp() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("file");
    let output = temp.path().join("output");
    fs::write(&source, b"abcdef").unwrap();
    let token = CancellationToken::default();
    let cancel = token.clone();
    let control = JobControl::new(token, move |progress| {
        if progress.current_file == "file.002" {
            cancel.cancel();
        }
    });
    let error = split_file(&source, &output, 2, &control).unwrap_err();
    assert!(error.downcast_ref::<Cancelled>().is_some());
    assert_eq!(fs::read(output.join("file.001")).unwrap(), b"ab");
    assert_eq!(fs::read_dir(output).unwrap().count(), 1);
}

#[test]
fn cancelling_merge_never_leaves_partial_output() {
    let temp = TempDir::new().unwrap();
    let parts = vec![temp.path().join("file.001"), temp.path().join("file.002")];
    for path in &parts {
        fs::write(path, b"abc").unwrap();
    }
    let token = CancellationToken::default();
    let cancel = token.clone();
    let control = JobControl::new(token, move |progress| {
        if progress.current_file == "file.002" {
            cancel.cancel();
        }
    });
    let error = merge_files(&parts, &control).unwrap_err();
    assert!(error.downcast_ref::<Cancelled>().is_some());
    assert!(!temp.path().join("file").exists());
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 2);
}

#[test]
fn changed_input_is_detected_before_committing_output() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("file");
    fs::write(&source, b"abcdef").unwrap();
    let changed = source.clone();
    let control = JobControl::new(CancellationToken::default(), move |_| {
        fs::write(&changed, b"changed size").unwrap();
    });
    assert!(split_file(&source, &temp.path().join("output"), 2, &control).is_err());
    assert_eq!(fs::read_dir(temp.path().join("output")).unwrap().count(), 0);
}

#[test]
fn presets_use_exact_requested_sizes_and_mb_units() {
    assert_eq!(
        SPLIT_SIZES_MB,
        &[
            ("10M", 10),
            ("100M", 100),
            ("1000M", 1000),
            ("650M-CD", 650),
            ("700M-CD", 700),
            ("4092M-FAT", 4092),
            ("4480M-DVD", 4480),
            ("8128M-DVD DL", 8128),
            ("23040M-BD", 23040),
        ]
    );
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("file");
    fs::write(&source, vec![42; 10 * 1024 * 1024 + 1]).unwrap();
    let parts = split_file(
        &source,
        temp.path(),
        SPLIT_SIZES_MB[0].1 * 1024 * 1024,
        &JobControl::default(),
    )
    .unwrap();
    assert_eq!(fs::metadata(&parts[0]).unwrap().len(), 10 * 1024 * 1024);
    assert_eq!(fs::metadata(&parts[1]).unwrap().len(), 1);
}

#[test]
fn cli_parses_split_and_merge() {
    use hx6zip::cli::{StartupAction, startup_action};
    assert_eq!(
        startup_action(["--split-file", "file"].map(Into::into).into_iter()),
        Some(StartupAction::Split {
            path: "file".into()
        })
    );
    assert_eq!(
        startup_action(
            ["--merge-files", "file.002", "file.001"]
                .map(Into::into)
                .into_iter()
        ),
        Some(StartupAction::Merge {
            sources: vec!["file.002".into(), "file.001".into()]
        })
    );
    assert_eq!(startup_action(["--split-file".into()].into_iter()), None);
}
