#![cfg(windows)]

use hx6zip::archive::{ArchiveFormat, JobControl, inspect_archive};
use std::{
    fs,
    path::Path,
    process::{Child, Command},
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, WPARAM},
        System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize},
        UI::{
            Shell::{
                CMF_NORMAL, CMINVOKECOMMANDINFO, Common::ITEMIDLIST, IContextMenu, IContextMenu2,
                IShellFolder, SHBindToParent, SHParseDisplayName,
            },
            WindowsAndMessaging::{
                CreatePopupMenu, DestroyMenu, FindWindowW, GetMenuItemCount, GetMenuItemID,
                GetMenuStringW, GetSubMenu, HMENU, MF_BYPOSITION, PostMessageW, SW_SHOWNORMAL,
                WM_CLOSE, WM_INITMENUPOPUP,
            },
        },
    },
    core::{HSTRING, Interface, PCSTR},
};

struct Apartment;
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}
struct Pidl(*mut ITEMIDLIST);
impl Drop for Pidl {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(Some(self.0.cast())) };
    }
}
struct Menu(HMENU);
impl Drop for Menu {
    fn drop(&mut self) {
        unsafe { DestroyMenu(self.0).unwrap() };
    }
}
struct ProgressWindow(String);
impl Drop for ProgressWindow {
    fn drop(&mut self) {
        if let Ok(window) = unsafe { FindWindowW(None, &HSTRING::from(&self.0)) } {
            unsafe { PostMessageW(Some(window), WM_CLOSE, WPARAM(0), LPARAM(0)).unwrap() };
        }
    }
}

fn label(menu: HMENU, index: i32) -> String {
    let mut text = [0u16; 256];
    let len = unsafe { GetMenuStringW(menu, index as u32, Some(&mut text), MF_BYPOSITION) };
    String::from_utf16_lossy(&text[..len as usize]).replace('&', "")
}

fn invoke(sources: &[impl AsRef<Path>], format: ArchiveFormat) {
    invoke_named(
        sources,
        match format {
            ArchiveFormat::Zip => "创建 ZIP 压缩包",
            ArchiveFormat::SevenZip => "创建 7z 压缩包",
            ArchiveFormat::Tar => "创建 TAR 压缩包",
        },
    );
}

fn invoke_named(sources: &[impl AsRef<Path>], expected: &str) {
    unsafe {
        let mut absolute = Vec::new();
        let mut relative = Vec::new();
        let mut folder = None;
        for source in sources {
            let mut pidl = std::ptr::null_mut();
            SHParseDisplayName(
                &HSTRING::from(source.as_ref().as_os_str()),
                None,
                &mut pidl,
                0,
                None,
            )
            .unwrap();
            let pidl = Pidl(pidl);
            let mut child = std::ptr::null_mut();
            let parent: IShellFolder = SHBindToParent(pidl.0, Some(&mut child)).unwrap();
            folder = Some(parent);
            relative.push(child.cast_const());
            absolute.push(pidl);
        }
        let context: IContextMenu = folder
            .unwrap()
            .GetUIObjectOf(HWND::default(), &relative, None)
            .unwrap();
        let menu = Menu(CreatePopupMenu().unwrap());
        context
            .QueryContextMenu(menu.0, 0, 1, 0x7fff, CMF_NORMAL)
            .ok()
            .unwrap();
        let root_positions: Vec<_> = (0..GetMenuItemCount(Some(menu.0)))
            .filter(|index| label(menu.0, *index) == "Hx6Zip")
            .collect();
        assert_eq!(
            root_positions.len(),
            1,
            "Hx6Zip must have exactly one top-level menu"
        );
        let submenu = GetSubMenu(menu.0, root_positions[0]);
        assert!(!submenu.is_invalid(), "Hx6Zip is not a cascading menu");
        if let Ok(handler) = context.cast::<IContextMenu2>() {
            handler
                .HandleMenuMsg(
                    WM_INITMENUPOPUP,
                    WPARAM(submenu.0 as usize),
                    LPARAM(root_positions[0] as isize),
                )
                .unwrap();
        }
        let names: Vec<_> = (0..GetMenuItemCount(Some(submenu)))
            .map(|i| label(submenu, i))
            .collect();
        println!(
            "Shell submenu for {} selected items: {names:?}",
            sources.len()
        );
        let should_offer_open = sources.len() == 1
            && sources[0].as_ref().is_file()
            && hx6zip_shell_protocol::can_attempt_extraction(sources[0].as_ref());
        assert_eq!(
            names.iter().any(|name| name == "打开和解压"),
            should_offer_open,
            "open command must be available only for a single extraction candidate"
        );
        let index = names
            .iter()
            .position(|name| name == expected)
            .expect("compression submenu command missing");
        let command = GetMenuItemID(submenu, index as i32);
        assert!((1..0x7fff).contains(&command));
        context
            .InvokeCommand(&CMINVOKECOMMANDINFO {
                cbSize: size_of::<CMINVOKECOMMANDINFO>() as u32,
                lpVerb: PCSTR((command - 1) as usize as *const u8),
                nShow: SW_SHOWNORMAL.0,
                ..Default::default()
            })
            .unwrap();
    }
}

