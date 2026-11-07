#![cfg(windows)]

use std::{
    cell::Cell,
    ffi::{OsString, c_void},
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicIsize, AtomicUsize, Ordering},
};

use anyhow::{Context, Result, bail, ensure};
use hx6zip_shell_protocol::{
    CompressionAction, can_attempt_extraction, launch_compression, selected_paths,
};
use windows::{
    Win32::{
        Foundation::{
            CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_FAIL, E_INVALIDARG, E_NOTIMPL,
            E_POINTER, HINSTANCE, HMODULE, S_FALSE, S_OK,
        },
        System::{
            Com::{IBindCtx, IClassFactory, IClassFactory_Impl},
            LibraryLoader::GetModuleFileNameW,
            SystemServices::{DLL_PROCESS_ATTACH, SFGAO_FILESYSTEM, SFGAO_STREAM},
        },
        UI::{
            Shell::{
                ECF_DEFAULT, ECF_HASSUBCOMMANDS, ECS_ENABLED, ECS_HIDDEN, IEnumExplorerCommand,
                IEnumExplorerCommand_Impl, IExplorerCommand, IExplorerCommand_Impl,
                IShellItemArray, SHStrDupW,
            },
            WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW},
        },
    },
    core::{BOOL, Error, GUID, HRESULT, IUnknown, Interface, PCWSTR, PWSTR, Ref, implement, w},
};

const ROOT_ID: GUID = GUID::from_u128(0x15c13c8a_46d2_45e3_9d26_82518fe73d50);
const ZIP_ID: GUID = GUID::from_u128(0x1af52fa6_6b7b_4797_8b82_13ac2d5a9070);
const SEVENZIP_ID: GUID = GUID::from_u128(0xf85ca4bf_eb5d_474b_bdcc_4e9673ddc076);
const OPEN_ID: GUID = GUID::from_u128(0x0b436baf_17c6_4e6d_9dd8_e99c4735ae18);
const EXTRACT_HERE_ID: GUID = GUID::from_u128(0x6821c903_d988_44c2_9c03_4ca243639171);
const EXTRACT_FOLDER_ID: GUID = GUID::from_u128(0x54049632_e56a_4b1f_bb08_e3744b360a7e);
const ADD_TO_ARCHIVE_ID: GUID = GUID::from_u128(0x3e4692aa_ea59_4a5b_9fe1_4663b5f51a91);
const EXTRACT_TO_ID: GUID = GUID::from_u128(0x4ee15381_c184_46ba_8372_ee6334cbfd1c);
const SPLIT_ID: GUID = GUID::from_u128(0xf4774d42_41e4_4ac8_b964_9a37688ae101);
const MERGE_ID: GUID = GUID::from_u128(0xf4774d42_41e4_4ac8_b964_9a37688ae102);
const EACH_ZIP_ID: GUID = GUID::from_u128(0xf4774d42_41e4_4ac8_b964_9a37688ae103);

static MODULE: AtomicIsize = AtomicIsize::new(0);
static OBJECTS: AtomicUsize = AtomicUsize::new(0);
static LOCKS: AtomicUsize = AtomicUsize::new(0);

struct ObjectReference;

impl ObjectReference {
    fn new() -> Self {
        OBJECTS.fetch_add(1, Ordering::SeqCst);
        Self
    }
}

impl Drop for ObjectReference {
    fn drop(&mut self) {
        OBJECTS.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Root,
    Zip,
    SevenZip,
    Open,
    ExtractHere,
    ExtractFolder,
    AddToArchive,
    ExtractTo,
    Split,
    Merge,
    EachZip,
}

impl Kind {
    fn id(self) -> GUID {
        match self {
            Self::Root => ROOT_ID,
            Self::Zip => ZIP_ID,
            Self::SevenZip => SEVENZIP_ID,
            Self::Open => OPEN_ID,
            Self::ExtractHere => EXTRACT_HERE_ID,
            Self::ExtractFolder => EXTRACT_FOLDER_ID,
            Self::AddToArchive => ADD_TO_ARCHIVE_ID,
            Self::ExtractTo => EXTRACT_TO_ID,
            Self::Split => SPLIT_ID,
            Self::Merge => MERGE_ID,
            Self::EachZip => EACH_ZIP_ID,
        }
    }

