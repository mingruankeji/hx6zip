// UnRAR source code may be used in any software to handle RAR archives without
// limitations free of charge, but cannot be used to develop RAR (WinRAR)
// compatible archiver and to re-create RAR compression algorithm, which is
// proprietary. Distribution of modified UnRAR source code in separate form
// or as a part of other software is permitted, provided that full text of this
// paragraph, starting from "UnRAR source code" words, is included in license, or
// in documentation if license is not available, and in source code comments of
// resulting package.

use std::{
    io::Write,
    path::Path,
    ptr::{self, NonNull},
    sync::{Mutex, MutexGuard},
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use unrar_ng_sys as native;

use super::{ArchiveEntry, JobControl, MAX_ARCHIVE_ENTRIES, timestamps::windows_time};

static RAR_LOCK: Mutex<()> = Mutex::new(());
const MAX_RAR_NAME_UNITS: usize = 32768;
const MAX_RAR_DICTIONARY_KIB: u32 = 256 * 1024;

#[cfg(windows)]
fn path_units(path: &Path) -> Vec<native::WCHAR> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().collect()
}

#[cfg(not(windows))]
fn path_units(path: &Path) -> Vec<native::WCHAR> {
    path.to_string_lossy()
        .chars()
        .map(|value| value as _)
        .collect()
}

#[cfg(windows)]
fn text_units(text: &str) -> Vec<native::WCHAR> {
    text.encode_utf16().collect()
}

#[cfg(not(windows))]
fn text_units(text: &str) -> Vec<native::WCHAR> {
    text.chars().map(|value| value as _).collect()
}

#[cfg(windows)]
fn name_from_units(units: &[native::WCHAR]) -> Result<String> {
    String::from_utf16(units).context("RAR 文件名不是有效的 Unicode")
}

#[cfg(not(windows))]
fn name_from_units(units: &[native::WCHAR]) -> Result<String> {
    units
        .iter()
        .map(|&unit| char::from_u32(unit as u32).context("RAR 文件名不是有效的 Unicode"))
        .collect()
}

struct CallbackState<'a, 'b> {
    control: &'a JobControl,
    password: Option<&'a [native::WCHAR]>,
    output: Option<&'b mut (dyn Write + 'b)>,
    expected: Option<u64>,
    written: u64,
    error: Option<anyhow::Error>,
}

extern "C" fn callback(
    message: native::UINT,
    user_data: native::LPARAM,
    parameter1: native::LPARAM,
    parameter2: native::LPARAM,
) -> i32 {
    if user_data == 0 {
        return -1;
    }
    // UnRAR invokes this callback synchronously while the stack-owned state is registered.
    let state = unsafe { &mut *(user_data as *mut CallbackState<'_, '_>) };
    if state.error.is_some() {
        return -1;
    }
    let result = (|| -> Result<i32> {
        state.control.check_cancelled()?;
        match message {
            native::UCM_NEEDPASSWORDW => {
                let password = state.password.ok_or(super::password::PasswordError::Required)?;
                let capacity = usize::try_from(parameter2).context("RAR 密码缓冲区无效")?;
                ensure!(
                    parameter1 != 0 && password.len() < capacity,
                    "RAR 密码过长或包含无效字符"
                );
                unsafe {
                    let destination = parameter1 as *mut native::WCHAR;
                    ptr::copy_nonoverlapping(password.as_ptr(), destination, password.len());
                    destination.add(password.len()).write(0);
                }
                Ok(0)
            }
            native::UCM_NEEDPASSWORD if state.password.is_none() => {
                Err(super::password::PasswordError::Required.into())
            }
            native::UCM_NEEDPASSWORD => bail!("此 RAR 密码编码不受支持"),
            native::UCM_CHANGEVOLUME | native::UCM_CHANGEVOLUMEW
                if parameter2 == native::RAR_VOL_ASK =>
            {
                bail!("暂不支持分卷 RAR 压缩包")
            }
            native::UCM_PROCESSDATA => {
                let count = usize::try_from(parameter2).context("RAR 数据块大小无效")?;
                ensure!(parameter1 != 0 || count == 0, "RAR 数据块无效");
                state.written = state
                    .written
                    .checked_add(count as u64)
                    .context("RAR 文件大小溢出")?;
                if let Some(expected) = state.expected {
                    ensure!(state.written <= expected, "RAR 文件内容超过声明大小");
                }
                if let Some(output) = state.output.as_deref_mut()
                    && count != 0
                {
                    let bytes =
                        unsafe { std::slice::from_raw_parts(parameter1 as *const u8, count) };
                    output.write_all(bytes).context("无法写入 RAR 文件内容")?;
                }
                Ok(0)
            }
            _ => Ok(0),
        }
    })();
    match result {
        Ok(value) => value,
        Err(error) => {
            state.error = Some(error);
            -1
        }
    }
}

