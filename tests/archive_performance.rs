use std::{
    ffi::OsString,
    fs::{self, File},
    io::{self, BufReader},
    path::Path,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use hx6zip::archive::{
    ArchiveFormat, CancellationToken, CreateOptions, ExtractOptions, IntegrityMode, JobControl,
    create_archive, export_entries, extract_archive, inspect_archive,
};
use tempfile::TempDir;

fn data(size: usize, mut state: u64) -> Vec<u8> {
    let text = b"archive benchmark: structured text with some changing bytes\n";
    (0..size)
        .map(|index| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            if index % 8 < 2 {
                state as u8
            } else {
                text[index % text.len()]
            }
        })
        .collect()
}

fn run_workload(root: &Path, name: &str, file_count: usize, file_size: usize) {
    let source = root.join(name);
    fs::create_dir(&source).unwrap();
    for index in 0..file_count {
        fs::write(
            source.join(format!("{index:05}.bin")),
            data(file_size, index as u64 + 1),
        )
        .unwrap();
    }

    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        let archive = root.join(format!("{name}.{}", format.extension()));
        let output = root.join(format!("{name}-{}", format.extension()));
        let reports = Arc::new(AtomicUsize::new(0));
        let report_count = reports.clone();
        let control = JobControl::new(CancellationToken::default(), move |_| {
            report_count.fetch_add(1, Ordering::Relaxed);
        });

        let started = Instant::now();
        create_archive(
            &CreateOptions {
                sources: vec![source.clone()],
                destination: archive.clone(),
                format,
                password: None,
                overwrite: false,
            },
            &control,
        )
        .unwrap();
        let create = started.elapsed();
        let started = Instant::now();
        let info = inspect_archive(&archive, None, &control).unwrap();
        let inspect = started.elapsed();
        let started = Instant::now();
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
        let extract = started.elapsed();
        for index in 0..file_count {
            let filename = format!("{index:05}.bin");
            assert_eq!(
                fs::read(output.join(name).join(&filename)).unwrap(),
                fs::read(source.join(&filename)).unwrap()
            );
        }
        assert_eq!(info.entries.len(), file_count + 1);
        println!(
            "{name:12} {format:?}: create={:.3}s inspect={:.3}s extract={:.3}s archive={} bytes reports={}",
            create.as_secs_f64(),
            inspect.as_secs_f64(),
            extract.as_secs_f64(),
            fs::metadata(archive).unwrap().len(),
            reports.load(Ordering::Relaxed),
        );
    }
}

#[test]
#[ignore = "synthetic release-mode performance benchmark"]
fn benchmark_archive_workloads() {
    let root = TempDir::new().unwrap();
    run_workload(root.path(), "small-files", 500, 1024);
    run_workload(root.path(), "medium-files", 8, 2 * 1024 * 1024);
    run_workload(root.path(), "large-file", 1, 32 * 1024 * 1024);
}

#[test]
#[ignore = "synthetic release-mode 7z extraction and Explorer drag-export benchmark"]
fn benchmark_sevenzip_extraction_and_drag_export() {
    let root = TempDir::new().unwrap();
    for (name, file_count, file_size) in [
        ("small-files", 500, 1024),
        ("medium-files", 8, 2 * 1024 * 1024),
        ("large-file", 1, 32 * 1024 * 1024),
    ] {
        let source = root.path().join(name);
        fs::create_dir(&source).unwrap();
        for index in 0..file_count {
            fs::write(
                source.join(format!("{index:05}.bin")),
                data(file_size, index as u64 + 1),
            )
            .unwrap();
        }

        let archive = root.path().join(format!("{name}.7z"));
        let control = JobControl::default();
        create_archive(
            &CreateOptions {
                sources: vec![source.clone()],
                destination: archive.clone(),
                format: ArchiveFormat::SevenZip,
                password: None,
                overwrite: false,
            },
            &control,
        )
        .unwrap();
        let info = inspect_archive(&archive, None, &control).unwrap();
        let mut normal = Vec::new();
        let mut safe = Vec::new();
        let mut drag = Vec::new();

        for round in 0..3 {
            for mode in [IntegrityMode::Normal, IntegrityMode::Safe] {
                let destination = root.path().join(format!("{name}-{}-{round}", mode.label()));
                let started = Instant::now();
                extract_archive(
                    &info,
                    &ExtractOptions {
                        destination: destination.clone(),
                        password: None,
                        selected: None,
                        overwrite: false,
                    },
                    &JobControl::default().with_integrity(mode),
                )
                .unwrap();
                let elapsed = started.elapsed();
                for index in 0..file_count {
                    let filename = format!("{index:05}.bin");
                    assert_eq!(
                        fs::read(destination.join(name).join(&filename)).unwrap(),
                        fs::read(source.join(&filename)).unwrap()
                    );
                }
                match mode {
                    IntegrityMode::Normal => normal.push(elapsed),
                    IntegrityMode::Safe => safe.push(elapsed),
                }
            }

            let started = Instant::now();
            let exported = export_entries(&info, &[name.into()], None, &control).unwrap();
            drag.push(started.elapsed());
            assert_eq!(exported.paths.len(), 1);
            for index in 0..file_count {
                let filename = format!("{index:05}.bin");
                assert_eq!(
                    fs::read(exported.directory.path().join(name).join(&filename)).unwrap(),
                    fs::read(source.join(&filename)).unwrap()
                );
            }
        }

        println!(
            "{name:12} SevenZip median of 3: normal={:.3}s safe={:.3}s Explorer-drag export={:.3}s",
            median_seconds(normal),
            median_seconds(safe),
            median_seconds(drag),
        );
    }
}