    fn title(self) -> PCWSTR {
        match self {
            Self::Root => w!("Hx6Zip"),
            Self::Zip => w!("创建 ZIP 压缩包"),
            Self::SevenZip => w!("创建 7z 压缩包"),
            Self::Open => w!("打开和解压"),
            Self::ExtractHere => w!("解压到当前文件夹"),
            Self::ExtractFolder => w!("解压到压缩文件名文件夹"),
            Self::AddToArchive => w!("添加到压缩文件"),
            Self::ExtractTo => w!("解压到"),
            Self::Split => w!("分割文件"),
            Self::Merge => w!("合并文件"),
            Self::EachZip => w!("压缩每个到单独的压缩包中"),
        }
    }
}

fn executable_path() -> Result<PathBuf> {
    let module = MODULE.load(Ordering::SeqCst);
    ensure!(module != 0, "无法定位 Hx6Zip 右键菜单模块");
    let mut buffer = vec![0u16; 32768];
    let length =
        unsafe { GetModuleFileNameW(Some(HMODULE(module as *mut c_void)), buffer.as_mut_slice()) }
            as usize;
    ensure!(
        length > 0 && length < buffer.len(),
        "无法读取 Hx6Zip 右键菜单模块路径"
    );
    let module_path = PathBuf::from(OsString::from_wide(&buffer[..length]));
    Ok(module_path.with_file_name("hx6zip.exe"))
}

fn show_error(error: &anyhow::Error) -> Error {
    let message = format!("无法执行 Hx6Zip 右键菜单：{error:#}");
    let wide: Vec<_> = message.encode_utf16().chain(Some(0)).collect();
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(wide.as_ptr()),
            w!("Hx6Zip"),
            MB_OK | MB_ICONERROR,
        )
    };
    Error::new(E_FAIL, message)
}

#[implement(IExplorerCommand, Agile = false)]
struct ExplorerCommand {
    kind: Kind,
    _reference: ObjectReference,
}

fn command(kind: Kind) -> IExplorerCommand {
    ExplorerCommand {
        kind,
        _reference: ObjectReference::new(),
    }
    .into()
}

impl IExplorerCommand_Impl for ExplorerCommand_Impl {
    fn GetTitle(&self, selection: Ref<'_, IShellItemArray>) -> windows::core::Result<PWSTR> {
        if self.kind == Kind::ExtractFolder
            && let Some(selection) = selection.as_ref()
        {
            let paths =
                selected_paths(selection).map_err(|error| Error::new(E_FAIL, error.to_string()))?;
            if paths.len() == 1
                && let Some(stem) = hx6zip_shell_protocol::archive_stem(&paths[0])
            {
                let name = stem.to_string_lossy();
                let title: Vec<_> = format!("解压到「{name}」文件夹")
                    .encode_utf16()
                    .chain(Some(0))
                    .collect();
                return unsafe { SHStrDupW(PCWSTR(title.as_ptr())) };
            }
        }
        unsafe { SHStrDupW(self.kind.title()) }
    }

    fn GetIcon(&self, _: Ref<'_, IShellItemArray>) -> windows::core::Result<PWSTR> {
        let executable =
            executable_path().map_err(|error| Error::new(E_FAIL, error.to_string()))?;
        let mut icon = executable.into_os_string();
        icon.push(",0");
        let wide: Vec<_> = icon.encode_wide().chain(Some(0)).collect();
        unsafe { SHStrDupW(PCWSTR(wide.as_ptr())) }
    }

    fn GetToolTip(&self, _: Ref<'_, IShellItemArray>) -> windows::core::Result<PWSTR> {
        Err(E_NOTIMPL.into())
    }

    fn GetCanonicalName(&self) -> windows::core::Result<GUID> {
        Ok(self.kind.id())
    }