struct TestProcess(Child);

impl Drop for TestProcess {
    fn drop(&mut self) {
        if self.0.try_wait().is_ok_and(|status| status.is_none()) {
            if let Err(error) = self.0.kill() {
                eprintln!("unable to stop test Hx6Zip process: {error}");
            }
            if let Err(error) = self.0.wait() {
                eprintln!("unable to wait for test Hx6Zip process: {error}");
            }
        }
    }
}

#[test]
#[ignore = "opens the built GUI; requires an interactive Windows desktop"]
fn separate_zip_delegate_accepts_a_complete_shell_selection() {
    use windows::Win32::{
        System::Com::{CLSCTX_LOCAL_SERVER, CoCreateInstance},
        UI::Shell::{IExecuteCommand, IObjectWithSelection, SHCreateShellItemArrayFromIDLists},
    };

    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().unwrap() };
    let _apartment = Apartment;
    let temp = TempDir::new().unwrap();
    let mut sources = Vec::new();
    for index in 0..20 {
        let path = temp.path().join(format!("文件 {index}.txt"));
        fs::write(&path, format!("payload {index}")).unwrap();
        sources.push(path);
    }
    let folder = temp.path().join("folder");
    fs::create_dir(&folder).unwrap();
    fs::write(folder.join("nested.txt"), b"nested").unwrap();
    sources.push(folder);
    let mut pidls = Vec::new();
    for path in &sources {
        let mut pidl = std::ptr::null_mut();
        unsafe {
            SHParseDisplayName(&HSTRING::from(path.as_os_str()), None, &mut pidl, 0, None).unwrap();
        }
        pidls.push(Pidl(pidl));
    }
    let pointers: Vec<_> = pidls.iter().map(|pidl| pidl.0.cast_const()).collect();
    let selection = unsafe { SHCreateShellItemArrayFromIDLists(&pointers).unwrap() };
    let mut server = TestProcess(
        Command::new(env!("CARGO_BIN_EXE_hx6zip"))
            .arg("--shell-server")
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    let command: IExecuteCommand = loop {
        let result = unsafe {
            CoCreateInstance(&hx6zip::shell::EACH_ZIP_COMMAND, None, CLSCTX_LOCAL_SERVER)
        };
        match result {
            Ok(command) => break command,
            Err(error) => {
                assert!(
                    Instant::now() < deadline,
                    "delegate activation failed: {error}"
                );
                assert!(
                    server.0.try_wait().unwrap().is_none(),
                    "Shell server exited early"
                );
                thread::sleep(Duration::from_millis(50));
            }
        }
    };
    let _window = ProgressWindow("Hx6Zip - 压缩每个到单独的压缩包中".into());
    unsafe {
        command
            .cast::<IObjectWithSelection>()
            .unwrap()
            .SetSelection(&selection)
            .unwrap();
        command.Execute().unwrap();
    }
    drop(command);
    let deadline = Instant::now() + Duration::from_secs(30);
    while sources
        .iter()
        .any(|source| !source.with_extension("zip").exists())
        && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(50));
    }
    for source in sources {
        let info =
            inspect_archive(&source.with_extension("zip"), None, &JobControl::default()).unwrap();
        if source.is_dir() {
            assert_eq!(info.entries.len(), 2);
            assert!(
                info.entries
                    .iter()
                    .any(|entry| entry.name == "folder/nested.txt")
            );
        } else {
            assert_eq!(info.entries.len(), 1);
            assert_eq!(
                info.entries[0].name,
                source.file_name().unwrap().to_str().unwrap()
            );
            assert_eq!(info.total_size, fs::metadata(&source).unwrap().len());
        }
    }
}

