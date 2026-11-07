#![cfg(windows)]

use std::{
    ffi::c_void,
    fs,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

use hx6zip_shell_protocol::read_sources;
use tempfile::TempDir;
use windows::{
    Win32::{
        Foundation::{FreeLibrary, HMODULE, S_FALSE, S_OK},
        System::{
            Com::{
                COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize,
                IClassFactory,
            },
            LibraryLoader::{GetProcAddress, LoadLibraryW},
        },
        UI::Shell::{
            Common::ITEMIDLIST, ECS_ENABLED, ECS_HIDDEN, IExplorerCommand, IShellItemArray,
            SHCreateShellItemArrayFromIDLists, SHParseDisplayName,
        },
    },
    core::{GUID, HRESULT, HSTRING, Interface, PCWSTR, s},
};

const ROOT_ID: GUID = GUID::from_u128(0x15c13c8a_46d2_45e3_9d26_82518fe73d50);

#[test]
fn installer_registers_single_archive_quick_extraction_commands() {
    let installer = include_str!(r"..\..\installer\Package.wxs");
    for (key, flag, title) in [
        ("04.ExtractHere", "--extract-here", "解压到当前文件夹"),
        (
            "05.ExtractFolder",
            "--extract-folder",
            "解压到压缩文件名文件夹",
        ),
        ("07.ExtractTo", "--extract-to", "解压到"),
    ] {
        let key = format!(r#"Key="Software\Classes\AllFilesystemObjects\shell\Hx6Zip\shell\{key}">"#);
        let section = installer
            .split_once(&key)
            .unwrap()
            .1
            .split("</RegistryKey>")
            .next()
            .unwrap();
        assert!(section.contains(&format!(r#"Value="{title}""#)));
        assert!(section.contains(r#"Name="MultiSelectModel" Value="Single""#));
        assert!(section.contains(&format!("{flag} &quot;%1&quot;")));
        assert!(section.contains(r#"Name="AppliesTo" Value="$(var.Hx6ZipArchiveFilter)""#));
        for extension in hx6zip_shell_protocol::EXTRACT_EXCLUDED_EXTENSIONS {
            assert!(
                installer.contains(&format!("System.FileExtension:=\".{extension}\"")),
                "{extension} missing"
            );
        }
    }
}

#[test]
fn package_manifest_registers_the_same_shell_class() {
    let manifest = include_str!(r"..\..\installer\AppxManifest.xml");
    let class = "15C13C8A-46D2-45E3-9D26-82518FE73D50";
    assert_eq!(manifest.matches(class).count(), 3);
    assert_eq!(manifest.matches("<desktop5:Verb Id=\"Hx6Zip\"").count(), 2);
    for required in [
        "Hx6ZipShell.dll",
        "windows.comServer",
        "windows.fileExplorerContextMenus",
        "Type=\"*\"",
        "Type=\"Directory\"",
        "<uap10:AllowExternalContent>true</uap10:AllowExternalContent>",
        "MinVersion=\"10.0.22000.0\"",
    ] {
        assert!(manifest.contains(required), "missing {required}");
    }
}

struct Apartment;

impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

struct Module(HMODULE);

impl Drop for Module {
    fn drop(&mut self) {
        unsafe { FreeLibrary(self.0).unwrap() };
    }
}

struct Pidl(*mut ITEMIDLIST);

impl Drop for Pidl {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(Some(self.0.cast())) };
    }
}

fn selection(paths: &[&Path]) -> IShellItemArray {
    let pidls: Vec<_> = paths
        .iter()
        .map(|path| {
            let mut item = std::ptr::null_mut();
            unsafe {
                SHParseDisplayName(&HSTRING::from(path.as_os_str()), None, &mut item, 0, None)
                    .unwrap();
            }
            Pidl(item)
        })
        .collect();
    let pointers: Vec<_> = pidls.iter().map(|pidl| pidl.0.cast_const()).collect();
    unsafe { SHCreateShellItemArrayFromIDLists(&pointers).unwrap() }
}

fn build_test_executable(directory: &Path) {
    let source = directory.join("receiver.rs");
    fs::write(
        &source,
        r#"
use std::{io::Read, path::PathBuf};
fn main() {
    let directory = std::env::current_exe().unwrap().parent().unwrap().to_path_buf();
    let argument = std::env::args_os().nth(1).unwrap();
    if argument == "--shell-create-zip" || argument == "--shell-create-each-zip" || argument == "--shell-create-7z" || argument == "--shell-add-to-archive" || argument == "--shell-split-file" || argument == "--shell-merge-files" {
        let mut bytes = Vec::new();
        std::io::stdin().read_to_end(&mut bytes).unwrap();
        std::fs::write(directory.join("received.bin"), bytes).unwrap();
    }
    let argument = PathBuf::from(argument);
    if let Some(path) = std::env::args_os().nth(2) {
        std::fs::write(directory.join("path.txt"), PathBuf::from(path).to_str().unwrap()).unwrap();
    }
    std::fs::write(directory.join("argument.txt"), argument.to_str().unwrap()).unwrap();
    std::fs::write(directory.join("done"), b"done").unwrap();
}
"#,
    )
    .unwrap();
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let output = Command::new(rustc)
        .arg("--edition=2024")
        .arg(&source)
        .arg("-o")
        .arg(directory.join("hx6zip.exe"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "could not build Shell receiver: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn wait_for_receiver(directory: &Path) {
    let done = directory.join("done");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(done.exists(), "Shell command did not launch the receiver");
    fs::remove_file(done).unwrap();
}

#[test]
fn actual_dll_activates_filters_and_invokes_real_shell_selections() {
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().unwrap() };
    let _apartment = Apartment;
    let built_dll = std::env::current_exe()
        .unwrap()
        .with_file_name("hx6zip_shell_extension.dll");
    let directory = TempDir::new().unwrap();
    let dll = directory.path().join("Hx6ZipShell.dll");
    fs::copy(built_dll, &dll).unwrap();
    build_test_executable(directory.path());
    let module = Module(unsafe { LoadLibraryW(&HSTRING::from(dll.as_os_str())).unwrap() });
    type GetClassObject =
        unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut c_void) -> HRESULT;
    type CanUnloadNow = unsafe extern "system" fn() -> HRESULT;
    let get_class: GetClassObject =
        unsafe { std::mem::transmute(GetProcAddress(module.0, s!("DllGetClassObject")).unwrap()) };
    let can_unload: CanUnloadNow =
        unsafe { std::mem::transmute(GetProcAddress(module.0, s!("DllCanUnloadNow")).unwrap()) };

    unsafe {
        assert_eq!(can_unload(), S_OK);
        let mut pointer = std::ptr::null_mut();
        get_class(&ROOT_ID, &IClassFactory::IID, &mut pointer)
            .ok()
            .unwrap();
        let factory = IClassFactory::from_raw(pointer);
        assert_eq!(can_unload(), S_FALSE);
        factory.LockServer(true).unwrap();
        let root: IExplorerCommand = factory.CreateInstance(None).unwrap();
        let icon = root.GetIcon(None).unwrap();
        let icon_path = PCWSTR(icon.0).to_string().unwrap();
        CoTaskMemFree(Some(icon.0.cast()));
        assert!(icon_path.ends_with(r"\hx6zip.exe,0"), "{icon_path}");

        let children = root.EnumSubCommands().unwrap();
        let mut commands = [None, None, None, None, None, None, None, None, None, None];
        let mut fetched = 0;
        assert_eq!(children.Next(&mut commands, Some(&mut fetched)), S_OK);
        assert_eq!(fetched, 10);
        let each = commands[9].as_ref().unwrap();
        let title = each.GetTitle(None).unwrap();
        assert_eq!(
            PCWSTR(title.0).to_string().unwrap(),
            "压缩每个到单独的压缩包中"
        );
        CoTaskMemFree(Some(title.0.cast()));
        let open = commands[2].as_ref().unwrap();

        let archive = directory.path().join("资料.ZIP");
        let text = directory.path().join("ordinary.txt");
        let folder = directory.path().join("folder.zip");
        fs::write(&archive, b"not inspected by the menu").unwrap();
        fs::write(&text, b"plain").unwrap();
        fs::create_dir(&folder).unwrap();
        let archive_selection = selection(&[&archive]);
        let text_selection = selection(&[&text]);
        let folder_selection = selection(&[&folder]);
        let multi_selection = selection(&[&archive, &text]);
        for selected in [&text_selection, &folder_selection, &multi_selection] {
            assert_eq!(
                each.GetState(selected, false).unwrap(),
                ECS_ENABLED.0 as u32
            );
        }
        let split = commands[7].as_ref().unwrap();
        let merge = commands[8].as_ref().unwrap();
        assert_eq!(
            split.GetState(&text_selection, false).unwrap(),
            ECS_ENABLED.0 as u32
        );
        assert_eq!(
            split.GetState(&folder_selection, false).unwrap(),
            ECS_HIDDEN.0 as u32
        );
        assert_eq!(
            split.GetState(&multi_selection, false).unwrap(),
            ECS_HIDDEN.0 as u32
        );
        assert_eq!(
            merge.GetState(&text_selection, false).unwrap(),
            ECS_HIDDEN.0 as u32
        );
        assert_eq!(
            merge.GetState(&multi_selection, false).unwrap(),
            ECS_HIDDEN.0 as u32
        );
        let part1 = directory.path().join("资料.bin.001");
        let part2 = directory.path().join("资料.bin.002");
        let part3 = directory.path().join("资料.bin.003");
        for path in [&part1, &part2, &part3] {
            fs::write(path, b"part").unwrap();
        }
        let parts_selection = selection(&[&part2, &part1]);
        let gap_selection = selection(&[&part1, &part3]);
        assert_eq!(
            merge.GetState(&parts_selection, false).unwrap(),
            ECS_ENABLED.0 as u32
        );
        assert_eq!(
            merge.GetState(&gap_selection, false).unwrap(),
            ECS_HIDDEN.0 as u32
        );
        for (command, selected, flag, expected) in [
            (
                split,
                &text_selection,
                "--shell-split-file",
                vec![text.clone()],
            ),
            (
                merge,
                &parts_selection,
                "--shell-merge-files",
                vec![part2, part1],
            ),
        ] {
            command.Invoke(selected, None).unwrap();
            wait_for_receiver(directory.path());
            assert_eq!(
                fs::read_to_string(directory.path().join("argument.txt")).unwrap(),
                flag
            );
            let payload = fs::read(directory.path().join("received.bin")).unwrap();
            assert_eq!(read_sources(payload.as_slice()).unwrap(), expected);
        }
        assert_eq!(
            open.GetState(&archive_selection, false).unwrap(),
            ECS_ENABLED.0 as u32
        );
        for selected in [&text_selection, &folder_selection, &multi_selection] {
            assert_eq!(open.GetState(selected, false).unwrap(), ECS_HIDDEN.0 as u32);
        }
        for name in [
            "setup.EXE", "program", "linux.run", "program.sh", "unknown.data",
        ] {
            let candidate = directory.path().join(name);
            fs::write(&candidate, b"content is only inspected after invocation").unwrap();
            let selected = selection(&[&candidate]);
            for index in [2, 3, 4, 6] {
                let command = commands[index].as_ref().unwrap();
                assert_eq!(
                    command.GetState(&selected, false).unwrap(),
                    ECS_ENABLED.0 as u32
                );
                command.Invoke(&selected, None).unwrap();
                wait_for_receiver(directory.path());
                let (file, expected) = if index == 2 {
                    ("argument.txt", candidate.to_str().unwrap())
                } else {
                    ("path.txt", candidate.to_str().unwrap())
                };
                assert_eq!(
                    fs::read_to_string(directory.path().join(file)).unwrap(),
                    expected
                );
            }
        }
        for (index, flag) in [
            (3, "--extract-here"),
            (4, "--extract-folder"),
            (6, "--extract-to"),
        ] {
            let extract = commands[index].as_ref().unwrap();
            assert_eq!(
                extract.GetState(&archive_selection, false).unwrap(),
                ECS_ENABLED.0 as u32
            );
            for selected in [&text_selection, &folder_selection, &multi_selection] {
                assert_eq!(
                    extract.GetState(selected, false).unwrap(),
                    ECS_HIDDEN.0 as u32
                );
            }
            let title = extract.GetTitle(&archive_selection).unwrap();
            let text = PCWSTR(title.0).to_string().unwrap();
            CoTaskMemFree(Some(title.0.cast()));
            assert_eq!(
                text,
                if index == 3 {
                    "解压到当前文件夹"
                } else if index == 4 {
                    "解压到「资料」文件夹"
                } else {
                    "解压到"
                }
            );
            extract.Invoke(&archive_selection, None).unwrap();
            wait_for_receiver(directory.path());
            assert_eq!(
                fs::read_to_string(directory.path().join("argument.txt")).unwrap(),
                flag
            );
            assert_eq!(
                fs::read_to_string(directory.path().join("path.txt")).unwrap(),
                archive.to_str().unwrap()
            );
        }

        let mut paths: Vec<PathBuf> = (0..20)
            .map(|index| {
                let path = directory.path().join(format!("文件 {index}.txt"));
                fs::write(&path, "source").unwrap();
                path
            })
            .collect();
        paths.push(folder);
        let path_refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
        let selected = selection(&path_refs);
        for (index, expected) in [
            (0, "--shell-create-zip"),
            (1, "--shell-create-7z"),
            (5, "--shell-add-to-archive"),
            (9, "--shell-create-each-zip"),
        ] {
            commands[index]
                .as_ref()
                .unwrap()
                .Invoke(&selected, None)
                .unwrap();
            wait_for_receiver(directory.path());
            assert_eq!(
                fs::read_to_string(directory.path().join("argument.txt")).unwrap(),
                expected
            );
            let payload = fs::read(directory.path().join("received.bin")).unwrap();
            assert_eq!(read_sources(payload.as_slice()).unwrap(), paths);
        }

        open.Invoke(&archive_selection, None).unwrap();
        wait_for_receiver(directory.path());
        assert_eq!(
            fs::read_to_string(directory.path().join("argument.txt")).unwrap(),
            archive.to_str().unwrap()
        );

        drop(commands);
        drop(children);
        drop(root);
        assert_eq!(can_unload(), S_FALSE);
        factory.LockServer(false).unwrap();
        drop(factory);
        assert_eq!(can_unload(), S_OK);
        for id in [
            GUID::from_u128(0xf4774d42_41e4_4ac8_b964_9a37688ae101),
            GUID::from_u128(0xf4774d42_41e4_4ac8_b964_9a37688ae102),
            GUID::from_u128(0xf4774d42_41e4_4ac8_b964_9a37688ae103),
        ] {
            let mut pointer = std::ptr::null_mut();
            get_class(&id, &IClassFactory::IID, &mut pointer)
                .ok()
                .unwrap();
            let factory = IClassFactory::from_raw(pointer);
            let handler: IExplorerCommand = factory.CreateInstance(None).unwrap();
            assert_eq!(handler.GetCanonicalName().unwrap(), id);
            drop(handler);
            drop(factory);
        }
        assert_eq!(can_unload(), S_OK);
    }
}

#[test]
fn installer_registers_dynamic_selection_handlers() {
    let installer = include_str!(r"..\..\installer\Package.wxs");
    for (key, title, model, class) in [
        (
            "08.SplitFile",
            "分割文件",
            "Single",
            "F4774D42-41E4-4AC8-B964-9A37688AE101",
        ),
        (
            "09.MergeFiles",
            "合并文件",
            "Player",
            "F4774D42-41E4-4AC8-B964-9A37688AE102",
        ),
    ] {
        let key = format!(r#"Key="Software\Classes\AllFilesystemObjects\shell\Hx6Zip\shell\{key}">"#);
        let (before, after) = installer.split_once(&key).unwrap();
        let component = before
            .rsplit_once(r#"<Component Id="Hx6ZipShellIntegration""#)
            .unwrap()
            .1;
        // This key's own opening tag is the only unclosed one, so it sits at component level.
        assert_eq!(
            component.matches("<RegistryKey ").count(),
            component.matches("</RegistryKey>").count() + 1
        );
        let section = after.split("</RegistryKey>").next().unwrap();
        assert!(section.contains(&format!(r#"Value="{title}""#)));
        assert!(section.contains(&format!(r#"Name="MultiSelectModel" Value="{model}""#)));
        assert!(section.contains(&format!(
            r#"Name="ExplorerCommandHandler" Value="{{{class}}}""#
        )));
        assert!(installer.contains(&format!(
            r#"Key="Software\Classes\CLSID\{{{class}}}\InprocServer32""#
        )));
    }
}
