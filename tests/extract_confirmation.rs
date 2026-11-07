#![cfg(all(windows, feature = "gui"))]

use std::{
    fs::{self, File},
    io::Write,
    path::Path,
    process::{Child, Command},
    thread,
    time::{Duration, Instant},
};

use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, WPARAM},
        UI::WindowsAndMessaging::{
            BM_CLICK, BS_DEFPUSHBUTTON, EnumChildWindows, EnumWindows, GUITHREADINFO, GW_OWNER,
            GWL_STYLE, GetClassNameW, GetGUIThreadInfo, GetWindow, GetWindowLongW, GetWindowTextW,
            GetWindowThreadProcessId, IsWindowVisible, PostMessageW, SendMessageW, WM_CLOSE,
            WM_GETTEXT,
        },
    },
    core::BOOL,
};
use zip::{ZipWriter, write::SimpleFileOptions};

struct TestProcess(Child);

impl Drop for TestProcess {
    fn drop(&mut self) {
        if self.0.try_wait().unwrap().is_none() {
            self.0.kill().unwrap();
        }
        self.0.wait().unwrap();
    }
}

struct DialogSearch {
    process_id: u32,
    dialog: Option<HWND>,
}

unsafe extern "system" fn find_dialog(window: HWND, data: LPARAM) -> BOOL {
    let search = unsafe { &mut *(data.0 as *mut DialogSearch) };
    let mut process_id = 0;
    unsafe { GetWindowThreadProcessId(window, Some(&mut process_id)) };
    if process_id == search.process_id && unsafe { IsWindowVisible(window).as_bool() } {
        let mut text = [0u16; 128];
        let length = unsafe { GetWindowTextW(window, &mut text) }.max(0) as usize;
        if String::from_utf16_lossy(&text[..length]) == "全部替换同名文件？" {
            search.dialog = Some(window);
        }
    }
    BOOL(1)
}

unsafe extern "system" fn find_button(window: HWND, data: LPARAM) -> BOOL {
    let buttons = unsafe { &mut *(data.0 as *mut Vec<(String, HWND)>) };
    let mut class = [0u16; 64];
    let length = unsafe { GetClassNameW(window, &mut class) }.max(0) as usize;
    if String::from_utf16_lossy(&class[..length]) == "Button"
        && unsafe { IsWindowVisible(window).as_bool() }
    {
        let mut text = [0u16; 128];
        let length = unsafe {
            SendMessageW(
                window,
                WM_GETTEXT,
                Some(WPARAM(text.len())),
                Some(LPARAM(text.as_mut_ptr() as isize)),
            )
        }
        .0
        .max(0) as usize;
        buttons.push((String::from_utf16_lossy(&text[..length]), window));
    }
    BOOL(1)
}

fn write_archive(path: &Path) {
    let mut writer = ZipWriter::new(File::create(path).unwrap());
    for (name, contents) in [
        ("bundle/alpha.txt", b"new alpha".as_slice()),
        ("bundle/beta.txt", b"new beta".as_slice()),
        ("bundle/new.txt", b"new file".as_slice()),
    ] {
        writer
            .start_file(name, SimpleFileOptions::default())
            .unwrap();
        writer.write_all(contents).unwrap();
    }
    writer.finish().unwrap();
}

