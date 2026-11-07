use std::{
    ffi::OsString,
    fs, io,
    os::windows::{ffi::OsStringExt, fs::MetadataExt},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use windows::{
    Win32::{
        Foundation::{HWND, POINT},
        Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT,
        System::{
            Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree, IDispatch, IServiceProvider},
            Variant::VARIANT,
        },
        UI::{
            Accessibility::{
                CUIAutomation8, IUIAutomation2, IUIAutomationElement, UIA_EditControlTypeId,
                UIA_ImageControlTypeId, UIA_ListControlTypeId, UIA_ListItemControlTypeId,
                UIA_TextControlTypeId,
            },
            Shell::{
                FOLDERID_Desktop, FOLDERID_PublicDesktop, IFolderView, IFolderView2,
                IPersistFolder2, IShellBrowser, IShellItem, IShellView, IShellWindows,
                KF_FLAG_DEFAULT, SHCreateItemFromIDList, SHGetKnownFolderPath,
                SID_STopLevelBrowser, SIGDN_FILESYSPATH, SIGDN_NORMALDISPLAY, SVGIO_ALLVIEW,
                SWC_DESKTOP, SWFO_NEEDDISPATCH, ShellWindows,
            },
            WindowsAndMessaging::{
                GetCursorPos, GetWindowThreadProcessId, IsChild, IsWindowVisible, WindowFromPoint,
            },
        },
    },
    core::{Interface, PWSTR},
};

#[derive(Debug, PartialEq)]
pub(super) struct Destination {
    pub directory: PathBuf,
    pub file_icon: bool,
}

fn contains_window(parent: HWND, child: HWND) -> bool {
    !parent.is_invalid()
        && !child.is_invalid()
        && (parent == child || unsafe { IsChild(parent, child).as_bool() })
}

fn shell_view(dispatch: &IDispatch) -> Result<IShellView> {
    let provider: IServiceProvider = dispatch.cast()?;
    let browser: IShellBrowser = unsafe { provider.QueryService(&SID_STopLevelBrowser)? };
    unsafe {
        browser
            .QueryActiveShellView()
            .context("无法读取资源管理器视图")
    }
}

fn owned_path(value: PWSTR) -> Result<PathBuf> {
    let path = unsafe { PathBuf::from(OsString::from_wide(value.as_wide())) };
    unsafe { CoTaskMemFree(Some(value.0.cast())) };
    anyhow::ensure!(path.is_absolute(), "拖放目标不是绝对文件系统路径");
    Ok(path)
}

fn folder_path(view: &IShellView, desktop: bool) -> Result<PathBuf> {
    if desktop {
        return owned_path(unsafe {
            SHGetKnownFolderPath(&FOLDERID_Desktop, KF_FLAG_DEFAULT, None)?
        });
    }
    let folder: IFolderView = view.cast()?;
    let folder: IPersistFolder2 = unsafe { folder.GetFolder()? };
    let pidl = unsafe { folder.GetCurFolder()? };
    let item: windows::core::Result<IShellItem> = unsafe { SHCreateItemFromIDList(pidl) };
    unsafe { CoTaskMemFree(Some(pidl.cast())) };
    owned_path(unsafe { item?.GetDisplayName(SIGDN_FILESYSPATH)? })
}

struct ListHit {
    leaf: IUIAutomationElement,
    item: Option<IUIAutomationElement>,
}

