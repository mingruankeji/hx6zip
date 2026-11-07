use std::{
    ffi::c_void,
    io::{Read, Seek, SeekFrom},
    sync::{Arc, Mutex},
};

use hx6zip::archive::ZipDragReader;
use windows::{
    Win32::{
        Foundation::{
            E_POINTER, S_FALSE, S_OK, STG_E_ACCESSDENIED, STG_E_INVALIDFLAG,
            STG_E_INVALIDFUNCTION, STG_E_MEDIUMFULL, STG_E_READFAULT, STG_E_SEEKERROR,
        },
        System::Com::{
            ISequentialStream_Impl, IStream, IStream_Impl, LOCKTYPE, STATFLAG, STATFLAG_DEFAULT,
            STATFLAG_NOOPEN, STATFLAG_NONAME, STATSTG, STGC, STGM_READ, STGTY_STREAM,
            STREAM_SEEK, STREAM_SEEK_CUR, STREAM_SEEK_END, STREAM_SEEK_SET,
        },
        UI::Shell::{FILEDESCRIPTORW, SHStrDupW},
    },
    core::{HRESULT, PCWSTR, Ref, implement},
};

use super::DragState;

pub(super) fn create(
    reader: ZipDragReader,
    state: Arc<Mutex<DragState>>,
    descriptor: FILEDESCRIPTORW,
) -> IStream {
    ZipStream {
        reader: Mutex::new(reader),
        state,
        descriptor,
    }
    .into()
}

#[implement(IStream)]
struct ZipStream {
    reader: Mutex<ZipDragReader>,
    state: Arc<Mutex<DragState>>,
    descriptor: FILEDESCRIPTORW,
}

impl ZipStream {
    fn read(&self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let result = self
            .reader
            .lock()
            .map_err(|_| std::io::Error::other("ZIP 拖放读取状态异常"))?
            .read(buffer);
        if let Err(error) = &result {
            let mut state = self.state.lock().expect("drag state poisoned");
            if state.error.is_none() {
                state.error = Some(error.to_string());
            }
        }
        result
    }
}

impl ISequentialStream_Impl for ZipStream_Impl {
    fn Read(&self, output: *mut c_void, length: u32, read: *mut u32) -> HRESULT {
        if let Some(read) = unsafe { read.as_mut() } {
            *read = 0;
        }
        if length == 0 {
            return S_OK;
        }
        if output.is_null() {
            return E_POINTER;
        }
        let buffer = unsafe { std::slice::from_raw_parts_mut(output.cast::<u8>(), length as usize) };
        let mut count = 0;
        while count < buffer.len() {
            match self.read(&mut buffer[count..]) {
                Ok(0) => break,
                Ok(bytes) => count += bytes,
                Err(_) => {
                    if let Some(read) = unsafe { read.as_mut() } {
                        *read = count as u32;
                    }
                    return STG_E_READFAULT;
                }
            }
        }
        if let Some(read) = unsafe { read.as_mut() } {
            *read = count as u32;
        }
        if count == buffer.len() { S_OK } else { S_FALSE }
    }

    fn Write(&self, _: *const c_void, _: u32, written: *mut u32) -> HRESULT {
        if let Some(written) = unsafe { written.as_mut() } {
            *written = 0;
        }
        STG_E_ACCESSDENIED
    }
}

impl IStream_Impl for ZipStream_Impl {
    fn Seek(
        &self,
        offset: i64,
        origin: STREAM_SEEK,
        position: *mut u64,
    ) -> windows::core::Result<()> {
        let seek = match origin {
            STREAM_SEEK_SET if offset >= 0 => SeekFrom::Start(offset as u64),
            STREAM_SEEK_CUR => SeekFrom::Current(offset),
            STREAM_SEEK_END => SeekFrom::End(offset),
            _ => return Err(STG_E_SEEKERROR.into()),
        };
        let result = self
            .reader
            .lock()
            .map_err(|_| windows::core::Error::from(STG_E_READFAULT))?
            .seek(seek)
            .map_err(|_| windows::core::Error::from(STG_E_SEEKERROR))?;
        if let Some(position) = unsafe { position.as_mut() } {
            *position = result;
        }
        Ok(())
    }