#[test]
#[ignore = "requires the current MSI installed and an interactive Windows desktop"]
fn explorer_menu_separately_compresses_a_large_mixed_selection() {
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().unwrap() };
    let _apartment = Apartment;
    let temp = TempDir::new().unwrap();
    let mut sources = Vec::new();
    for index in 0..20 {
        let path = temp.path().join(format!("文件 {index}.txt"));
        fs::write(&path, format!("payload {index}")).unwrap();
        sources.push(path);
    }
    let folder = temp.path().join("folder");
    fs::create_dir(&folder).unwrap();
    fs::write(folder.join("nested.txt"), b"nested").unwrap();
    sources.push(folder);
    let _window = ProgressWindow("Hx6Zip - 压缩每个到单独的压缩包中".into());
    invoke_named(&sources, "压缩每个到单独的压缩包中");
    let deadline = Instant::now() + Duration::from_secs(30);
    while sources
        .iter()
        .any(|source| !source.with_extension("zip").exists())
        && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(50));
    }
    for source in &sources {
        let info =
            inspect_archive(&source.with_extension("zip"), None, &JobControl::default()).unwrap();
        assert_eq!(info.entries.len(), if source.is_dir() { 2 } else { 1 });
    }
    assert_eq!(
        fs::read_dir(temp.path()).unwrap().count(),
        sources.len() * 2
    );
}

#[test]
#[ignore = "opens the built GUI; requires an interactive Windows desktop"]
fn separate_zip_cli_and_shell_pipe_create_one_archive_per_selection() {
    use std::{io::Write, process::Stdio};

    let temp = TempDir::new().unwrap();
    for pipe in [false, true] {
        let directory = temp.path().join(if pipe { "pipe" } else { "cli" });
        fs::create_dir(&directory).unwrap();
        let mut sources = Vec::new();
        for index in 0..18 {
            let path = directory.join(format!("资料 {index}.txt"));
            fs::write(&path, format!("payload {index}")).unwrap();
            sources.push(path);
        }
        let folder = directory.join("folder");
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("nested.txt"), b"nested").unwrap();
        sources.push(folder);
        let mut command = Command::new(env!("CARGO_BIN_EXE_hx6zip"));
        if pipe {
            command.arg("--shell-create-each-zip").stdin(Stdio::piped());
        } else {
            command.arg("--create-each-zip").args(&sources);
        }
        let mut process = TestProcess(command.spawn().unwrap());
        if pipe {
            let payload = hx6zip_shell_protocol::encode_sources(&sources).unwrap();
            process.0.stdin.take().unwrap().write_all(&payload).unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        while process.0.try_wait().unwrap().is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(25));
        }
        assert!(
            process
                .0
                .try_wait()
                .unwrap()
                .is_some_and(|status| status.success()),
            "batch window did not finish and auto-close"
        );
        assert_eq!(fs::read_dir(&directory).unwrap().count(), sources.len() * 2);
        for source in sources {
            let output = hx6zip::cli::default_archive_path(&source, ArchiveFormat::Zip).unwrap();
            let info = inspect_archive(&output, None, &JobControl::default()).unwrap();
            assert_eq!(info.entries.len(), if source.is_dir() { 2 } else { 1 });
            if source.is_file() {
                assert_eq!(
                    info.entries[0].name,
                    source.file_name().unwrap().to_str().unwrap()
                );
                assert_eq!(info.total_size, fs::metadata(&source).unwrap().len());
            } else {
                assert!(
                    info.entries
                        .iter()
                        .any(|entry| entry.name == "folder/nested.txt")
                );
            }
        }
    }
}