    fn GetState(&self, selection: Ref<'_, IShellItemArray>, _: BOOL) -> windows::core::Result<u32> {
        if matches!(self.kind, Kind::Split | Kind::Merge) {
            let Some(selection) = selection.as_ref() else {
                return Ok(ECS_HIDDEN.0 as u32);
            };
            let paths =
                selected_paths(selection).map_err(|error| Error::new(E_FAIL, error.to_string()))?;
            let visible = if self.kind == Kind::Split {
                hx6zip_shell_protocol::can_split(&paths)
            } else {
                hx6zip_shell_protocol::can_merge(&paths)
            };
            return Ok(if visible {
                ECS_ENABLED.0 as u32
            } else {
                ECS_HIDDEN.0 as u32
            });
        }
        if !matches!(
            self.kind,
            Kind::Open | Kind::ExtractHere | Kind::ExtractFolder | Kind::ExtractTo
        ) {
            return Ok(ECS_ENABLED.0 as u32);
        }
        let Some(selection) = selection.as_ref() else {
            return Ok(ECS_HIDDEN.0 as u32);
        };
        if unsafe { selection.GetCount()? } != 1 {
            return Ok(ECS_HIDDEN.0 as u32);
        }
        let item = unsafe { selection.GetItemAt(0)? };
        let file_attributes = SFGAO_FILESYSTEM | SFGAO_STREAM;
        if !unsafe { item.GetAttributes(file_attributes)? }.contains(file_attributes) {
            return Ok(ECS_HIDDEN.0 as u32);
        }
        let paths =
            selected_paths(selection).map_err(|error| Error::new(E_FAIL, error.to_string()))?;
        Ok(if can_attempt_extraction(&paths[0]) {
            ECS_ENABLED.0 as u32
        } else {
            ECS_HIDDEN.0 as u32
        })
    }

    fn Invoke(
        &self,
        selection: Ref<'_, IShellItemArray>,
        _: Ref<'_, IBindCtx>,
    ) -> windows::core::Result<()> {
        let result = (|| -> Result<()> {
            let selection = selection.as_ref().context("资源管理器未传入选择列表")?;
            let executable = executable_path()?;
            match self.kind {
                Kind::Split | Kind::Merge => {
                    let paths = selected_paths(selection)?;
                    let valid = if self.kind == Kind::Split {
                        hx6zip_shell_protocol::can_split(&paths)
                    } else {
                        hx6zip_shell_protocol::can_merge(&paths)
                    };
                    ensure!(
                        valid,
                        "请选择一个文件进行分割，或选择同名且从 .001 开始的连续分片进行合并"
                    );
                    hx6zip_shell_protocol::launch_compression_paths(
                        &executable,
                        &paths,
                        if self.kind == Kind::Split {
                            CompressionAction::Split
                        } else {
                            CompressionAction::Merge
                        },
                    )
                }
                Kind::Zip => launch_compression(&executable, selection, CompressionAction::Zip),
                Kind::EachZip => {
                    launch_compression(&executable, selection, CompressionAction::EachZip)
                }
                Kind::SevenZip => {
                    launch_compression(&executable, selection, CompressionAction::SevenZip)
                }
                Kind::AddToArchive => {
                    launch_compression(&executable, selection, CompressionAction::Configure)
                }
                Kind::Open | Kind::ExtractHere | Kind::ExtractFolder | Kind::ExtractTo => {
                    let paths = selected_paths(selection)?;
                    ensure!(
                        hx6zip_shell_protocol::can_extract_selection(&paths),
                        "请选择一个可尝试解压的文件"
                    );
                    let mut command = Command::new(executable);
                    match self.kind {
                        Kind::ExtractHere => {
                            command.arg("--extract-here");
                        }
                        Kind::ExtractFolder => {
                            command.arg("--extract-folder");
                        }
                        Kind::ExtractTo => {
                            command.arg("--extract-to");
                        }
                        _ => {}
                    }
                    command.arg(&paths[0]).spawn().context("无法启动 Hx6Zip")?;
                    Ok(())
                }
                Kind::Root => bail!("请选择 Hx6Zip 子菜单中的操作"),
            }
        })();
        result.map_err(|error| show_error(&error))
    }