#[test]
#[ignore = "opens short-lived Hx6Zip windows; requires an interactive Windows desktop"]
fn quick_extract_confirmation_has_exact_buttons_and_preserves_or_replaces_files() {
    let executable = std::env::var_os("HX6ZIP_TEST_EXTRACT_EXECUTABLE")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_hx6zip").into());
    for (flag, named_folder, choice) in [
        ("--extract-here", false, Some("取消")),
        ("--extract-here", false, Some("全部替换")),
        ("--extract-folder", true, Some("全部替换")),
        ("--extract-folder", true, None),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("sample.zip");
        write_archive(&archive);
        let original = fs::read(&archive).unwrap();
        let destination = if named_folder {
            temp.path().join("sample")
        } else {
            temp.path().to_path_buf()
        };
        let output = destination.join("bundle");
        fs::create_dir_all(&output).unwrap();
        let alpha = output.join("alpha.txt");
        let beta = output.join("beta.txt");
        let new_file = output.join("new.txt");
        let unrelated = output.join("keep.txt");
        fs::write(&alpha, b"old alpha").unwrap();
        fs::write(&beta, b"old beta").unwrap();
        fs::write(&unrelated, b"keep").unwrap();
        let mut process = TestProcess(
            Command::new(&executable)
                .arg(flag)
                .arg(&archive)
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        let dialog = loop {
            assert!(
                process.0.try_wait().unwrap().is_none(),
                "Hx6Zip exited before showing the confirmation"
            );
            let mut search = DialogSearch {
                process_id: process.0.id(),
                dialog: None,
            };
            unsafe {
                EnumWindows(
                    Some(find_dialog),
                    LPARAM(&mut search as *mut DialogSearch as isize),
                )
                .unwrap();
            }
            if let Some(dialog) = search.dialog {
                break dialog;
            }
            assert!(Instant::now() < deadline, "confirmation did not appear");
            thread::sleep(Duration::from_millis(50));
        };
        let owner = unsafe { GetWindow(dialog, GW_OWNER).unwrap() };
        let mut owner_process = 0;
        unsafe { GetWindowThreadProcessId(owner, Some(&mut owner_process)) };
        assert_eq!(owner_process, process.0.id());
        let mut buttons: Vec<(String, HWND)> = Vec::new();
        unsafe {
            let _ = EnumChildWindows(
                Some(dialog),
                Some(find_button),
                LPARAM(&mut buttons as *mut Vec<(String, HWND)> as isize),
            );
        }
        let mut labels = buttons
            .iter()
            .map(|(text, _)| text.as_str())
            .collect::<Vec<_>>();
        labels.sort();
        assert_eq!(labels, ["全部替换", "取消"]);
        let cancel_button = buttons.iter().find(|(text, _)| text == "取消").unwrap().1;
        let dialog_thread = unsafe { GetWindowThreadProcessId(dialog, None) };
        let mut gui = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        unsafe { GetGUIThreadInfo(dialog_thread, &mut gui).unwrap() };
        assert_eq!(gui.hwndFocus, cancel_button);
        assert_eq!(
            unsafe { GetWindowLongW(cancel_button, GWL_STYLE) } & 0x0f,
            BS_DEFPUSHBUTTON
        );
        assert_eq!(fs::read(&alpha).unwrap(), b"old alpha");
        assert_eq!(fs::read(&beta).unwrap(), b"old beta");
        assert!(!new_file.exists());
        if let Some(choice) = choice {
            let button = buttons.iter().find(|(text, _)| text == choice).unwrap().1;
            unsafe { PostMessageW(Some(button), BM_CLICK, WPARAM(0), LPARAM(0)).unwrap() };
        } else {
            unsafe { PostMessageW(Some(dialog), WM_CLOSE, WPARAM(0), LPARAM(0)).unwrap() };
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = process.0.try_wait().unwrap() {
                assert!(status.success(), "Hx6Zip exited unsuccessfully: {status}");
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Hx6Zip did not finish {flag} {choice:?}"
            );
            thread::sleep(Duration::from_millis(50));
        }
        if choice == Some("全部替换") {
            assert_eq!(fs::read(&alpha).unwrap(), b"new alpha");
            assert_eq!(fs::read(&beta).unwrap(), b"new beta");
            assert_eq!(fs::read(new_file).unwrap(), b"new file");
        } else {
            assert_eq!(fs::read(&alpha).unwrap(), b"old alpha");
            assert_eq!(fs::read(&beta).unwrap(), b"old beta");
            assert!(!new_file.exists());
        }
        assert_eq!(fs::read(unrelated).unwrap(), b"keep");
        assert_eq!(fs::read(archive).unwrap(), original);
    }
}
