use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

use crate::archive::ArchiveFormat;
use anyhow::{Context, Result};

#[derive(Debug, Eq, PartialEq)]
pub enum StartupAction {
    Split {
        path: PathBuf,
    },
    Merge {
        sources: Vec<PathBuf>,
    },
    Open(PathBuf),
    Extract {
        path: PathBuf,
        named_folder: bool,
    },
    ConfigureExtract {
        path: PathBuf,
    },
    Create {
        format: ArchiveFormat,
        sources: Vec<PathBuf>,
    },
    ConfigureCreate {
        sources: Vec<PathBuf>,
    },
    CreateEach {
        sources: Vec<PathBuf>,
    },
}

pub fn automatic_archive_path(sources: &[PathBuf], format: ArchiveFormat) -> Result<PathBuf> {
    let first = sources.first().context("右键菜单没有传入文件或文件夹")?;
    if sources.len() == 1 {
        return default_archive_path(first, format).context("无法确定压缩包保存位置");
    }
    let parent = first.parent().context("无法确定源文件夹")?;
    let name = parent
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("archive"));
    let mut name = name.to_os_string();
    name.push(format!(".{}", format.extension()));
    Ok(parent.join(name))
}

pub fn default_archive_path(source: &Path, format: ArchiveFormat) -> Option<PathBuf> {
    let file_name = source.file_name()?.to_str()?;
    let parent = source.parent()?;
    let mut destination = parent.join(file_name);
    if source
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case(format.extension()))
    {
        destination.set_file_name(format!("{file_name}.{}", format.extension()));
    } else {
        destination.set_extension(format.extension());
    }
    Some(destination)
}

pub fn startup_action(args: impl Iterator<Item = OsString>) -> Option<StartupAction> {
    let mut args = args;
    let first = args.next()?;
    if first == "--create-each-zip" {
        return Some(StartupAction::CreateEach {
            sources: args.map(PathBuf::from).collect(),
        });
    }
    if first == "--split-file" {
        return Some(StartupAction::Split {
            path: args.next()?.into(),
        });
    }
    if first == "--merge-files" {
        return Some(StartupAction::Merge {
            sources: args.map(PathBuf::from).collect(),
        });
    }
    if matches!(first.to_str(), Some("--extract-here" | "--extract-folder")) {
        return Some(StartupAction::Extract {
            path: args.next()?.into(),
            named_folder: first == "--extract-folder",
        });
    }
    if first == "--extract-to" {
        return Some(StartupAction::ConfigureExtract {
            path: args.next()?.into(),
        });
    }
    if first == "--add-to-archive" {
        return Some(StartupAction::ConfigureCreate {
            sources: args.map(PathBuf::from).collect(),
        });
    }
    let format = match first.to_str() {
        Some("--create-zip") => Some(ArchiveFormat::Zip),
        Some("--create-7z") => Some(ArchiveFormat::SevenZip),
        _ => None,
    };
    if let Some(format) = format {
        Some(StartupAction::Create {
            format,
            sources: args.map(PathBuf::from).collect(),
        })
    } else {
        Some(StartupAction::Open(first.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_archive_open_arguments() {
        assert_eq!(
            startup_action([r"C:\archives\my archive.zip".into()].into_iter()),
            Some(StartupAction::Open(r"C:\archives\my archive.zip".into()))
        );
    }

    #[test]
    fn parses_quick_extraction_arguments() {
        for (flag, named_folder) in [("--extract-here", false), ("--extract-folder", true)] {
            assert_eq!(
                startup_action([flag.into(), r"C:\archives\my archive.zip".into()].into_iter()),
                Some(StartupAction::Extract {
                    path: r"C:\archives\my archive.zip".into(),
                    named_folder
                })
            );
            assert_eq!(startup_action([flag.into()].into_iter()), None);
        }
    }

    #[test]
    fn parses_compression_context_menu_arguments() {
        assert_eq!(
            startup_action(
                [
                    "--create-7z".into(),
                    r"C:\files\first file.txt".into(),
                    r"C:\files\second.txt".into(),
                ]
                .into_iter()
            ),
            Some(StartupAction::Create {
                format: ArchiveFormat::SevenZip,
                sources: vec![
                    r"C:\files\first file.txt".into(),
                    r"C:\files\second.txt".into()
                ]
            })
        );
    }

    #[test]
    fn parses_separate_compression_arguments() {
        assert_eq!(
            startup_action(
                [
                    "--create-each-zip".into(),
                    r"C:\files\first file.txt".into(),
                    r"D:\other\资料.txt".into(),
                ]
                .into_iter()
            ),
            Some(StartupAction::CreateEach {
                sources: vec![
                    r"C:\files\first file.txt".into(),
                    r"D:\other\资料.txt".into(),
                ]
            })
        );
    }

    #[test]
    fn parses_custom_context_menu_arguments() {
        assert_eq!(
            startup_action(
                [
                    "--add-to-archive".into(),
                    r"C:\files\first file.txt".into(),
                    r"C:\files\second.txt".into(),
                ]
                .into_iter()
            ),
            Some(StartupAction::ConfigureCreate {
                sources: vec![
                    r"C:\files\first file.txt".into(),
                    r"C:\files\second.txt".into()
                ]
            })
        );
        assert_eq!(
            startup_action(["--extract-to".into(), r"C:\files\archive.7z".into()].into_iter()),
            Some(StartupAction::ConfigureExtract {
                path: r"C:\files\archive.7z".into(),
            })
        );
        assert_eq!(startup_action(["--extract-to".into()].into_iter()), None);
    }

    #[test]
    fn default_archive_path_does_not_replace_a_selected_archive() {
        assert_eq!(
            default_archive_path(Path::new(r"C:\files\source.zip"), ArchiveFormat::Zip),
            Some(PathBuf::from(r"C:\files\source.zip.zip"))
        );
        assert_eq!(
            default_archive_path(Path::new(r"C:\files\source.zip"), ArchiveFormat::SevenZip),
            Some(PathBuf::from(r"C:\files\source.7z"))
        );
        assert_eq!(
            default_archive_path(Path::new(r"C:\files\source.zip"), ArchiveFormat::Tar),
            Some(PathBuf::from(r"C:\files\source.tar"))
        );
    }
}