#[test]
#[ignore = "compresses the existing directory selected by HX6ZIP_BENCH_SOURCE"]
fn benchmark_zip_directory() {
    let source = fs::canonicalize(
        std::env::var_os("HX6ZIP_BENCH_SOURCE")
            .expect("set HX6ZIP_BENCH_SOURCE to an input directory"),
    )
    .unwrap();
    assert!(source.is_dir());
    let output_parent = std::env::var_os("HX6ZIP_BENCH_OUTPUT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let output_parent = fs::canonicalize(output_parent).unwrap();
    assert!(!output_parent.starts_with(&source));
    let root = TempDir::new_in(output_parent).unwrap();
    let sources = fs::read_dir(&source)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    let archive = root.path().join("hx6zip.zip");
    let started = Instant::now();
    let last_report = Arc::new(std::sync::Mutex::new(started));
    let first_report = std::sync::Once::new();
    let control = JobControl::new(CancellationToken::default(), move |progress| {
        first_report.call_once(|| {
            println!(
                "Hx6Zip scan finished after {:.1}s",
                started.elapsed().as_secs_f64()
            );
        });
        let mut last = last_report.lock().unwrap();
        if last.elapsed() >= Duration::from_secs(10) {
            println!(
                "Hx6Zip elapsed={:.1}s input={}/{} output={:?}",
                started.elapsed().as_secs_f64(),
                progress.completed_bytes,
                progress.total_bytes,
                progress.archive_size,
            );
            *last = Instant::now();
        }
    });
    create_archive(
        &CreateOptions {
            sources: sources.clone(),
            destination: archive.clone(),
            format: ArchiveFormat::Zip,
            password: None,
            overwrite: false,
        },
        &control,
    )
    .unwrap();
    println!(
        "Hx6Zip directory: create={:.3}s archive={} bytes",
        started.elapsed().as_secs_f64(),
        fs::metadata(&archive).unwrap().len(),
    );
    let mut zip = zip::ZipArchive::new(BufReader::new(File::open(&archive).unwrap())).unwrap();
    let mut bytes = 0;
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index).unwrap();
        bytes += io::copy(&mut entry, &mut io::sink()).unwrap();
    }
    println!(
        "Hx6Zip verified: {} entries, {bytes} bytes, all CRCs checked",
        zip.len()
    );
    drop(zip);
    fs::remove_file(&archive).unwrap();

    if let Some(seven_zip) = std::env::var_os("HX6ZIP_BENCH_7Z") {
        let external_archive = root.path().join("7zip.zip");
        let started = Instant::now();
        run_7zip(
            Command::new(&seven_zip)
                .current_dir(&source)
                .args(["a", "-tzip", "-mm=Deflate", "-mx=6", "-y", "-bd"])
                .arg(seven_zip_threads())
                .arg(&external_archive)
                .args(sources.iter().map(|path| path.file_name().unwrap())),
        );
        println!(
            "7-Zip directory: create={:.3}s archive={} bytes",
            started.elapsed().as_secs_f64(),
            fs::metadata(&external_archive).unwrap().len(),
        );
        run_7zip(
            Command::new(seven_zip)
                .args(["t", "-bd"])
                .arg(&external_archive),
        );
    }
}

