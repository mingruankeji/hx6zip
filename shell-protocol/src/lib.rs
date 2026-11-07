pub const SUPPORTED_EXTENSIONS: &[&str] = &[
    "zip", "zipx", "jar", "odt", "ods", "odp", "odg", "odf", "odb", "sxw", "sxc", "sxi", "sxd",
    "7z", "cab", "rar", "iso", "tar", "gz", "tgz", "bz2", "tbz", "tbz2", "xz", "txz", "zst",
    "zstd", "tzst", "br", "brotli", "tbr",
];

// Keep the traditional menu's Hx6ZipArchiveFilter in sync; the test below checks it.
pub const EXTRACT_EXCLUDED_EXTENSIONS: &[&str] = &[
    "txt", "log", "md", "csv", "json", "xml", "yaml", "yml", "toml", "ini", "cfg",
    "rtf", "pdf", "htm", "html", "css", "js", "ts", "jsx", "tsx", "c", "h", "cpp",
    "hpp", "rs", "py", "java", "go", "sql", "bat", "cmd", "ps1", "vbs",
    "png", "jpg", "jpeg", "gif", "bmp", "tif", "tiff", "ico", "svg", "webp", "avif",
    "mp3", "wav", "flac", "aac", "ogg", "mp4", "mkv", "avi", "mov", "webm", "wma",
    "wmv", "lnk", "url",
];

/// A cheap menu hint, not content detection. Unknown extensions remain eligible.
pub fn can_attempt_extraction(path: &std::path::Path) -> bool {
    path.file_name().is_some()
        && path
            .extension()
            .and_then(|value| value.to_str())
            .is_none_or(|extension| {
                !EXTRACT_EXCLUDED_EXTENSIONS
                    .iter()
                    .any(|excluded| extension.eq_ignore_ascii_case(excluded))
            })
}

pub fn can_extract_selection(paths: &[std::path::PathBuf]) -> bool {
    paths.len() == 1 && can_attempt_extraction(&paths[0]) && paths[0].is_file()
}

#[cfg(test)]
mod extraction_tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn menu_filter_excludes_plain_files_but_allows_unknown_and_executable_names() {
        for extension in EXTRACT_EXCLUDED_EXTENSIONS {
            for extension in [extension.to_string(), extension.to_uppercase()] {
                let name = format!("file.{extension}");
                assert!(!can_attempt_extraction(Path::new(&name)), "{name}");
            }
        }
        for extension in SUPPORTED_EXTENSIONS
            .iter()
            .chain(["exe", "run", "sh", "bin", "appimage", "unknown", "docx"].iter())
        {
            let name = format!("file.{extension}");
            assert!(can_attempt_extraction(Path::new(&name)), "{name}");
        }
        assert!(can_attempt_extraction(Path::new("program")));
        assert!(!can_attempt_extraction(Path::new("")));
        assert!(!can_extract_selection(&[]));
        assert!(!can_extract_selection(&[
            Path::new(env!("CARGO_MANIFEST_DIR")).to_owned(),
        ]));
        assert!(!can_extract_selection(&[Path::new("missing.exe").to_owned()]));
        assert!(!can_extract_selection(&[
            Path::new("one.exe").to_owned(),
            Path::new("two.exe").to_owned(),
        ]));
    }

    #[test]
    fn traditional_menu_uses_the_same_exclusion_list_without_changing_associations() {
        let manifest = include_str!(r"..\..\installer\Package.wxs");
        let filter = manifest.lines().next().unwrap();
        let actual: Vec<_> = filter
            .split("System.FileExtension:=\".")
            .skip(1)
            .map(|part| part.split('"').next().unwrap())
            .collect();
        assert_eq!(actual, EXTRACT_EXCLUDED_EXTENSIONS);
        assert!(filter.contains(" AND NOT ("));
        assert!(filter.contains("NOT System.ItemType:=\"Directory\""));
        assert_eq!(
            manifest.matches("Value=\"$(var.Hx6ZipArchiveFilter)\"").count(),
            4
        );
        assert!(!manifest.contains("Software\\Classes\\.exe\\OpenWithProgids"));
        assert!(!manifest.contains("Name=\".exe\" Value=\"Hx6Zip.Archive.1\""));
    }
}