    fn GetFlags(&self) -> windows::core::Result<u32> {
        Ok(if self.kind == Kind::Root {
            ECF_HASSUBCOMMANDS.0 as u32
        } else {
            ECF_DEFAULT.0 as u32
        })
    }

    fn EnumSubCommands(&self) -> windows::core::Result<IEnumExplorerCommand> {
        if self.kind != Kind::Root {
            return Err(E_NOTIMPL.into());
        }
        Ok(enumerator(0))
    }
}

const SUBCOMMANDS: [Kind; 10] = [
    Kind::Zip,
    Kind::SevenZip,
    Kind::Open,
    Kind::ExtractHere,
    Kind::ExtractFolder,
    Kind::AddToArchive,
    Kind::ExtractTo,
    Kind::Split,
    Kind::Merge,
    Kind::EachZip,
];

#[implement(IEnumExplorerCommand, Agile = false)]
struct CommandEnumerator {
    position: Cell<usize>,
    _reference: ObjectReference,
}

fn enumerator(position: usize) -> IEnumExplorerCommand {
    CommandEnumerator {
        position: Cell::new(position),
        _reference: ObjectReference::new(),
    }
    .into()
}

impl IEnumExplorerCommand_Impl for CommandEnumerator_Impl {
    fn Next(
        &self,
        requested: u32,
        output: *mut Option<IExplorerCommand>,
        fetched: *mut u32,
    ) -> HRESULT {
        if (requested > 0 && output.is_null()) || (requested != 1 && fetched.is_null()) {
            return E_POINTER;
        }
        let position = self.position.get();
        let count = (requested as usize).min(SUBCOMMANDS.len() - position);
        for (index, kind) in SUBCOMMANDS[position..position + count].iter().enumerate() {
            unsafe { output.add(index).write(Some(command(*kind))) };
        }
        self.position.set(position + count);
        if !fetched.is_null() {
            unsafe { fetched.write(count as u32) };
        }
        if count == requested as usize {
            S_OK
        } else {
            S_FALSE
        }
    }

    fn Skip(&self, requested: u32) -> windows::core::Result<()> {
        let position = self.position.get();
        let count = (requested as usize).min(SUBCOMMANDS.len() - position);
        self.position.set(position + count);
        if count == requested as usize {
            Ok(())
        } else {
            Err(Error::from_hresult(S_FALSE))
        }
    }

    fn Reset(&self) -> windows::core::Result<()> {
        self.position.set(0);
        Ok(())
    }

    fn Clone(&self) -> windows::core::Result<IEnumExplorerCommand> {
        Ok(enumerator(self.position.get()))
    }
}

#[implement(IClassFactory, Agile = false)]
struct CommandFactory {
    kind: Kind,
    _reference: ObjectReference,
}

impl IClassFactory_Impl for CommandFactory_Impl {
    fn CreateInstance(
        &self,
        outer: Ref<'_, IUnknown>,
        iid: *const GUID,
        output: *mut *mut c_void,
    ) -> windows::core::Result<()> {
        if iid.is_null() || output.is_null() {
            return Err(E_POINTER.into());
        }
        unsafe { *output = std::ptr::null_mut() };
        if outer.is_some() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        unsafe { command(self.kind).query(iid, output).ok() }
    }

    fn LockServer(&self, lock: BOOL) -> windows::core::Result<()> {
        if lock.as_bool() {
            LOCKS.fetch_add(1, Ordering::SeqCst);
        } else if LOCKS
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                count.checked_sub(1)
            })
            .is_err()
        {
            return Err(E_INVALIDARG.into());
        }
        Ok(())
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn DllMain(instance: HINSTANCE, reason: u32, _: *mut c_void) -> BOOL {
    if reason == DLL_PROCESS_ATTACH {
        MODULE.store(instance.0 as isize, Ordering::SeqCst);
    }
    BOOL(1)
}