fn check_code(code: i32, action: &str) -> Result<()> {
    if code == native::ERAR_SUCCESS {
        return Ok(());
    }
    if code == native::ERAR_MISSING_PASSWORD {
        return Err(super::password::PasswordError::Required).context(action.to_owned());
    }
    if code == native::ERAR_BAD_PASSWORD {
        return Err(super::password::PasswordError::Incorrect).context(action.to_owned());
    }
    let reason = match code {
        native::ERAR_NO_MEMORY => "内存不足",
        native::ERAR_BAD_DATA => "数据损坏或校验失败",
        native::ERAR_BAD_ARCHIVE => "压缩包损坏",
        native::ERAR_UNKNOWN_FORMAT => "RAR 格式不受支持",
        native::ERAR_EOPEN => "无法打开文件或缺少分卷",
        native::ERAR_EREAD => "读取失败",
        native::ERAR_EWRITE => "写入失败",
        native::ERAR_MISSING_PASSWORD => "需要密码",
        native::ERAR_BAD_PASSWORD => "密码不正确",
        native::ERAR_LARGE_DICT => "压缩字典过大",
        _ => "RAR 操作失败",
    };
    bail!("{action}：{reason}（错误代码 {code}）")
}

pub(super) struct Reader<'a> {
    handle: NonNull<native::Handle>,
    password: Option<Vec<native::WCHAR>>,
    control: &'a JobControl,
    solid: bool,
    _lock: MutexGuard<'static, ()>,
}

impl<'a> Reader<'a> {
    pub(super) fn open(
        path: &Path,
        password: Option<&str>,
        control: &'a JobControl,
        extract: bool,
    ) -> Result<Self> {
        control.check_cancelled()?;
        let lock = RAR_LOCK.lock().map_err(|_| anyhow!("RAR 解码器状态异常"))?;
        let mut path = path_units(path);
        ensure!(!path.contains(&0), "RAR 路径包含空字符");
        path.push(0);
        let password = password.filter(|value| !value.is_empty()).map(text_units);
        ensure!(
            password.as_ref().is_none_or(|value| !value.contains(&0)),
            "RAR 密码包含空字符"
        );
        let mut state = CallbackState {
            control,
            password: password.as_deref(),
            output: None,
            expected: None,
            written: 0,
            error: None,
        };
        let mode = if extract {
            native::RAR_OM_EXTRACT
        } else {
            native::RAR_OM_LIST
        };
        let mut data = native::OpenArchiveDataEx::new(path.as_ptr(), mode);
        data.callback = Some(callback);
        data.user_data = &mut state as *mut _ as native::LPARAM;
        let handle = unsafe { native::RAROpenArchiveEx(&raw mut data) };
        if !handle.is_null() {
            unsafe { native::RARSetCallback(handle, None, 0) };
        }
        let error = state.error.take();
        let code = data.open_result as i32;
        if error.is_some() || code != native::ERAR_SUCCESS {
            if !handle.is_null() {
                unsafe { native::RARCloseArchive(handle) };
            }
            if let Some(error) = error {
                return Err(error).context("无法打开 RAR 压缩包");
            }
            check_code(code, "无法打开 RAR 压缩包")?;
        }
        let handle = NonNull::new(handle.cast_mut()).context("无法打开 RAR 压缩包")?;
        if data.flags & native::ROADF_VOLUME != 0 {
            unsafe { native::RARCloseArchive(handle.as_ptr()) };
            bail!("暂不支持分卷 RAR 压缩包");
        }
        Ok(Self {
            handle,
            password,
            control,
            solid: data.flags & native::ROADF_SOLID != 0,
            _lock: lock,
        })
    }

    pub(super) fn is_solid(&self) -> bool {
        self.solid
    }

