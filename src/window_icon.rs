use anyhow::{Context, Result, bail};
use gpui_kit::Window;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, WPARAM},
        Storage::EnhancedStorage::{
            PKEY_AppUserModel_ID, PKEY_AppUserModel_RelaunchCommand,
            PKEY_AppUserModel_RelaunchDisplayNameResource, PKEY_AppUserModel_RelaunchIconResource,
        },
        System::{
            Com::StructuredStorage::{PROPVARIANT, PropVariantClear},
            LibraryLoader::GetModuleHandleW,
            Variant::VT_LPWSTR,
        },
        UI::{
            Shell::{
                PropertiesSystem::{IPropertyStore, SHGetPropertyStoreForWindow},
                SHStrDupW,
            },
            WindowsAndMessaging::{
                GetSystemMetrics, ICON_BIG, ICON_SMALL, IMAGE_ICON, LR_SHARED, LoadImageW,
                SM_CXICON, SM_CXSMICON, SM_CYICON, SM_CYSMICON, SendMessageW, WM_SETICON,
            },
        },
    },
    core::PCWSTR,
};

/// Must match the System.AppUserModel.ID of the Start menu shortcut in installer\Package.wxs.
pub const APP_USER_MODEL_ID: &str = "MZip.Desktop";

pub fn set_window_icon(window: &Window) -> Result<()> {
    let handle =
        HasWindowHandle::window_handle(window).context("cannot get native window handle")?;
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        bail!("expected a Win32 window handle");
    };
    let hwnd = HWND(handle.hwnd.get() as *mut std::ffi::c_void);
    unsafe {
        let module = GetModuleHandleW(None).context("cannot get application module")?;
        for (kind, width, height) in [
            (ICON_BIG, SM_CXICON, SM_CYICON),
            (ICON_SMALL, SM_CXSMICON, SM_CYSMICON),
        ] {
            // Resource 1 is embedded by build.rs; shared icons live for the process lifetime.
            let icon = LoadImageW(
                Some(module.into()),
                PCWSTR(1 as *const u16),
                IMAGE_ICON,
                GetSystemMetrics(width),
                GetSystemMetrics(height),
                LR_SHARED,
            )
            .context("cannot load embedded application icon")?;
            SendMessageW(
                hwnd,
                WM_SETICON,
                Some(WPARAM(kind as usize)),
                Some(LPARAM(icon.0 as isize)),
            );
        }
    }
    set_taskbar_identity(hwnd)
}

// Windows started from the Windows 11 menu inherit the menu package identity, so the
// taskbar would look up package logos. A fixed window AUMID makes every launch path use
// the same group and the executable's icon.
fn set_taskbar_identity(hwnd: HWND) -> Result<()> {
    let executable = std::env::current_exe().context("cannot locate MZip executable")?;
    let executable = executable.display();
    let store: IPropertyStore =
        unsafe { SHGetPropertyStoreForWindow(hwnd) }.context("cannot open window properties")?;
    for (key, value) in [
        (PKEY_AppUserModel_ID, APP_USER_MODEL_ID.to_owned()),
        (
            PKEY_AppUserModel_RelaunchCommand,
            format!("\"{executable}\""),
        ),
        (
            PKEY_AppUserModel_RelaunchDisplayNameResource,
            "MZip".to_owned(),
        ),
        (
            PKEY_AppUserModel_RelaunchIconResource,
            format!("{executable},-1"),
        ),
    ] {
        let wide: Vec<u16> = value.encode_utf16().chain([0]).collect();
        unsafe {
            let mut variant = PROPVARIANT::default();
            (*variant.Anonymous.Anonymous).vt = VT_LPWSTR;
            (*variant.Anonymous.Anonymous).Anonymous.pwszVal =
                SHStrDupW(PCWSTR(wide.as_ptr())).context("cannot allocate window property")?;
            let result = store.SetValue(&key, &variant);
            let _ = PropVariantClear(&mut variant);
            result.context("cannot set taskbar identity")?;
        }
    }
    unsafe { store.Commit() }.context("cannot save taskbar identity")
}