pub const SPLIT_SIZES_MB: &[(&str, u64)] = &[
    ("10M", 10),
    ("100M", 100),
    ("1000M", 1000),
    ("650M-CD", 650),
    ("700M-CD", 700),
    ("4092M-FAT", 4092),
    ("4480M-DVD", 4480),
    ("8128M-DVD DL", 8128),
    ("23040M-BD", 23040),
];

/// Returns one contiguous, same-directory group, starting at .001, in numeric order.
pub fn merge_parts(
    paths: &[std::path::PathBuf],
) -> Option<(std::path::PathBuf, Vec<std::path::PathBuf>)> {
    if paths.len() < 2 {
        return None;
    }
    let mut parts = Vec::with_capacity(paths.len());
    let first = paths.first()?;
    let parent = first.parent()?;
    let stem = first.file_stem()?;
    if stem.is_empty() {
        return None;
    }
    for path in paths {
        if path.parent()? != parent || path.file_stem()? != stem {
            return None;
        }
        let suffix = path.extension()?.to_str()?;
        if suffix.len() < 3 || !suffix.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let index = suffix.parse::<usize>().ok()?;
        if suffix != format!("{index:03}") {
            return None;
        }
        parts.push((index, path.clone()));
    }
    parts.sort_unstable_by_key(|(index, _)| *index);
    if parts
        .iter()
        .enumerate()
        .any(|(i, (index, _))| *index != i + 1)
    {
        return None;
    }
    Some((
        parent.join(stem),
        parts.into_iter().map(|(_, path)| path).collect(),
    ))
}

pub fn can_split(paths: &[std::path::PathBuf]) -> bool {
    paths.len() == 1 && paths[0].is_file()
}

pub fn can_merge(paths: &[std::path::PathBuf]) -> bool {
    merge_parts(paths).is_some() && paths.iter().all(|path| path.is_file())
}

pub fn archive_stem(path: &std::path::Path) -> Option<&std::ffi::OsStr> {
    let stem = path.file_stem()?;
    let compressed_tar = path
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|extension| {
            ["gz", "bz2", "xz", "zst", "zstd", "br", "brotli"]
                .iter()
                .any(|suffix| extension.eq_ignore_ascii_case(suffix))
        })
        && std::path::Path::new(stem)
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("tar"));
    if compressed_tar {
        std::path::Path::new(stem).file_stem()
    } else {
        Some(stem)
    }
}

#[cfg(windows)]
mod windows_protocol {
    use std::{
        ffi::OsString,
        io::{Read, Write},
        os::windows::ffi::{OsStrExt, OsStringExt},
        path::{Path, PathBuf},
        process::{Command, Stdio},
    };

    use anyhow::{Context, Result, ensure};
    use windows::Win32::{
        System::Com::CoTaskMemFree,
        UI::Shell::{IShellItemArray, SIGDN_FILESYSPATH},
    };

    pub const MAX_SELECTION_ITEMS: usize = 100_000;
    const MAX_PATH_UNITS: usize = 32768;
    const MAX_SELECTION_BYTES: usize = 64 * 1024 * 1024;

    #[derive(Clone, Copy)]
    pub enum CompressionAction {
        Zip,
        EachZip,
        SevenZip,
        Configure,
        Split,
        Merge,
    }