fn list_hit(automation: &IUIAutomation2, view: HWND, point: POINT) -> Result<Option<ListHit>> {
    let leaf = unsafe { automation.ElementFromPoint(point)? };
    let initial_type = unsafe { leaf.CurrentControlType()? };
    // Explorer exposes filename cells as UIProperty edits even outside rename mode.
    let property_cell = initial_type == UIA_EditControlTypeId
        && unsafe { leaf.CurrentClassName()? }.to_string() == "UIProperty";
    if !property_cell
        && ![
            UIA_ListControlTypeId,
            UIA_ListItemControlTypeId,
            UIA_TextControlTypeId,
            UIA_ImageControlTypeId,
        ]
        .contains(&initial_type)
    {
        return Ok(None);
    }
    let mut process = 0;
    unsafe { GetWindowThreadProcessId(view, Some(&mut process)) };
    if unsafe { leaf.CurrentProcessId()? } as u32 != process {
        return Ok(None);
    }
    let walker = unsafe { automation.RawViewWalker()? };
    let mut element = leaf.clone();
    let mut item = None;
    for _ in 0..16 {
        let kind = unsafe { element.CurrentControlType()? };
        if kind == UIA_ListItemControlTypeId && item.is_none() {
            item = Some(element.clone());
        }
        if kind == UIA_ListControlTypeId {
            let class = unsafe { element.CurrentClassName()? }.to_string();
            let window = unsafe { element.CurrentNativeWindowHandle()? };
            if matches!(class.as_str(), "UIItemsView" | "SysListView32")
                && contains_window(view, window)
                && (item.is_some() || initial_type == UIA_ListControlTypeId)
            {
                return Ok(Some(ListHit { leaf, item }));
            }
            return Ok(None);
        }
        element = unsafe { walker.GetParentElement(&element)? };
    }
    Ok(None)
}

fn item_destination(
    view: &IShellView,
    element: &IUIAutomationElement,
) -> Result<Option<Destination>> {
    let name = unsafe { element.CurrentName()? };
    let folder: IFolderView2 = view.cast()?;
    let mut result = None;
    // Resolve real Shell items, never append a possibly extension-hidden label to a path.
    // Duplicate labels are usable only if every matching item has the same destination.
    for index in 0..unsafe { folder.ItemCount(SVGIO_ALLVIEW)? } {
        let item: IShellItem = unsafe { folder.GetItem(index)? };
        let display = unsafe { item.GetDisplayName(SIGDN_NORMALDISPLAY)? };
        let matches = unsafe { display.as_wide() == &name[..] };
        unsafe { CoTaskMemFree(Some(display.0.cast())) };
        if !matches {
            continue;
        }
        let path = owned_path(unsafe { item.GetDisplayName(SIGDN_FILESYSPATH)? })?;
        let metadata = fs::symlink_metadata(&path).context("无法检查图标对应的文件")?;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
            return Ok(None);
        }
        let file_icon = metadata.is_file();
        let directory = if metadata.is_dir() {
            path
        } else if file_icon {
            path.parent().context("无法确定文件所在目录")?.to_path_buf()
        } else {
            return Ok(None);
        };
        let candidate = Destination {
            directory,
            file_icon,
        };
        if result
            .as_ref()
            .is_some_and(|previous| previous != &candidate)
        {
            eprintln!("Direct drag: ambiguous Shell item labels; not claiming the drop");
            return Ok(None);
        }
        result = Some(candidate);
    }
    if unsafe { element.CurrentName()? } != name {
        return Ok(None);
    }
    Ok(result)
}

fn view_destination(
    view: &IShellView,
    desktop: bool,
    target: HWND,
    point: POINT,
    names: &[PathBuf],
) -> Result<Option<Destination>> {
    let window = unsafe { view.GetWindow()? };
    let under_pointer = unsafe { WindowFromPoint(point) };
    if !unsafe { IsWindowVisible(window).as_bool() }
        || !contains_window(window, under_pointer)
        || !(contains_window(window, target) || contains_window(target, window))
    {
        return Ok(None);
    }
    let view_path = folder_path(view, desktop)?;
    let automation: IUIAutomation2 =
        unsafe { CoCreateInstance(&CUIAutomation8, None, CLSCTX_ALL)? };
    unsafe {
        automation.SetConnectionTimeout(1000)?;
        automation.SetTransactionTimeout(1000)?;
    }
    let Some(hit) = list_hit(&automation, window, point)? else {
        return Ok(None);
    };
    let destination = if let Some(item) = &hit.item {
        let Some(destination) = item_destination(view, item)? else {
            return Ok(None);
        };
        destination
    } else {
        Destination {
            directory: view_path.clone(),
            file_icon: false,
        }
    };
    if desktop && hit.item.is_none() {
        let public = owned_path(unsafe {
            SHGetKnownFolderPath(&FOLDERID_PublicDesktop, KF_FLAG_DEFAULT, None)?
        })?;
        if has_existing_roots(&public, names)? {
            return Ok(None);
        }
    }
    if folder_path(view, desktop)? != view_path || !destination.directory.is_dir() {
        return Ok(None);
    }
    let current = unsafe { automation.ElementFromPoint(point)? };
    if !unsafe { automation.CompareElements(&hit.leaf, &current)?.as_bool() } {
        return Ok(None);
    }
    if unsafe { WindowFromPoint(point) } != under_pointer {
        return Ok(None);
    }
    Ok(Some(destination))
}

