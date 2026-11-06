use std::{
    cell::{Cell, RefCell},
    ffi::c_void,
    rc::Rc,
};

use anyhow::{Context, Result, bail, ensure};
pub use mzip_shell_protocol::read_sources;
use mzip_shell_protocol::{CompressionAction, launch_compression};
use windows::{
    Win32::{
        Foundation::{CLASS_E_NOAGGREGATION, E_FAIL, E_INVALIDARG, E_POINTER, POINT},
        System::Com::{
            CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED, CoAddRefServerProcess, CoInitializeEx,
            CoRegisterClassObject, CoReleaseServerProcess, CoResumeClassObjects,
            CoRevokeClassObject, CoUninitialize, IClassFactory, IClassFactory_Impl,
            REGCLS_MULTIPLEUSE, REGCLS_SUSPENDED,
        },
        UI::{
            Shell::{
                IExecuteCommand, IExecuteCommand_Impl, IObjectWithSelection,
                IObjectWithSelection_Impl, IShellItemArray,
            },
            WindowsAndMessaging::{
                DispatchMessageW, GetMessageW, KillTimer, MB_ICONERROR, MB_OK, MSG, MessageBoxW,
                PostQuitMessage, SetTimer, TranslateMessage, WM_TIMER,
            },
        },
    },
    core::{BOOL, Error, GUID, IUnknown, Interface, PCWSTR, Ref, implement, w},
};

pub const ZIP_COMMAND: GUID = GUID::from_u128(0xdc99db75_8a46_4d65_9f25_329fdfcb0c60);
pub const SEVENZIP_COMMAND: GUID = GUID::from_u128(0xc58ad8e4_e793_4374_a2d2_76054397c012);
pub const CONFIGURE_COMMAND: GUID = GUID::from_u128(0x1252546f_25da_49cc_b862_a443ba0d0e15);
pub const EACH_ZIP_COMMAND: GUID = GUID::from_u128(0xf4774d42_41e4_4ac8_b964_9a37688ae104);

fn launch_selection(selection: &IShellItemArray, action: CompressionAction) -> Result<()> {
    let executable = std::env::current_exe().context("无法定位 MZip 程序")?;
    launch_compression(&executable, selection, action)
}

struct ServerReference;

impl ServerReference {
    fn new() -> Self {
        unsafe { CoAddRefServerProcess() };
        Self
    }
}

impl Drop for ServerReference {
    fn drop(&mut self) {
        unsafe {
            if CoReleaseServerProcess() == 0 {
                PostQuitMessage(0);
            }
        }
    }
}

// Keep callbacks and final Release on the STA owning the message loop.
#[implement(IExecuteCommand, IObjectWithSelection, Agile = false)]
struct ShellCommand {
    action: CompressionAction,
    selection: RefCell<Option<IShellItemArray>>,
    executed: Cell<bool>,
    _reference: ServerReference,
}

impl IObjectWithSelection_Impl for ShellCommand_Impl {
    fn SetSelection(&self, selection: Ref<'_, IShellItemArray>) -> windows::core::Result<()> {
        *self.selection.borrow_mut() = Some(
            selection
                .as_ref()
                .ok_or_else(|| Error::from(E_POINTER))?
                .clone(),
        );
        Ok(())
    }

    fn GetSelection(
        &self,
        iid: *const GUID,
        output: *mut *mut c_void,
    ) -> windows::core::Result<()> {
        if iid.is_null() || output.is_null() {
            return Err(E_POINTER.into());
        }
        unsafe { *output = std::ptr::null_mut() };
        let selection = self.selection.borrow();
        let selection = selection.as_ref().ok_or_else(|| Error::from(E_FAIL))?;
        unsafe { selection.query(iid, output).ok() }
    }
}

impl IExecuteCommand_Impl for ShellCommand_Impl {
    fn SetKeyState(&self, _: u32) -> windows::core::Result<()> {
        Ok(())
    }
    fn SetParameters(&self, _: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }
    fn SetPosition(&self, _: &POINT) -> windows::core::Result<()> {
        Ok(())
    }
    fn SetShowWindow(&self, _: i32) -> windows::core::Result<()> {
        Ok(())
    }
    fn SetNoShowUI(&self, _: BOOL) -> windows::core::Result<()> {
        Ok(())
    }
    fn SetDirectory(&self, _: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }

    fn Execute(&self) -> windows::core::Result<()> {
        if self.executed.replace(true) {
            return Err(E_INVALIDARG.into());
        }
        let result = (|| {
            let selection = self.selection.borrow();
            let selection = selection.as_ref().context("资源管理器未传入选择列表")?;
            launch_selection(selection, self.action)
        })();
        result.map_err(|error| {
            let message = format!("无法启动右键压缩：{error:#}");
            let wide: Vec<_> = message.encode_utf16().chain(Some(0)).collect();
            unsafe {
                MessageBoxW(
                    None,
                    PCWSTR(wide.as_ptr()),
                    w!("MZip"),
                    MB_OK | MB_ICONERROR,
                )
            };
            Error::new(E_FAIL, message)
        })
    }
}