#[test]
#[ignore = "opens the built GUI; requires an interactive Windows desktop"]
fn file_parts_cli_merges_reverse_selection_and_shell_pipe() {
    use std::{io::Write, process::Stdio};

    let temp = TempDir::new().unwrap();
    for (index, pipe) in [false, true].into_iter().enumerate() {
        let directory = temp.path().join(format!("case-{index}"));
        fs::create_dir(&directory).unwrap();
        let part1 = directory.join("资料.bin.001");
        let part2 = directory.join("资料.bin.002");
        fs::write(&part1, b"first").unwrap();
        fs::write(&part2, b"second").unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_hx6zip"));
        if pipe {
            command.arg("--shell-merge-files").stdin(Stdio::piped());
        } else {
            command
                .current_dir(&directory)
                .arg("--merge-files")
                .arg(part2.file_name().unwrap())
                .arg(part1.file_name().unwrap());
        }
        let mut process = TestProcess(command.spawn().unwrap());
        if pipe {
            let payload = hx6zip_shell_protocol::encode_sources(&[part2, part1]).unwrap();
            process.0.stdin.take().unwrap().write_all(&payload).unwrap();
        }
        let output = directory.join("资料.bin");
        let deadline = Instant::now() + Duration::from_secs(30);
        while !output.exists() && Instant::now() < deadline {
            assert!(
                process.0.try_wait().unwrap().is_none(),
                "merge window exited before output"
            );
            thread::sleep(Duration::from_millis(25));
        }
        assert_eq!(fs::read(&output).unwrap(), b"firstsecond");
        let deadline = Instant::now() + Duration::from_secs(10);
        while process.0.try_wait().unwrap().is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(25));
        }
        assert!(
            process.0.try_wait().unwrap().is_some(),
            "merge window did not auto-close"
        );
    }
}

#[test]
#[ignore = "opens the built GUI; requires an interactive Windows desktop"]
fn quick_extraction_cli_extracts_to_both_destinations() {
    use hx6zip::archive::{CreateOptions, create_archive};

    let temp = TempDir::new().unwrap();
    let source = temp.path().join("input").join("payload");
    fs::create_dir_all(source.join("nested")).unwrap();
    fs::write(source.join("first.txt"), b"first").unwrap();
    fs::write(source.join("nested").join("资料.txt"), b"nested").unwrap();
    for (flag, named_folder) in [("--extract-here", false), ("--extract-folder", true)] {
        let parent = temp
            .path()
            .join(if named_folder { "named" } else { "here" });
        fs::create_dir(&parent).unwrap();
        let archive = parent.join("mxbgl.zip");
        create_archive(
            &CreateOptions {
                sources: vec![source.clone()],
                destination: archive.clone(),
                format: ArchiveFormat::Zip,
                password: None,
                overwrite: false,
            },
            &JobControl::default(),
        )
        .unwrap();
        let destination = if named_folder {
            parent.join("mxbgl")
        } else {
            parent.clone()
        };
        let mut process = TestProcess(
            Command::new(env!("CARGO_BIN_EXE_hx6zip"))
                .arg(flag)
                .arg(&archive)
                .spawn()
                .unwrap(),
        );
        let first = destination.join("payload").join("first.txt");
        let nested = destination.join("payload").join("nested").join("资料.txt");
        let deadline = Instant::now() + Duration::from_secs(30);
        while (!first.exists() || !nested.exists()) && Instant::now() < deadline {
            assert!(
                process.0.try_wait().unwrap().is_none(),
                "quick extraction GUI exited early"
            );
            thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(fs::read(first).unwrap(), b"first");
        assert_eq!(fs::read(nested).unwrap(), b"nested");
        let completed = Instant::now();
        let deadline = completed + Duration::from_secs(10);
        while process.0.try_wait().unwrap().is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(25));
        }
        assert!(
            process.0.try_wait().unwrap().is_some(),
            "countdown did not close the extraction window"
        );
        assert!(completed.elapsed() >= Duration::from_secs(4));
        if named_folder {
            assert!(!parent.join("payload").exists());
        } else {
            assert!(!parent.join("mxbgl").exists());
        }
    }
}