fn has_existing_roots(directory: &Path, names: &[PathBuf]) -> Result<bool> {
    for name in names {
        match fs::symlink_metadata(directory.join(name)) {
            Ok(_) => return Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("无法检查拖放目标中的同名项目"),
        }
    }
    Ok(false)
}

pub(super) fn destination(target: HWND, names: &[PathBuf]) -> Result<Option<Destination>> {
    let mut point = POINT::default();
    unsafe { GetCursorPos(&mut point)? };
    // Freeze the release point; moving the mouse afterwards must not choose another folder.
    destination_at_point(target, point, names)
}

fn destination_at_point(
    target: HWND,
    point: POINT,
    names: &[PathBuf],
) -> Result<Option<Destination>> {
    // Some drag loops do not supply IDropSourceNotify; validate the window under the release point.
    let target = if target.is_invalid() {
        unsafe { WindowFromPoint(point) }
    } else {
        target
    };
    let windows: IShellWindows = unsafe { CoCreateInstance(&ShellWindows, None, CLSCTX_ALL)? };
    for index in 0..unsafe { windows.Count()? } {
        let candidate = (|| {
            let dispatch = unsafe { windows.Item(&VARIANT::from(index))? };
            view_destination(&shell_view(&dispatch)?, false, target, point, names)
        })();
        match candidate {
            Ok(Some(path)) => return Ok(Some(path)),
            Ok(None) => {}
            Err(error) => eprintln!("Direct drag: skipping Shell view: {error:#}"),
        }
    }
    let mut desktop_window = 0;
    let desktop = unsafe {
        windows.FindWindowSW(
            &VARIANT::default(),
            &VARIANT::default(),
            SWC_DESKTOP,
            &mut desktop_window,
            SWFO_NEEDDISPATCH,
        )?
    };
    view_destination(&shell_view(&desktop)?, true, target, point, names)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::{
        Foundation::RECT,
        System::Ole::OleInitialize,
        UI::{
            Accessibility::{
                TreeScope_Descendants, UIA_ControlTypePropertyId, UIA_ListItemControlTypeId,
            },
            WindowsAndMessaging::{
                GA_ROOT, GetAncestor, GetWindowRect, HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOACTIVATE,
                SWP_NOMOVE, SWP_NOSIZE, SetWindowPos,
            },
        },
    };

    #[test]
    fn existing_drop_roots_require_shell_collision_handling() {
        let temp = tempfile::tempdir().unwrap();
        let names = vec![PathBuf::from("folder")];
        assert!(!has_existing_roots(temp.path(), &names).unwrap());
        fs::create_dir(temp.path().join("folder")).unwrap();
        assert!(has_existing_roots(temp.path(), &names).unwrap());
        assert!(has_existing_roots(temp.path(), &[PathBuf::from("folder")]).unwrap());
    }

    #[test]
    #[ignore = "requires an explicitly opened probe Explorer window in an interactive desktop"]
    fn native_direct_target_probe() {
        unsafe { OleInitialize(None).unwrap() };
        let _apartment = super::super::OleApartment;
        let expected =
            PathBuf::from(std::env::var_os("HX6ZIP_DRAG_PROBE_DIRECTORY").expect("probe path"));
        let windows: IShellWindows =
            unsafe { CoCreateInstance(&ShellWindows, None, CLSCTX_ALL).unwrap() };
        let mut found = false;
        for index in 0..unsafe { windows.Count().unwrap() } {
            let dispatch = unsafe { windows.Item(&VARIANT::from(index)).unwrap() };
            let view = shell_view(&dispatch).unwrap();
            let window = unsafe { view.GetWindow().unwrap() };
            if !unsafe { IsWindowVisible(window).as_bool() }
                || folder_path(&view, false).unwrap() != expected
            {
                continue;
            }
            let frame = unsafe { GetAncestor(window, GA_ROOT) };
            struct Restore(HWND);
            impl Drop for Restore {
                fn drop(&mut self) {
                    unsafe {
                        SetWindowPos(
                            self.0,
                            Some(HWND_NOTOPMOST),
                            0,
                            0,
                            0,
                            0,
                            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                        )
                        .unwrap();
                    }
                }
            }
            unsafe {
                SetWindowPos(
                    frame,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                )
                .unwrap();
            }
            let _restore = Restore(frame);
            let mut rect = RECT::default();
            unsafe { GetWindowRect(window, &mut rect).unwrap() };
            let point = POINT {
                x: rect.right - 40,
                y: rect.bottom - 40,
            };
            let names = [PathBuf::from("Hx6ZipDragProbe")];
            let under = unsafe { WindowFromPoint(point) };
            let probe: IUIAutomation2 =
                unsafe { CoCreateInstance(&CUIAutomation8, None, CLSCTX_ALL).unwrap() };
            let hit = unsafe { probe.ElementFromPoint(point).unwrap() };
            eprintln!(
                "view={window:?} frame={frame:?} rect={rect:?} point={point:?} under={under:?} child={} hit={:?} class={} hwnd={:?}",
                contains_window(window, under),
                unsafe { hit.CurrentControlType().unwrap() },
                unsafe { hit.CurrentClassName().unwrap() },
                unsafe { hit.CurrentNativeWindowHandle().unwrap() }
            );
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            loop {
                if destination_at_point(HWND::default(), point, &names)
                    .unwrap()
                    .map(|target| target.directory)
                    == Some(expected.clone())
                {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "probe window did not expose its background"
                );
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            assert_eq!(
                destination_at_point(window, point, &names)
                    .unwrap()
                    .map(|target| target.directory),
                Some(expected.clone())
            );
            assert_eq!(
                destination_at_point(window, point, &[PathBuf::from("ExistingFolder")])
                    .unwrap()
                    .map(|target| target.directory),
                Some(expected.clone())
            );
            let automation: IUIAutomation2 =
                unsafe { CoCreateInstance(&CUIAutomation8, None, CLSCTX_ALL).unwrap() };
            let root = unsafe { automation.ElementFromHandle(window).unwrap() };
            let condition = unsafe {
                automation
                    .CreatePropertyCondition(
                        UIA_ControlTypePropertyId,
                        &VARIANT::from(UIA_ListItemControlTypeId.0),
                    )
                    .unwrap()
            };
            let items = unsafe { root.FindAll(TreeScope_Descendants, &condition).unwrap() };
            let mut checked = 0;
            for index in 0..unsafe { items.Length().unwrap() } {
                let item = unsafe { items.GetElement(index).unwrap() };
                let name = unsafe { item.CurrentName().unwrap() }.to_string();
                if !matches!(
                    name.as_str(),
                    "ExistingFolder" | "target.txt" | "target.csv"
                ) {
                    continue;
                }
                let bounds = unsafe { item.CurrentBoundingRectangle().unwrap() };
                let icon = POINT {
                    x: bounds.left + 35,
                    y: (bounds.top + bounds.bottom) / 2,
                };
                let target = destination_at_point(window, icon, &names).unwrap().unwrap();
                let directory = if name == "ExistingFolder" {
                    expected.join("ExistingFolder")
                } else {
                    expected.clone()
                };
                assert_eq!(target.directory, directory);
                assert_eq!(target.file_icon, name != "ExistingFolder");
                eprintln!("Confirmed icon {name} -> {}", target.directory.display());
                checked += 1;
            }
            assert_eq!(
                checked, 3,
                "probe needs ExistingFolder, target.txt and target.csv"
            );
            let outside = POINT {
                x: rect.left - 30,
                y: (rect.top + rect.bottom) / 2,
            };
            assert_eq!(destination_at_point(window, outside, &names).unwrap(), None);
            eprintln!(
                "Confirmed Explorer background, folder/file icons and existing-root handling: {}",
                expected.display()
            );
            found = true;
        }
        assert!(
            found,
            "probe Explorer folder must be open and not minimized"
        );
    }
}