    fn SetSize(&self, _: u64) -> windows::core::Result<()> {
        Err(STG_E_ACCESSDENIED.into())
    }

    fn CopyTo(
        &self,
        destination: Ref<'_, IStream>,
        length: u64,
        read: *mut u64,
        written: *mut u64,
    ) -> windows::core::Result<()> {
        if let Some(read) = unsafe { read.as_mut() } {
            *read = 0;
        }
        if let Some(written) = unsafe { written.as_mut() } {
            *written = 0;
        }
        let destination = destination
            .as_ref()
            .ok_or_else(|| windows::core::Error::from(E_POINTER))?;
        let mut total_read = 0u64;
        let mut total_written = 0u64;
        let mut buffer = [0u8; 64 * 1024];
        while total_read < length {
            let requested = (length - total_read).min(buffer.len() as u64) as usize;
            let count = self
                .read(&mut buffer[..requested])
                .map_err(|_| windows::core::Error::from(STG_E_READFAULT))?;
            if count == 0 {
                break;
            }
            total_read += count as u64;
            if let Some(read) = unsafe { read.as_mut() } {
                *read = total_read;
            }
            let mut offset = 0;
            while offset < count {
                let mut bytes = 0;
                unsafe {
                    destination
                        .Write(
                            buffer[offset..count].as_ptr().cast(),
                            (count - offset) as u32,
                            Some(&mut bytes),
                        )
                        .ok()?;
                }
                if bytes == 0 || bytes as usize > count - offset {
                    return Err(STG_E_MEDIUMFULL.into());
                }
                offset += bytes as usize;
                total_written += u64::from(bytes);
                if let Some(written) = unsafe { written.as_mut() } {
                    *written = total_written;
                }
            }
        }
        Ok(())
    }

    fn Commit(&self, _: &STGC) -> windows::core::Result<()> {
        Ok(())
    }

    fn Revert(&self) -> windows::core::Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }

    fn LockRegion(&self, _: u64, _: u64, _: &LOCKTYPE) -> windows::core::Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }

    fn UnlockRegion(&self, _: u64, _: u64, _: u32) -> windows::core::Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }

    fn Stat(&self, output: *mut STATSTG, flags: &STATFLAG) -> windows::core::Result<()> {
        let output = unsafe { output.as_mut() }.ok_or_else(|| windows::core::Error::from(E_POINTER))?;
        if *flags != STATFLAG_DEFAULT && *flags != STATFLAG_NONAME && *flags != STATFLAG_NOOPEN {
            return Err(STG_E_INVALIDFLAG.into());
        }
        let reader = self
            .reader
            .lock()
            .map_err(|_| windows::core::Error::from(STG_E_READFAULT))?;
        let mut stat = STATSTG {
            r#type: STGTY_STREAM.0 as u32,
            cbSize: reader.size(),
            mtime: self.descriptor.ftLastWriteTime,
            grfMode: STGM_READ,
            ..Default::default()
        };
        if *flags != STATFLAG_NONAME {
            let name = reader.name().encode_utf16().chain(Some(0)).collect::<Vec<_>>();
            stat.pwcsName = unsafe { SHStrDupW(PCWSTR(name.as_ptr()))? };
        }
        *output = stat;
        Ok(())
    }

    fn Clone(&self) -> windows::core::Result<IStream> {
        let reader = self
            .reader
            .lock()
            .map_err(|_| windows::core::Error::from(STG_E_READFAULT))?
            .try_clone()
            .map_err(|error| {
                let mut state = self.state.lock().expect("drag state poisoned");
                if state.error.is_none() {
                    state.error = Some(format!("{error:#}"));
                }
                windows::core::Error::from(STG_E_READFAULT)
            })?;
        Ok(create(reader, self.state.clone(), self.descriptor))
    }
}