#[test]
#[ignore = "opens the built GUI; requires an interactive Windows desktop"]
fn quick_compression_closes_five_seconds_after_completion_but_not_error() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("input.txt");
    fs::write(&source, b"compression auto-close").unwrap();
    let mut process = TestProcess(
        Command::new(env!("CARGO_BIN_EXE_hx6zip"))
            .arg("--create-zip")
            .arg(&source)
            .spawn()
            .unwrap(),
    );
    let archive = temp.path().join("input.zip");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !archive.exists() && Instant::now() < deadline {
        assert!(
            process.0.try_wait().unwrap().is_none(),
            "compression GUI exited early"
        );
        thread::sleep(Duration::from_millis(25));
    }
    assert!(archive.exists(), "compression did not produce an archive");
    let completed = Instant::now();
    thread::sleep(Duration::from_secs(4));
    assert!(
        process.0.try_wait().unwrap().is_none(),
        "countdown closed too early"
    );
    let deadline = completed + Duration::from_secs(10);
    while process.0.try_wait().unwrap().is_none() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(25));
    }
    assert!(
        process.0.try_wait().unwrap().is_some(),
        "countdown did not close the window"
    );
    assert!(completed.elapsed() >= Duration::from_secs(4));
    let info = inspect_archive(&archive, None, &JobControl::default()).unwrap();
    assert_eq!(info.entries.len(), 1);

    let missing = temp.path().join("missing.txt");
    let mut failed = TestProcess(
        Command::new(env!("CARGO_BIN_EXE_hx6zip"))
            .arg("--create-zip")
            .arg(missing)
            .spawn()
            .unwrap(),
    );
    thread::sleep(Duration::from_secs(7));
    assert!(
        failed.0.try_wait().unwrap().is_none(),
        "error window closed automatically"
    );
}

#[test]
#[ignore = "requires the current MSI installed and an interactive Windows desktop"]
fn explorer_menu_combines_a_large_mixed_selection_into_one_archive() {
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().unwrap() };
    let _apartment = Apartment;
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        let temp = TempDir::with_prefix("Hx6Zip-Shell-Smoke-").unwrap();
        let mut sources = Vec::new();
        for i in 0..20 {
            let path = temp.path().join(format!("文件 {i}.txt"));
            fs::write(&path, format!("content {i}")).unwrap();
            sources.push(path);
        }

        let folder = temp.path().join("nested");
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("child.txt"), b"nested").unwrap();
        sources.push(folder);
        let expected = hx6zip::cli::automatic_archive_path(&sources, format).unwrap();
        let _window = ProgressWindow(format!(
            "Hx6Zip - {}",
            expected.file_name().unwrap().to_string_lossy()
        ));
        invoke(&sources, format);
        let deadline = Instant::now() + Duration::from_secs(30);
        while !expected.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(100));
        }
        assert!(
            expected.exists(),
            "Shell command did not automatically create an archive"
        );
        let info = inspect_archive(&expected, None, &JobControl::default()).unwrap();
        assert_eq!(info.entries.len(), 22, "selection was split or truncated");
        assert!(
            info.entries
                .iter()
                .any(|entry| entry.name == "nested/child.txt")
        );
        assert_eq!(
            fs::read_dir(temp.path()).unwrap().count(),
            22,
            "more than one output archive was created"
        );
        let single = vec![expected];
        let repacked = hx6zip::cli::automatic_archive_path(&single, format).unwrap();
        let _single_window = ProgressWindow(format!(
            "Hx6Zip - {}",
            repacked.file_name().unwrap().to_string_lossy()
        ));
        invoke(&single, format);
        let deadline = Instant::now() + Duration::from_secs(30);
        while !repacked.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(100));
        }
        let info = inspect_archive(&repacked, None, &JobControl::default()).unwrap();
        assert_eq!(info.entries.len(), 1);
    }
}