/// # Safety
/// The caller must pass valid COM pointers for `class`, `iid`, and `output`.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllGetClassObject(
    class: *const GUID,
    iid: *const GUID,
    output: *mut *mut c_void,
) -> HRESULT {
    if class.is_null() || iid.is_null() || output.is_null() {
        return E_POINTER;
    }
    unsafe { *output = std::ptr::null_mut() };
    let kind = match unsafe { *class } {
        ROOT_ID => Kind::Root,
        SPLIT_ID => Kind::Split,
        MERGE_ID => Kind::Merge,
        EACH_ZIP_ID => Kind::EachZip,
        _ => return CLASS_E_CLASSNOTAVAILABLE,
    };
    let factory: IClassFactory = CommandFactory {
        kind,
        _reference: ObjectReference::new(),
    }
    .into();
    unsafe { factory.query(iid, output) }
}

#[unsafe(no_mangle)]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    if OBJECTS.load(Ordering::SeqCst) == 0 && LOCKS.load(Ordering::SeqCst) == 0 {
        S_OK
    } else {
        S_FALSE
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn archive_filter_matches_supported_extensions() {
        for extension in hx6zip_shell_protocol::SUPPORTED_EXTENSIONS {
            assert!(can_attempt_extraction(Path::new(&format!(
                "example.{}",
                extension.to_ascii_uppercase()
            ))));
        }
        assert!(!can_attempt_extraction(Path::new("example.txt")));
        assert!(can_attempt_extraction(Path::new("example")));
        assert!(can_attempt_extraction(Path::new("example.EXE")));
    }

    #[test]
    fn enumerator_returns_all_children_and_clones_its_position() {
        let root = command(Kind::Root);
        unsafe {
            assert_eq!(root.GetFlags().unwrap(), ECF_HASSUBCOMMANDS.0 as u32);
            let children = root.EnumSubCommands().unwrap();
            let mut first = [None];
            assert_eq!(children.Next(&mut first, None), S_OK);
            assert_eq!(
                first[0].as_ref().unwrap().GetCanonicalName().unwrap(),
                ZIP_ID
            );
            let copy = children.Clone().unwrap();
            let mut remaining = [None, None, None, None, None, None, None, None, None, None];
            let mut fetched = 0;
            assert_eq!(children.Next(&mut remaining, Some(&mut fetched)), S_FALSE);
            assert_eq!(fetched, 9);
            assert_eq!(
                remaining[0].as_ref().unwrap().GetCanonicalName().unwrap(),
                SEVENZIP_ID
            );
            assert_eq!(
                remaining[1].as_ref().unwrap().GetCanonicalName().unwrap(),
                OPEN_ID
            );
            assert_eq!(
                remaining[2].as_ref().unwrap().GetCanonicalName().unwrap(),
                EXTRACT_HERE_ID
            );
            assert_eq!(
                remaining[3].as_ref().unwrap().GetCanonicalName().unwrap(),
                EXTRACT_FOLDER_ID
            );
            assert_eq!(
                remaining[4].as_ref().unwrap().GetCanonicalName().unwrap(),
                ADD_TO_ARCHIVE_ID
            );
            assert_eq!(
                remaining[5].as_ref().unwrap().GetCanonicalName().unwrap(),
                EXTRACT_TO_ID
            );
            assert_eq!(
                remaining[6].as_ref().unwrap().GetCanonicalName().unwrap(),
                SPLIT_ID
            );
            assert_eq!(
                remaining[7].as_ref().unwrap().GetCanonicalName().unwrap(),
                MERGE_ID
            );
            assert_eq!(
                remaining[8].as_ref().unwrap().GetCanonicalName().unwrap(),
                EACH_ZIP_ID
            );
            let mut copied = [None];
            assert_eq!(copy.Next(&mut copied, None), S_OK);
            assert_eq!(
                copied[0].as_ref().unwrap().GetCanonicalName().unwrap(),
                SEVENZIP_ID
            );
            copy.Reset().unwrap();
            assert_eq!(copy.Next(&mut copied, None), S_OK);
            assert_eq!(
                copied[0].as_ref().unwrap().GetCanonicalName().unwrap(),
                ZIP_ID
            );
        }
    }
}