    fn call<'b>(
        &'b mut self,
        output: Option<&'b mut (dyn Write + 'b)>,
        expected: Option<u64>,
        action: impl FnOnce(*const native::Handle) -> i32,
    ) -> Result<(i32, u64)> {
        self.control.check_cancelled()?;
        let mut state = CallbackState {
            control: self.control,
            password: self.password.as_deref(),
            output,
            expected,
            written: 0,
            error: None,
        };
        let handle = self.handle.as_ptr();
        unsafe {
            native::RARSetCallback(
                handle,
                Some(callback),
                &mut state as *mut _ as native::LPARAM,
            );
        }
        let result = action(handle);
        unsafe { native::RARSetCallback(handle, None, 0) };
        if let Some(error) = state.error {
            return Err(error);
        }
        Ok((result, state.written))
    }

    pub(super) fn next(&mut self, index: usize) -> Result<Option<ArchiveEntry>> {
        let mut name = vec![0 as native::WCHAR; MAX_RAR_NAME_UNITS];
        let mut header = native::HeaderDataEx {
            file_name_ex: name.as_mut_ptr(),
            file_name_ex_size: name.len() as u32,
            ..Default::default()
        };
        let (code, _) = self.call(None, None, |handle| unsafe {
            native::RARReadHeaderEx(handle, &raw mut header)
        })?;
        if code == native::ERAR_END_ARCHIVE {
            return Ok(None);
        }
        check_code(code, "无法读取 RAR 条目")?;
        ensure!(
            header.flags & (native::RHDF_SPLITBEFORE | native::RHDF_SPLITAFTER) == 0,
            "暂不支持分卷 RAR 压缩包"
        );
        ensure!(
            header.dict_size <= MAX_RAR_DICTIONARY_KIB,
            "RAR 压缩字典超过 256 MiB 安全限制"
        );
        let end = name
            .iter()
            .position(|value| *value == 0)
            .context("RAR 文件名过长")?;
        ensure!(end < MAX_RAR_NAME_UNITS - 1, "RAR 文件名过长");
        let name = name_from_units(&name[..end])?;
        let size = (u64::from(header.unp_size_high) << 32) | u64::from(header.unp_size);
        let compressed_size =
            (u64::from(header.pack_size_high) << 32) | u64::from(header.pack_size);
        let is_directory = header.flags & native::RHDF_DIRECTORY != 0;
        let unix_kind = header.file_attr & 0o170000;
        let unsupported_kind = header.redir_type != 0
            || (header.host_os == 3
                && unix_kind != 0
                && unix_kind != 0o100000
                && unix_kind != 0o040000)
            || (header.host_os == 2 && header.file_attr & 0x400 != 0);
        let modified = (u64::from(header.mtime_high) << 32) | u64::from(header.mtime_low);
        Ok(Some(ArchiveEntry {
            index,
            name,
            size,
            compressed_size: Some(compressed_size),
            is_directory,
            is_encrypted: Some(header.flags & native::RHDF_ENCRYPTED != 0),
            unsupported_kind,
            is_symbolic_link: header.host_os == 3 && unix_kind == 0o120000,
            modified: if modified == 0 {
                None
            } else {
                Some(windows_time(modified)?)
            },
        }))
    }

    pub(super) fn skip(&mut self) -> Result<()> {
        let (code, _) = self.call(None, None, |handle| unsafe {
            native::RARProcessFileW(handle, native::RAR_SKIP, ptr::null(), ptr::null())
        })?;
        check_code(code, "无法跳过 RAR 条目")
    }

    pub(super) fn process<'b>(
        &'b mut self,
        expected: u64,
        output: Option<&'b mut (dyn Write + 'b)>,
    ) -> Result<()> {
        let (code, written) = self.call(output, Some(expected), |handle| unsafe {
            native::RARProcessFileW(handle, native::RAR_TEST, ptr::null(), ptr::null())
        })?;
        check_code(code, "无法解压 RAR 文件")?;
        ensure!(written == expected, "RAR 文件内容大小与声明不符");
        Ok(())
    }
}

impl Drop for Reader<'_> {
    fn drop(&mut self) {
        unsafe { native::RARCloseArchive(self.handle.as_ptr()) };
    }
}

pub(super) fn inspect(
    path: &Path,
    password: Option<&str>,
    control: &JobControl,
) -> Result<Vec<ArchiveEntry>> {
    let mut reader = Reader::open(path, password, control, false)?;
    let mut entries = Vec::new();
    while let Some(entry) = reader.next(entries.len())? {
        ensure!(
            entries.len() < MAX_ARCHIVE_ENTRIES,
            "压缩包条目超过安全限制（{MAX_ARCHIVE_ENTRIES}）"
        );
        entries.push(entry);
        reader.skip()?;
    }
    Ok(entries)
}