#[implement(IClassFactory, Agile = false)]
struct CommandFactory {
    action: CompressionAction,
    activated: Rc<Cell<bool>>,
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
        self.activated.set(true);
        let command: IExecuteCommand = ShellCommand {
            action: self.action,
            selection: RefCell::new(None),
            executed: Cell::new(false),
            _reference: ServerReference::new(),
        }
        .into();
        unsafe { command.query(iid, output).ok() }
    }

    fn LockServer(&self, lock: BOOL) -> windows::core::Result<()> {
        if lock.as_bool() {
            unsafe { CoAddRefServerProcess() };
        } else {
            drop(ServerReference);
        }
        Ok(())
    }
}

struct Apartment;
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

struct Registration(u32);
impl Drop for Registration {
    fn drop(&mut self) {
        if let Err(error) = unsafe { CoRevokeClassObject(self.0) } {
            eprintln!("MZip: unable to revoke Shell class: {error}");
        }
    }
}

pub fn run_server() -> Result<()> {
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()? };
    let _apartment = Apartment;
    let activated = Rc::new(Cell::new(false));
    let mut registrations = Vec::new();
    for (id, action) in [
        (ZIP_COMMAND, CompressionAction::Zip),
        (SEVENZIP_COMMAND, CompressionAction::SevenZip),
        (CONFIGURE_COMMAND, CompressionAction::Configure),
        (EACH_ZIP_COMMAND, CompressionAction::EachZip),
    ] {
        let factory: IClassFactory = CommandFactory {
            action,
            activated: activated.clone(),
        }
        .into();
        registrations.push(Registration(unsafe {
            CoRegisterClassObject(
                &id,
                &factory,
                CLSCTX_LOCAL_SERVER,
                REGCLS_MULTIPLEUSE | REGCLS_SUSPENDED,
            )?
        }));
    }
    // Abandoned activation must not leave an idle server behind.
    let timer = unsafe { SetTimer(None, 0, 30_000, None) };
    ensure!(timer != 0, "无法创建 Shell 启动超时计时器");
    let result = (|| {
        unsafe { CoResumeClassObjects()? };
        let mut message = MSG::default();
        loop {
            let status = unsafe { GetMessageW(&mut message, None, 0, 0) }.0;
            if status == -1 {
                bail!("无法接收 Shell 消息：{}", Error::from_win32());
            }
            if status == 0 {
                break;
            }
            if message.message == WM_TIMER && message.wParam.0 == timer && !activated.get() {
                break;
            }
            unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        Ok(())
    })();
    unsafe { KillTimer(None, timer)? };
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use mzip_shell_protocol::encode_sources;
    use std::path::PathBuf;

    #[test]
    fn installer_uses_delegate_execute_for_separate_zip_and_removes_legacy_handler() {
        let installer = include_str!(r"..\installer\Package.wxs");
        let key = r"Software\Classes\AllFilesystemObjects\shell\MZip\shell\10.CreateEachZip";
        let section = installer
            .split_once(&format!(r#"Key="{key}">"#))
            .unwrap()
            .1
            .split("</RegistryKey>")
            .next()
            .unwrap();
        let class = format!("{{{EACH_ZIP_COMMAND:?}}}");
        assert!(section.contains(r#"Name="MUIVerb" Value="压缩每个到单独的压缩包中""#));
        assert!(section.contains(r#"Name="MultiSelectModel" Value="Player""#));
        assert!(section.contains(r#"Key="command""#));
        assert!(section.contains(&format!(r#"Name="DelegateExecute" Value="{class}""#)));
        assert!(!section.contains("ExplorerCommandHandler"));
        let server = installer
            .split_once(&format!(
                r#"Key="Software\Classes\CLSID\{class}\LocalServer32">"#
            ))
            .unwrap()
            .1
            .split("</RegistryKey>")
            .next()
            .unwrap();
        assert!(server.contains("--shell-server"));
        assert!(server.contains(r#"Name="ServerExecutable" Value="[INSTALLFOLDER]mzip.exe""#));
        assert!(installer.contains(&format!(
            r#"<RemoveRegistryValue Root="HKCU" Key="{key}" Name="ExplorerCommandHandler" />"#
        )));
    }

    #[test]
    fn selection_round_trips_unicode_and_exceeds_command_line_limit() {
        let paths: Vec<_> = (0..2000)
            .map(|i| PathBuf::from(format!(r"C:\资料 文件夹\file {i}.txt")))
            .collect();
        let data = encode_sources(&paths).unwrap();
        assert!(data.len() > 65536);
        assert_eq!(read_sources(data.as_slice()).unwrap(), paths);
    }

    #[test]
    fn selection_rejects_empty_truncated_and_trailing_data() {
        assert!(encode_sources(&[]).is_err());
        let mut data = encode_sources(&[PathBuf::from(r"C:\test.txt")]).unwrap();
        assert!(read_sources(&data[..data.len() - 1]).is_err());
        data.push(0);
        assert!(read_sources(data.as_slice()).is_err());
    }
}
