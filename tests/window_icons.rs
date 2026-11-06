#![cfg(all(windows, feature = "gui"))]

use std::{
    process::{Child, Command},
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, WPARAM},
        Storage::EnhancedStorage::PKEY_AppUserModel_ID,
        System::Com::StructuredStorage::PropVariantClear,
        UI::{
            Shell::PropertiesSystem::{IPropertyStore, SHGetPropertyStoreForWindow},
            WindowsAndMessaging::{
                EnumWindows, GetWindowThreadProcessId, ICON_BIG, ICON_SMALL, IsWindowVisible,
                PostMessageW, SendMessageW, WM_CLOSE, WM_GETICON,
            },
        },
    },
    core::BOOL,
};

fn window_app_id(window: HWND) -> String {
    unsafe {
        let store: IPropertyStore = SHGetPropertyStoreForWindow(window).unwrap();
        let mut value = store.GetValue(&PKEY_AppUserModel_ID).unwrap();
        let text = (*value.Anonymous.Anonymous).Anonymous.pwszVal;
        let id = if text.is_null() {
            String::new()
        } else {
            text.to_string().unwrap()
        };
        PropVariantClear(&mut value).unwrap();
        id
    }
}

struct TestProcess(Child);

impl Drop for TestProcess {
    fn drop(&mut self) {
        if self.0.try_wait().unwrap().is_none() {
            self.0.kill().unwrap();
        }
        self.0.wait().unwrap();
    }
}

struct WindowSearch {
    process_id: u32,
    window: Option<HWND>,
}

unsafe extern "system" fn find_window(hwnd: HWND, data: LPARAM) -> BOOL {
    let search = unsafe { &mut *(data.0 as *mut WindowSearch) };
    let mut process_id = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process_id)) };
    if process_id == search.process_id && unsafe { IsWindowVisible(hwnd).as_bool() } {
        search.window = Some(hwnd);
    }
    BOOL(1)
}

#[test]
#[ignore = "opens the built GUI; requires an interactive Windows desktop and closed API tester"]
fn all_window_types_set_large_and_small_application_icons() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("split-source.txt");
    std::fs::write(&source, b"icon test").unwrap();
    for args in [
        vec![],
        vec!["--split-file".into(), source.into_os_string()],
        vec!["--api-tester".into()],
    ] {
        let mut process = TestProcess(
            Command::new(env!("CARGO_BIN_EXE_mzip"))
                .args(&args)
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        let window = loop {
            assert!(
                process.0.try_wait().unwrap().is_none(),
                "application exited before opening a window: {args:?}"
            );
            let mut search = WindowSearch {
                process_id: process.0.id(),
                window: None,
            };
            unsafe {
                EnumWindows(
                    Some(find_window),
                    LPARAM(&mut search as *mut WindowSearch as isize),
                )
                .unwrap();
            }
            if let Some(window) = search.window {
                let icons = [ICON_BIG, ICON_SMALL].map(|kind| unsafe {
                    SendMessageW(window, WM_GETICON, Some(WPARAM(kind as usize)), None).0
                });
                if icons.iter().all(|icon| *icon != 0) && window_app_id(window) == "MZip.Desktop" {
                    break window;
                }
            }
            assert!(
                Instant::now() < deadline,
                "window did not expose both application icons and the MZip taskbar identity: {args:?}"
            );
            thread::sleep(Duration::from_millis(50));
        };
        unsafe { PostMessageW(Some(window), WM_CLOSE, WPARAM(0), LPARAM(0)).unwrap() };
        let deadline = Instant::now() + Duration::from_secs(10);
        while process.0.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "application did not close");
            thread::sleep(Duration::from_millis(50));
        }
    }
}
