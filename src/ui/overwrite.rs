use std::path::PathBuf;

use anyhow::{Result, bail};
use gpui_kit::Window;
use hx6zip::archive::{ArchiveInfo, ExtractOptions, JobControl, extract_archive, extract_conflicts};

#[derive(Debug)]
pub(super) struct ConfirmationCancelled;

impl std::fmt::Display for ConfirmationCancelled {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("已取消同名文件替换")
    }
}

impl std::error::Error for ConfirmationCancelled {}

#[derive(Clone)]
pub(super) struct OverwriteDialog {
    #[cfg(windows)]
    owner: std::result::Result<isize, String>,
}

impl OverwriteDialog {
    pub fn new(window: &Window) -> Self {
        #[cfg(windows)]
        let owner = (|| -> Result<isize> {
            use anyhow::Context as _;
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};

            let handle = HasWindowHandle::window_handle(window).context("无法获取主窗口")?;
            let RawWindowHandle::Win32(handle) = handle.as_raw() else {
                bail!("主窗口不是 Windows 窗口");
            };
            Ok(handle.hwnd.get())
        })()
        .map_err(|error| format!("{error:#}"));
        #[cfg(not(windows))]
        let _ = window;
        Self {
            #[cfg(windows)]
            owner,
        }
    }

    pub fn confirm(&self, conflicts: &[PathBuf]) -> Result<bool> {
        let mut preview = conflicts
            .iter()
            .take(8)
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        if conflicts.len() > 8 {
            preview.push_str(&format!("\n……还有 {} 个文件", conflicts.len() - 8));
        }
        let message = format!(
            "目标位置存在 {} 个同名文件：\n{preview}\n\n选择“全部替换”后将覆盖本次解压中的全部同名文件。已有文件夹会合并，不删除其他文件。",
            conflicts.len()
        );
        self.show(&message)
    }

    #[cfg(windows)]
    fn show(&self, message: &str) -> Result<bool> {
        use anyhow::Context as _;
        use windows::{
            Win32::{
                Foundation::HWND,
                UI::{
                    Controls::{
                        TASKDIALOG_BUTTON, TASKDIALOGCONFIG, TASKDIALOGCONFIG_0, TD_WARNING_ICON,
                        TDF_ALLOW_DIALOG_CANCELLATION, TDF_SIZE_TO_CONTENT, TaskDialogIndirect,
                    },
                    WindowsAndMessaging::IDCANCEL,
                },
            },
            core::{PCWSTR, w},
        };

        const REPLACE: i32 = 1000;
        const CANCEL: i32 = 1001;
        let owner = self
            .owner
            .as_ref()
            .map_err(|error| anyhow::anyhow!("无法显示同名文件确认对话框：{error}"))?;
        let content = message.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
        let buttons = [
            TASKDIALOG_BUTTON {
                nButtonID: REPLACE,
                pszButtonText: w!("全部替换"),
            },
            TASKDIALOG_BUTTON {
                nButtonID: CANCEL,
                pszButtonText: w!("取消"),
            },
        ];
        let config = TASKDIALOGCONFIG {
            cbSize: std::mem::size_of::<TASKDIALOGCONFIG>() as u32,
            hwndParent: HWND(*owner as *mut std::ffi::c_void),
            dwFlags: TDF_ALLOW_DIALOG_CANCELLATION | TDF_SIZE_TO_CONTENT,
            pszWindowTitle: w!("全部替换同名文件？"),
            pszContent: PCWSTR(content.as_ptr()),
            Anonymous1: TASKDIALOGCONFIG_0 {
                pszMainIcon: TD_WARNING_ICON,
            },
            cButtons: buttons.len() as u32,
            pButtons: buttons.as_ptr(),
            nDefaultButton: CANCEL,
            ..Default::default()
        };
        let mut result = 0;
        unsafe { TaskDialogIndirect(&config, Some(&mut result), None, None) }
            .context("无法显示同名文件确认对话框")?;
        match result {
            REPLACE => Ok(true),
            CANCEL => Ok(false),
            value if value == IDCANCEL.0 => Ok(false),
            _ => bail!("同名文件确认对话框返回未知结果：{result}"),
        }
    }

    #[cfg(not(windows))]
    fn show(&self, message: &str) -> Result<bool> {
        let result = rfd::MessageDialog::new()
            .set_title("全部替换同名文件？")
            .set_description(message)
            .set_level(rfd::MessageLevel::Warning)
            .set_buttons(rfd::MessageButtons::OkCancelCustom(
                "全部替换".into(),
                "取消".into(),
            ))
            .show();
        match result {
            rfd::MessageDialogResult::Custom(label) if label == "全部替换" => Ok(true),
            rfd::MessageDialogResult::Custom(label) if label == "取消" => Ok(false),
            rfd::MessageDialogResult::Cancel => Ok(false),
            result => bail!("同名文件确认对话框返回未知结果：{result:?}"),
        }
    }
}