/// Hx6Zip's ZIP compression uses every logical processor; give 7-Zip the same.
fn seven_zip_threads() -> String {
    let threads = std::thread::available_parallelism().map_or(1, |count| count.get());
    format!("-mmt={threads}")
}

fn run_7zip(command: &mut Command) {
    let output = command
        .output()
        .expect("无法启动 7-Zip；请设置 HX6ZIP_BENCH_7Z 为 7z.exe 的完整路径");
    assert!(
        output.status.success(),
        "7-Zip 失败：{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn median_seconds(mut values: Vec<Duration>) -> f64 {
    values.sort_unstable();
    values[values.len() / 2].as_secs_f64()
}

fn benchmark_zip_extractor(
    root: &Path,
    ferrozip: &std::ffi::OsStr,
    name: &str,
    file_count: usize,
    file_size: usize,
) {
    let source = root.join(name);
    fs::create_dir(&source).unwrap();
    for index in 0..file_count {
        fs::write(
            source.join(format!("{index:05}.bin")),
            data(file_size, index as u64 + 1),
        )
        .unwrap();
    }

    let archive = root.join(format!("{name}.zip"));
    let control = JobControl::default();
    create_archive(
        &CreateOptions {
            sources: vec![source.clone()],
            destination: archive.clone(),
            format: ArchiveFormat::Zip,
            password: None,
            overwrite: false,
        },
        &control,
    )
    .unwrap();
    let info = inspect_archive(&archive, None, &control).unwrap();
    let threads = std::thread::available_parallelism()
        .map_or(1, |count| count.get())
        .min(8)
        .to_string();
    let mut ferrozip_extract = Vec::new();
    let mut hx6zip_normal_extract = Vec::new();
    let mut hx6zip_safe_extract = Vec::new();
    let mut outputs = Vec::new();

    for round in 0..5 {
        match round % 3 {
            0 => {
                let (elapsed, output) = run_hx6zip_extract(
                    root,
                    &info,
                    name,
                    round,
                    IntegrityMode::Normal,
                    "hx-normal",
                );
                hx6zip_normal_extract.push(elapsed);
                outputs.push(output);
                let (elapsed, output) =
                    run_ferrozip_extract(ferrozip, &threads, &archive, root, name, round);
                ferrozip_extract.push(elapsed);
                outputs.push(output);
                let (elapsed, output) =
                    run_hx6zip_extract(root, &info, name, round, IntegrityMode::Safe, "hx-safe");
                hx6zip_safe_extract.push(elapsed);
                outputs.push(output);
            }
            1 => {
                let (elapsed, output) =
                    run_ferrozip_extract(ferrozip, &threads, &archive, root, name, round);
                ferrozip_extract.push(elapsed);
                outputs.push(output);
                let (elapsed, output) =
                    run_hx6zip_extract(root, &info, name, round, IntegrityMode::Safe, "hx-safe");
                hx6zip_safe_extract.push(elapsed);
                outputs.push(output);
                let (elapsed, output) = run_hx6zip_extract(
                    root,
                    &info,
                    name,
                    round,
                    IntegrityMode::Normal,
                    "hx-normal",
                );
                hx6zip_normal_extract.push(elapsed);
                outputs.push(output);
            }
            _ => {
                let (elapsed, output) =
                    run_hx6zip_extract(root, &info, name, round, IntegrityMode::Safe, "hx-safe");
                hx6zip_safe_extract.push(elapsed);
                outputs.push(output);
                let (elapsed, output) = run_hx6zip_extract(
                    root,
                    &info,
                    name,
                    round,
                    IntegrityMode::Normal,
                    "hx-normal",
                );
                hx6zip_normal_extract.push(elapsed);
                outputs.push(output);
                let (elapsed, output) =
                    run_ferrozip_extract(ferrozip, &threads, &archive, root, name, round);
                ferrozip_extract.push(elapsed);
                outputs.push(output);
            }
        }
    }

    for destination in &outputs {
        for index in 0..file_count {
            let filename = format!("{index:05}.bin");
            assert_eq!(
                fs::read(destination.join(name).join(&filename)).unwrap(),
                fs::read(source.join(&filename)).unwrap(),
                "解压内容不一致：{}",
                destination.display()
            );
        }
    }
    let total_mib = (file_count * file_size) as f64 / (1024.0 * 1024.0);
    let ferrozip = median_seconds(ferrozip_extract);
    let hx_normal = median_seconds(hx6zip_normal_extract);
    let hx_safe = median_seconds(hx6zip_safe_extract);
    println!(
        "{name:12} ZIP extract median of 5: FerroZip={ferrozip:.3}s ({:.1} MiB/s), Hx6Zip-normal={hx_normal:.3}s ({:.1} MiB/s), Hx6Zip-safe={hx_safe:.3}s ({:.1} MiB/s); archive={} bytes",
        total_mib / ferrozip,
        total_mib / hx_normal,
        total_mib / hx_safe,
        fs::metadata(archive).unwrap().len(),
    );
}

fn run_hx6zip_extract(
    root: &Path,
    info: &hx6zip::archive::ArchiveInfo,
    name: &str,
    round: usize,
    integrity: IntegrityMode,
    label: &str,
) -> (Duration, std::path::PathBuf) {
    let destination = root.join(format!("{name}-{label}-{round}"));
    let started = Instant::now();
    extract_archive(
        info,
        &ExtractOptions {
            destination: destination.clone(),
            password: None,
            selected: None,
            overwrite: false,
        },
        &JobControl::default().with_integrity(integrity),
    )
    .unwrap();
    (started.elapsed(), destination)
}

fn run_ferrozip_extract(
    runner: &std::ffi::OsStr,
    threads: &str,
    archive: &Path,
    root: &Path,
    name: &str,
    round: usize,
) -> (Duration, std::path::PathBuf) {
    let destination = root.join(format!("{name}-ferrozip-{round}"));
    let output = Command::new(runner)
        .arg(archive)
        .arg(&destination)
        .arg(threads)
        .output()
        .expect("无法启动 FerroZip 基准运行器");
    assert!(
        output.status.success(),
        "FerroZip 解压失败：{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let elapsed = String::from_utf8(output.stdout)
        .expect("FerroZip 基准输出不是 UTF-8")
        .split_whitespace()
        .next()
        .expect("FerroZip 基准未报告耗时")
        .parse::<f64>()
        .expect("FerroZip 基准耗时无效");
    (Duration::from_secs_f64(elapsed), destination)
}

#[test]
#[ignore = "requires FERROZIP_BENCH_RUNNER built against FerroZip fz-core"]
fn benchmark_zip_extraction_against_ferrozip() {
    let runner = std::env::var_os("FERROZIP_BENCH_RUNNER")
        .expect("set FERROZIP_BENCH_RUNNER to the FerroZip API benchmark runner");
    let root = TempDir::new().unwrap();
    benchmark_zip_extractor(root.path(), &runner, "small-files", 500, 1024);
    benchmark_zip_extractor(root.path(), &runner, "medium-files", 8, 2 * 1024 * 1024);
    benchmark_zip_extractor(root.path(), &runner, "large-file", 1, 32 * 1024 * 1024);
}

fn compare_zip_workload(
    root: &Path,
    seven_zip: &std::ffi::OsStr,
    name: &str,
    file_count: usize,
    file_size: usize,
) {
    let source = root.join(name);
    fs::create_dir(&source).unwrap();
    for index in 0..file_count {
        fs::write(
            source.join(format!("{index:05}.bin")),
            data(file_size, index as u64 + 1),
        )
        .unwrap();
    }

    let mut hx6zip_create = Vec::new();
    let mut sevenzip_create = Vec::new();
    let mut hx6zip_extract = Vec::new();
    let mut sevenzip_extract = Vec::new();
    let mut archive_sizes = (0, 0);
    for round in 0..3 {
        let archive = root.join(format!("{name}-hx6zip-{round}.zip"));
        let external_archive = root.join(format!("{name}-7zip-{round}.zip"));
        let control = JobControl::default();
        let started = Instant::now();
        create_archive(
            &CreateOptions {
                sources: vec![source.clone()],
                destination: archive.clone(),
                format: ArchiveFormat::Zip,
                password: None,
                overwrite: false,
            },
            &control,
        )
        .unwrap();
        hx6zip_create.push(started.elapsed());

        let started = Instant::now();
        run_7zip(
            Command::new(seven_zip)
                .current_dir(root)
                .args(["a", "-tzip", "-mm=Deflate", "-mx=6", "-y", "-bd"])
                .arg(seven_zip_threads())
                .arg(&external_archive)
                .arg(name),
        );
        sevenzip_create.push(started.elapsed());
        archive_sizes = (
            fs::metadata(&archive).unwrap().len(),
            fs::metadata(&external_archive).unwrap().len(),
        );

        let hx6zip_output = root.join(format!("{name}-hx6zip-output-{round}"));
        let started = Instant::now();
        let info = inspect_archive(&archive, None, &control).unwrap();
        extract_archive(
            &info,
            &ExtractOptions {
                destination: hx6zip_output.clone(),
                password: None,
                selected: None,
                overwrite: false,
            },
            &control,
        )
        .unwrap();
        hx6zip_extract.push(started.elapsed());

        let external_output = root.join(format!("{name}-7zip-output-{round}"));
        let mut output_argument = OsString::from("-o");
        output_argument.push(&external_output);
        let started = Instant::now();
        run_7zip(
            Command::new(seven_zip)
                .args(["x", "-mmt=4", "-y", "-bd"])
                .arg(output_argument)
                .arg(&archive),
        );
        sevenzip_extract.push(started.elapsed());

        let compatibility_output = root.join(format!("{name}-7zip-archive-check"));
        if round == 0 {
            let external_info = inspect_archive(&external_archive, None, &control).unwrap();
            extract_archive(
                &external_info,
                &ExtractOptions {
                    destination: compatibility_output.clone(),
                    password: None,
                    selected: None,
                    overwrite: false,
                },
                &control,
            )
            .unwrap();
        }
        for index in 0..file_count {
            let file = format!("{index:05}.bin");
            let expected = fs::read(source.join(&file)).unwrap();
            assert_eq!(
                fs::read(hx6zip_output.join(name).join(&file)).unwrap(),
                expected
            );
            assert_eq!(
                fs::read(external_output.join(name).join(&file)).unwrap(),
                expected
            );
            if round == 0 {
                assert_eq!(
                    fs::read(compatibility_output.join(name).join(&file)).unwrap(),
                    expected
                );
            }
        }
    }

    println!(
        "{name:12} ZIP median of 3: create Hx6Zip={:.3}s 7-Zip={:.3}s; extract Hx6Zip={:.3}s 7-Zip={:.3}s; sizes Hx6Zip={} 7-Zip={} bytes",
        median_seconds(hx6zip_create),
        median_seconds(sevenzip_create),
        median_seconds(hx6zip_extract),
        median_seconds(sevenzip_extract),
        archive_sizes.0,
        archive_sizes.1,
    );
}

/// PeaZip's default "Normal" LZMA2 switches (peach.pas: level, dictionary, fast bytes, solid).
const PEAZIP_7Z_NORMAL: [&str; 7] = [
    "-t7z",
    "-m0=LZMA2",
    "-mx5",
    "-md=32m",
    "-mfb=32",
    "-ms=8g",
    "-mmt=on",
];

fn compare_sevenzip_workload(
    root: &Path,
    seven_zip: &std::ffi::OsStr,
    name: &str,
    file_count: usize,
    file_size: usize,
    repeated: bool,
) {
    let source = root.join(name);
    fs::create_dir(&source).unwrap();
    for index in 0..file_count {
        fs::write(
            source.join(format!("{index:05}.bin")),
            data(file_size, if repeated { 1 } else { index as u64 + 1 }),
        )
        .unwrap();
    }
    let mut hx6zip = Vec::new();
    let mut external = Vec::new();
    let mut sizes = (0, 0);
    for round in 0..3 {
        let archive = root.join(format!("{name}-hx6zip-{round}.7z"));
        let external_archive = root.join(format!("{name}-7zip-{round}.7z"));
        let started = Instant::now();
        create_archive(
            &CreateOptions {
                sources: vec![source.clone()],
                destination: archive.clone(),
                format: ArchiveFormat::SevenZip,
                password: None,
                overwrite: false,
            },
            &JobControl::default(),
        )
        .unwrap();
        hx6zip.push(started.elapsed());
        let started = Instant::now();
        run_7zip(
            Command::new(seven_zip)
                .current_dir(root)
                .arg("a")
                .args(PEAZIP_7Z_NORMAL)
                .args(["-y", "-bd"])
                .arg(&external_archive)
                .arg(name),
        );
        external.push(started.elapsed());
        run_7zip(Command::new(seven_zip).args(["t", "-bd"]).arg(&archive));
        sizes = (
            fs::metadata(&archive).unwrap().len(),
            fs::metadata(&external_archive).unwrap().len(),
        );
    }
    let input = (file_count * file_size) as f64 / (1024.0 * 1024.0);
    let (hx6zip, external) = (median_seconds(hx6zip), median_seconds(external));
    println!(
        "{name:12} 7z median of 3: Hx6Zip={hx6zip:.3}s ({:.1} MB/s) PeaZip-normal={external:.3}s ({:.1} MB/s); sizes Hx6Zip={} 7-Zip={} bytes",
        input / hx6zip,
        input / external,
        sizes.0,
        sizes.1,
    );
}

#[test]
#[ignore = "requires 7-Zip and compares synthetic 7z workloads in release mode"]
fn benchmark_sevenzip_against_7zip() {
    let seven_zip = std::env::var_os("HX6ZIP_BENCH_7Z").unwrap_or_else(|| OsString::from("7z"));
    let root = TempDir::new().unwrap();
    compare_sevenzip_workload(root.path(), &seven_zip, "small-files", 500, 1024, false);
    compare_sevenzip_workload(
        root.path(),
        &seven_zip,
        "medium-files",
        32,
        2 * 1024 * 1024,
        false,
    );
    compare_sevenzip_workload(
        root.path(),
        &seven_zip,
        "large-file",
        1,
        256 * 1024 * 1024,
        false,
    );
}

#[test]
#[ignore = "requires 7-Zip and compares repeated files in release mode"]
fn benchmark_sevenzip_repeated_against_7zip() {
    let seven_zip = std::env::var_os("HX6ZIP_BENCH_7Z").unwrap_or_else(|| OsString::from("7z"));
    let root = TempDir::new().unwrap();
    compare_sevenzip_workload(
        root.path(),
        &seven_zip,
        "repeated-files",
        12,
        2 * 1024 * 1024,
        true,
    );
}

#[test]
#[ignore = "requires 7-Zip and compresses the directory selected by HX6ZIP_BENCH_SOURCE"]
fn benchmark_sevenzip_directory_against_7zip() {
    let seven_zip = std::env::var_os("HX6ZIP_BENCH_7Z").unwrap_or_else(|| OsString::from("7z"));
    let source = fs::canonicalize(
        std::env::var_os("HX6ZIP_BENCH_SOURCE")
            .expect("set HX6ZIP_BENCH_SOURCE to an input directory"),
    )
    .unwrap();
    let root = TempDir::new().unwrap();
    let archive = root.path().join("hx6zip.7z");
    let started = Instant::now();
    create_archive(
        &CreateOptions {
            sources: vec![source.clone()],
            destination: archive.clone(),
            format: ArchiveFormat::SevenZip,
            password: None,
            overwrite: false,
        },
        &JobControl::default(),
    )
    .unwrap();
    let hx6zip = started.elapsed().as_secs_f64();
    let external_archive = root.path().join("7zip.7z");
    let started = Instant::now();
    run_7zip(
        Command::new(&seven_zip)
            .arg("a")
            .args(PEAZIP_7Z_NORMAL)
            .args(["-y", "-bd"])
            .arg(&external_archive)
            .arg(&source),
    );
    let external = started.elapsed().as_secs_f64();
    run_7zip(Command::new(&seven_zip).args(["t", "-bd"]).arg(&archive));
    println!(
        "directory 7z: Hx6Zip={hx6zip:.3}s PeaZip-normal={external:.3}s; sizes Hx6Zip={} 7-Zip={} bytes",
        fs::metadata(&archive).unwrap().len(),
        fs::metadata(&external_archive).unwrap().len(),
    );
}

#[test]
#[ignore = "requires 7-Zip and compares synthetic workloads in release mode"]
fn benchmark_zip_against_7zip() {
    let seven_zip = std::env::var_os("HX6ZIP_BENCH_7Z").unwrap_or_else(|| OsString::from("7z"));
    let root = TempDir::new().unwrap();
    compare_zip_workload(root.path(), &seven_zip, "small-files", 500, 1024);
    compare_zip_workload(root.path(), &seven_zip, "medium-files", 8, 2 * 1024 * 1024);
    compare_zip_workload(root.path(), &seven_zip, "large-file", 1, 32 * 1024 * 1024);
}