    pub fn read_sources(mut reader: impl Read) -> Result<Vec<PathBuf>> {
        let mut data = Vec::new();
        reader
            .by_ref()
            .take(MAX_SELECTION_BYTES as u64 + 1)
            .read_to_end(&mut data)
            .context("无法读取资源管理器选择列表")?;
        ensure!(data.len() <= MAX_SELECTION_BYTES, "资源管理器选择列表过大");
        let mut input = data.as_slice();
        fn number(input: &mut &[u8]) -> Result<usize> {
            let mut bytes = [0; 4];
            input
                .read_exact(&mut bytes)
                .context("资源管理器选择列表不完整")?;
            Ok(u32::from_le_bytes(bytes) as usize)
        }
        let count = number(&mut input)?;
        ensure!(
            (1..=MAX_SELECTION_ITEMS).contains(&count),
            "资源管理器选择数量无效"
        );
        let mut paths = Vec::with_capacity(count);
        for _ in 0..count {
            let len = number(&mut input)?;
            ensure!(
                (1..=MAX_PATH_UNITS).contains(&len) && len * 2 <= input.len(),
                "资源管理器路径长度无效"
            );
            let units: Vec<u16> = input[..len * 2]
                .chunks_exact(2)
                .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
                .collect();
            ensure!(!units.contains(&0), "资源管理器路径包含空字符");
            let path = PathBuf::from(OsString::from_wide(&units));
            ensure!(path.is_absolute(), "资源管理器必须传入绝对路径");
            paths.push(path);
            input = &input[len * 2..];
        }
        ensure!(input.is_empty(), "资源管理器选择列表含有多余数据");
        Ok(paths)
    }

    pub fn encode_sources(paths: &[PathBuf]) -> Result<Vec<u8>> {
        ensure!(
            (1..=MAX_SELECTION_ITEMS).contains(&paths.len()),
            "资源管理器选择数量无效"
        );
        let mut data = Vec::new();
        data.extend_from_slice(&(paths.len() as u32).to_le_bytes());
        for path in paths {
            let units: Vec<_> = path.as_os_str().encode_wide().collect();
            ensure!(
                path.is_absolute()
                    && (1..=MAX_PATH_UNITS).contains(&units.len())
                    && !units.contains(&0),
                "资源管理器路径无效：{}",
                path.display()
            );
            ensure!(
                data.len() + 4 + units.len() * 2 <= MAX_SELECTION_BYTES,
                "资源管理器选择列表过大"
            );
            data.extend_from_slice(&(units.len() as u32).to_le_bytes());
            for unit in units {
                data.extend_from_slice(&unit.to_le_bytes());
            }
        }
        Ok(data)
    }

    pub fn selected_paths(selection: &IShellItemArray) -> Result<Vec<PathBuf>> {
        let count = unsafe { selection.GetCount()? } as usize;
        ensure!(
            (1..=MAX_SELECTION_ITEMS).contains(&count),
            "资源管理器选择数量无效"
        );
        let mut sources = Vec::with_capacity(count);
        for index in 0..count {
            let item = unsafe { selection.GetItemAt(index as u32)? };
            let name = unsafe { item.GetDisplayName(SIGDN_FILESYSPATH)? };
            let path = unsafe { OsString::from_wide(name.as_wide()) };
            unsafe { CoTaskMemFree(Some(name.0.cast())) };
            sources.push(PathBuf::from(path));
        }
        Ok(sources)
    }

    pub fn launch_compression(
        executable: &Path,
        selection: &IShellItemArray,
        action: CompressionAction,
    ) -> Result<()> {
        launch_compression_paths(executable, &selected_paths(selection)?, action)
    }

    pub fn launch_compression_paths(
        executable: &Path,
        paths: &[PathBuf],
        action: CompressionAction,
    ) -> Result<()> {
        let payload = encode_sources(paths)?;
        let mut child = Command::new(executable)
            .arg(match action {
                CompressionAction::Zip => "--shell-create-zip",
                CompressionAction::EachZip => "--shell-create-each-zip",
                CompressionAction::SevenZip => "--shell-create-7z",
                CompressionAction::Configure => "--shell-add-to-archive",
                CompressionAction::Split => "--shell-split-file",
                CompressionAction::Merge => "--shell-merge-files",
            })
            .stdin(Stdio::piped())
            .spawn()
            .context("无法启动 Hx6Zip 操作窗口")?;
        child
            .stdin
            .take()
            .context("无法打开选择列表管道")?
            .write_all(&payload)
            .context("无法传递完整选择列表")?;
        Ok(())
    }
}

#[cfg(windows)]
pub use windows_protocol::*;