pub(super) fn extract_with_confirmation(
    info: &ArchiveInfo,
    options: ExtractOptions,
    control: &JobControl,
    dialog: &OverwriteDialog,
) -> Result<()> {
    extract_with(info, options, control, |conflicts| {
        dialog.confirm(conflicts)
    })
}

fn extract_with(
    info: &ArchiveInfo,
    mut options: ExtractOptions,
    control: &JobControl,
    confirm: impl FnOnce(&[PathBuf]) -> Result<bool>,
) -> Result<()> {
    let conflicts = extract_conflicts(info, &options, control)?;
    options.overwrite = if conflicts.is_empty() {
        false
    } else if confirm(&conflicts)? {
        true
    } else {
        return Err(ConfirmationCancelled.into());
    };
    control.check_cancelled()?;
    extract_archive(info, &options, control)
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, fs, path::Path};

    use hx6zip::archive::{
        ArchiveFormat, CancellationToken, Cancelled, CreateOptions, IntegrityMode, create_archive,
        inspect_archive,
    };

    use super::*;

    fn fixture(root: &Path, format: ArchiveFormat) -> (ArchiveInfo, BTreeSet<usize>) {
        let source = root.join("tree");
        fs::create_dir_all(source.join("nested")).unwrap();
        for (path, contents) in [
            (PathBuf::from("a.txt"), b"new a".as_slice()),
            (PathBuf::from("nested").join("b.txt"), b"new b".as_slice()),
            (PathBuf::from("new.txt"), b"new file".as_slice()),
            (PathBuf::from("unselected.txt"), b"archive copy".as_slice()),
        ] {
            fs::write(source.join(path), contents).unwrap();
        }
        let archive = root.join(format!("sample.{}", format.extension()));
        create_archive(
            &CreateOptions {
                sources: vec![source],
                destination: archive.clone(),
                format,
                password: None,
                overwrite: false,
            },
            &JobControl::default().with_integrity(IntegrityMode::Normal),
        )
        .unwrap();
        let info = inspect_archive(&archive, None, &JobControl::default()).unwrap();
        let selected = info
            .entries
            .iter()
            .filter(|entry| !entry.is_directory && !entry.name.ends_with("unselected.txt"))
            .map(|entry| entry.index)
            .collect();
        (info, selected)
    }

    fn options(root: &Path, selected: BTreeSet<usize>) -> ExtractOptions {
        ExtractOptions {
            destination: root.join("output"),
            password: None,
            selected: Some(selected),
            overwrite: false,
        }
    }

    fn existing_files(output: &Path) -> (PathBuf, PathBuf, PathBuf) {
        let first = output.join("tree").join("a.txt");
        let second = output.join("tree").join("nested").join("b.txt");
        let unrelated = output.join("tree").join("unselected.txt");
        fs::create_dir_all(second.parent().unwrap()).unwrap();
        fs::write(&first, b"old a").unwrap();
        fs::write(&second, b"old b").unwrap();
        fs::write(&unrelated, b"keep").unwrap();
        (first, second, unrelated)
    }

    #[test]
    fn overwrite_confirmation_cancels_without_writes_or_replaces_only_the_selection() {
        for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
            for mode in [IntegrityMode::Normal, IntegrityMode::Safe] {
                let temp = tempfile::tempdir().unwrap();
                let (info, selected) = fixture(temp.path(), format);
                let original = fs::read(&info.path).unwrap();
                let options = options(temp.path(), selected);
                let (first, second, unrelated) = existing_files(&options.destination);
                let control = JobControl::default().with_integrity(mode);
                let error = extract_with(&info, options.clone(), &control, |conflicts| {
                    let mut actual = conflicts.to_vec();
                    actual.sort();
                    let mut expected = vec![first.clone(), second.clone()];
                    expected.sort();
                    assert_eq!(actual, expected);
                    Ok(false)
                })
                .unwrap_err();
                assert!(error.downcast_ref::<ConfirmationCancelled>().is_some());
                assert_eq!(fs::read(&first).unwrap(), b"old a");
                assert_eq!(fs::read(&second).unwrap(), b"old b");
                assert!(!options.destination.join("tree").join("new.txt").exists());
                assert_eq!(fs::read(&unrelated).unwrap(), b"keep");
                extract_with(&info, options.clone(), &control, |_| Ok(true)).unwrap();
                assert_eq!(fs::read(&first).unwrap(), b"new a");
                assert_eq!(fs::read(&second).unwrap(), b"new b");
                assert_eq!(
                    fs::read(options.destination.join("tree").join("new.txt")).unwrap(),
                    b"new file"
                );
                assert_eq!(fs::read(&unrelated).unwrap(), b"keep");
                assert_eq!(fs::read(&info.path).unwrap(), original);
            }
        }
    }

    #[test]
    fn overwrite_confirmation_skips_dialog_without_conflicts_and_propagates_errors() {
        let temp = tempfile::tempdir().unwrap();
        let (info, selected) = fixture(temp.path(), ArchiveFormat::Zip);
        let options = options(temp.path(), selected);
        let control = JobControl::default().with_integrity(IntegrityMode::Normal);
        extract_with(&info, options.clone(), &control, |_| {
            panic!("a conflict-free extraction must not display a confirmation")
        })
        .unwrap();
        let target = options.destination.join("tree").join("a.txt");
        fs::write(&target, b"existing").unwrap();
        let error = extract_with(&info, options.clone(), &control, |_| {
            anyhow::bail!("dialog failed")
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("dialog failed"));
        assert_eq!(fs::read(&target).unwrap(), b"existing");

        let token = CancellationToken::default();
        let cancel = token.clone();
        let control = JobControl::new(token, |_| {}).with_integrity(IntegrityMode::Normal);
        let error = extract_with(&info, options, &control, |_| {
            cancel.cancel();
            Ok(true)
        })
        .unwrap_err();
        assert!(error.downcast_ref::<Cancelled>().is_some());
        assert_eq!(fs::read(target).unwrap(), b"existing");
    }

    #[test]
    fn overwrite_confirmation_keeps_safe_mode_rollback() {
        let temp = tempfile::tempdir().unwrap();
        let (info, selected) = fixture(temp.path(), ArchiveFormat::Zip);
        let options = options(temp.path(), selected);
        let (first, second, unrelated) = existing_files(&options.destination);
        let token = CancellationToken::default();
        let cancel = token.clone();
        let control = JobControl::new(token, move |progress| {
            if progress.operation == "正在提交解压结果" && progress.completed_bytes > 0 {
                cancel.cancel();
            }
        })
        .with_integrity(IntegrityMode::Safe);
        assert!(extract_with(&info, options.clone(), &control, |_| Ok(true)).is_err());
        assert_eq!(fs::read(first).unwrap(), b"old a");
        assert_eq!(fs::read(second).unwrap(), b"old b");
        assert_eq!(fs::read(unrelated).unwrap(), b"keep");
        assert!(!options.destination.join("tree").join("new.txt").exists());
    }
}
