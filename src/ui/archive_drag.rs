use std::{
    mem::{ManuallyDrop, offset_of, size_of},
    os::windows::ffi::OsStrExt,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result, bail};
use mzip::archive::{
    ArchiveEntry, ArchiveInfo, ArchiveKind, ArchiveTree, ExportedEntries, JobControl,
    ZipDragSource, archive_selection, drag_conflicts, export_entries, extract_drag_entries,
};
use windows::{
    Win32::{
        Foundation::{
            DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, DV_E_FORMATETC,
            DV_E_LINDEX, E_FAIL, E_NOTIMPL, E_POINTER, E_UNEXPECTED, FILETIME, GlobalFree, HWND,
            S_OK,
        },
        Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL},
        System::{
            Com::{
                DATADIR_GET, DVASPECT_CONTENT, FORMATETC, IAdviseSink, IDataObject,
                IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA, STGM_READ, STGM_SHARE_DENY_WRITE,
                STGMEDIUM, STGMEDIUM_0, StructuredStorage::CreateStreamOnHGlobal, TYMED_HGLOBAL,
                TYMED_ISTREAM,
            },
            DataExchange::RegisterClipboardFormatW,
            Memory::{GMEM_MOVEABLE, GMEM_ZEROINIT, GlobalAlloc, GlobalLock, GlobalUnlock},
            Ole::{
                DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_SCROLL, DoDragDrop, IDropSource,
                IDropSource_Impl, IDropSourceNotify, IDropSourceNotify_Impl, OleInitialize,
                OleUninitialize,
            },
            SystemServices::MODIFIERKEYS_FLAGS,
            Threading::{AttachThreadInput, GetCurrentThreadId},
        },
        UI::{
            Input::KeyboardAndMouse::{GetAsyncKeyState, VK_ESCAPE, VK_LBUTTON, VK_RBUTTON},
            Shell::{
                CFSTR_FILECONTENTS, CFSTR_FILEDESCRIPTORW, FD_ATTRIBUTES, FD_FILESIZE,
                FD_PROGRESSUI, FD_UNICODE, FD_WRITESTIME, FILEDESCRIPTORW, FILEGROUPDESCRIPTORW,
                SHCreateStdEnumFmtEtc, SHCreateStreamOnFileEx,
            },
            WindowsAndMessaging::{
                GetSystemMetrics, IDC_ARROW, LoadCursorW, MSG, PM_NOREMOVE, PeekMessageW,
                SM_SWAPBUTTON, SetCursor,
            },
        },
    },
    core::{BOOL, HRESULT, PCWSTR, Ref, implement},
};

mod direct_target;
mod zip_stream;

struct OleApartment;
impl Drop for OleApartment {
    fn drop(&mut self) {
        unsafe {
            OleUninitialize();
        }
    }
}

struct AttachedInput {
    worker_thread: u32,
    ui_thread: u32,
}

impl AttachedInput {
    fn new(ui_thread: u32) -> Result<Self> {
        let worker_thread = unsafe { GetCurrentThreadId() };
        let mut message = MSG::default();
        unsafe {
            let _ = PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE);
            AttachThreadInput(worker_thread, ui_thread, true)
                .ok()
                .context("无法连接 Windows 拖放输入")?;
        }
        Ok(Self {
            worker_thread,
            ui_thread,
        })
    }
}

impl Drop for AttachedInput {
    fn drop(&mut self) {
        unsafe {
            let _ = AttachThreadInput(self.worker_thread, self.ui_thread, false);
        }
    }
}

#[derive(Default)]
struct DragState {
    exported: Option<ExportedEntries>,
    zip: Option<ZipDragSource>,
    error: Option<String>,
}

struct VirtualEntry {
    index: usize,
    source: String,
    descriptor: FILEDESCRIPTORW,
}

fn virtual_entries(
    info: &ArchiveInfo,
    paths: &[String],
    control: &JobControl,
) -> Result<Vec<VirtualEntry>> {
    archive_selection(info, paths)?;
    let tree = ArchiveTree::new(info, control)?;
    let mut roots = paths
        .iter()
        .map(|path| path.replace('\\', "/"))
        .collect::<Vec<_>>();
    roots.sort_by_key(String::len);
    let mut selected = Vec::<String>::new();
    let mut entries = Vec::new();
    for root in roots {
        let root = root.trim_end_matches('/');
        let key = root.to_lowercase();
        if selected
            .iter()
            .any(|parent| key == *parent || key.starts_with(&format!("{parent}/")))
        {
            continue;
        }
        selected.push(key);
        let parent = root.rsplit_once('/').map_or("", |(parent, _)| parent);
        let entry = tree
            .children(parent)?
            .iter()
            .find(|entry| entry.name.eq_ignore_ascii_case(root))
            .context("拖放项目已不存在，请刷新压缩包")?;
        append_virtual_entry(
            info,
            &tree,
            entry,
            entry.name.rsplit('/').next().unwrap(),
            control,
            &mut entries,
        )?;
    }
    anyhow::ensure!(
        !entries.is_empty(),
        "没有可拖出的条目；ISO 符号链接不会提取"
    );
    Ok(entries)
}

fn append_virtual_entry(
    info: &ArchiveInfo,
    tree: &ArchiveTree,
    entry: &ArchiveEntry,
    name: &str,
    control: &JobControl,
    entries: &mut Vec<VirtualEntry>,
) -> Result<()> {
    control.check_cancelled()?;
    if info.skips_iso_symlink(entry) {
        return Ok(());
    }
    let mut descriptor = FILEDESCRIPTORW::default();
    let mut filename = descriptor.cFileName;
    let name = name.replace('/', "\\").encode_utf16().collect::<Vec<_>>();
    anyhow::ensure!(
        name.len() < filename.len(),
        "拖放路径过长，请使用“解压所选到”"
    );
    filename[..name.len()].copy_from_slice(&name);
    descriptor.cFileName = filename;
    descriptor.dwFlags = (FD_ATTRIBUTES.0 | FD_UNICODE.0 | FD_PROGRESSUI.0) as u32;
    descriptor.dwFileAttributes = if entry.is_directory {
        FILE_ATTRIBUTE_DIRECTORY.0
    } else {
        descriptor.dwFlags |= FD_FILESIZE.0 as u32;
        descriptor.nFileSizeHigh = (entry.size >> 32) as u32;
        descriptor.nFileSizeLow = entry.size as u32;
        FILE_ATTRIBUTE_NORMAL.0
    };
    if let Some(modified) = entry.modified {
        let epoch = SystemTime::UNIX_EPOCH - Duration::from_secs(11_644_473_600);
        let ticks = u64::try_from(modified.duration_since(epoch)?.as_nanos() / 100)?;
        descriptor.dwFlags |= FD_WRITESTIME.0 as u32;
        descriptor.ftLastWriteTime = FILETIME {
            dwLowDateTime: ticks as u32,
            dwHighDateTime: (ticks >> 32) as u32,
        };
    }
    let relative = String::from_utf16(&name)?;
    entries.push(VirtualEntry {
        index: entry.index,
        source: entry.name.clone(),
        descriptor,
    });
    if entry.is_directory {
        for child in tree.children(&entry.name)? {
            let child_name = child.name.rsplit('/').next().unwrap();
            append_virtual_entry(
                info,
                tree,
                child,
                &format!("{relative}\\{child_name}"),
                control,
                entries,
            )?;
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct DragFormats {
    descriptor: u16,
    contents: u16,
}

impl DragFormats {
    fn new() -> Result<Self> {
        let descriptor = unsafe { RegisterClipboardFormatW(CFSTR_FILEDESCRIPTORW) };
        let contents = unsafe { RegisterClipboardFormatW(CFSTR_FILECONTENTS) };
        anyhow::ensure!(
            descriptor != 0 && contents != 0,
            "无法注册 Windows 拖放格式"
        );
        Ok(Self {
            descriptor: u16::try_from(descriptor)?,
            contents: u16::try_from(contents)?,
        })
    }

    fn descriptor(self) -> FORMATETC {
        FORMATETC {
            cfFormat: self.descriptor,
            ..file_format(TYMED_HGLOBAL.0 as u32)
        }
    }

    fn contents(self, index: i32) -> FORMATETC {
        FORMATETC {
            cfFormat: self.contents,
            lindex: index,
            ..file_format(TYMED_ISTREAM.0 as u32)
        }
    }
}

#[implement(IDataObject)]
struct ArchiveData {
    info: ArchiveInfo,
    paths: Vec<String>,
    password: Option<String>,
    control: JobControl,
    state: Arc<Mutex<DragState>>,
    released: Arc<AtomicBool>,
    formats: DragFormats,
    entries: Vec<VirtualEntry>,
}

fn file_format(tymed: u32) -> FORMATETC {
    FORMATETC {
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed,
        ..Default::default()
    }
}

impl ArchiveData {
    fn descriptors(&self) -> windows::core::Result<STGMEDIUM> {
        let count =
            u32::try_from(self.entries.len()).map_err(|_| windows::core::Error::from(E_FAIL))?;
        let offset = offset_of!(FILEGROUPDESCRIPTORW, fgd);
        let bytes = self
            .entries
            .len()
            .checked_mul(size_of::<FILEDESCRIPTORW>())
            .and_then(|bytes| bytes.checked_add(offset))
            .ok_or_else(|| windows::core::Error::from(E_FAIL))?;
        unsafe {
            let memory = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes)?;
            let buffer = GlobalLock(memory).cast::<u8>();
            if buffer.is_null() {
                let error = windows::core::Error::from_win32();
                let _ = GlobalFree(Some(memory));
                return Err(error);
            }
            buffer.cast::<u32>().write_unaligned(count);
            for (index, entry) in self.entries.iter().enumerate() {
                buffer
                    .add(offset + index * size_of::<FILEDESCRIPTORW>())
                    .cast::<FILEDESCRIPTORW>()
                    .write_unaligned(entry.descriptor);
            }
            // GlobalUnlock returns false on the successful final unlock as well.
            let _ = GlobalUnlock(memory);
            Ok(STGMEDIUM {
                tymed: TYMED_HGLOBAL.0 as u32,
                u: STGMEDIUM_0 { hGlobal: memory },
                ..Default::default()
            })
        }
    }

    fn contents(&self, index: usize) -> Result<STGMEDIUM> {
        self.control.check_cancelled()?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("拖放状态异常"))?;
        if let Some(error) = &state.error {
            bail!("{error}");
        }
        let entry = &self.entries[index];
        let stream = if entry.descriptor.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0 {
            unsafe { CreateStreamOnHGlobal(Default::default(), true)? }
        } else if self.info.format == ArchiveKind::Zip {
            if state.zip.is_none() {
                state.zip = Some(ZipDragSource::new(
                    &self.info,
                    &self.paths,
                    self.password.as_deref(),
                    &self.control,
                )?);
            }
            let source = state.zip.as_ref().context("ZIP 拖放数据未准备")?.clone();
            drop(state);
            zip_stream::create(
                source.open(entry.index)?,
                self.state.clone(),
                entry.descriptor,
            )
        } else {
            if state.exported.is_none() {
                state.exported = Some(export_entries(
                    &self.info,
                    &self.paths,
                    self.password.as_deref(),
                    &self.control,
                )?);
            }
            let exported = state.exported.as_ref().context("拖放文件未提取")?;
            let path = exported.directory.path().join(&entry.source);
            let wide = path
                .as_os_str()
                .encode_wide()
                .chain(Some(0))
                .collect::<Vec<_>>();
            unsafe {
                SHCreateStreamOnFileEx(
                    PCWSTR(wide.as_ptr()),
                    (STGM_READ | STGM_SHARE_DENY_WRITE).0,
                    0,
                    false,
                    None,
                )?
            }
        };
        Ok(STGMEDIUM {
            tymed: TYMED_ISTREAM.0 as u32,
            u: STGMEDIUM_0 {
                pstm: ManuallyDrop::new(Some(stream)),
            },
            ..Default::default()
        })
    }
}

impl IDataObject_Impl for ArchiveData_Impl {
    fn GetData(&self, format: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
        self.QueryGetData(format).ok()?;
        let format = unsafe { &*format };
        if format.cfFormat == self.formats.descriptor {
            return self.descriptors().inspect_err(|error| {
                self.state.lock().expect("drag state poisoned").error =
                    Some(format!("无法提供拖放文件列表：{error}"));
            });
        }
        let index = usize::try_from(format.lindex)
            .ok()
            .filter(|index| *index < self.entries.len())
            .ok_or_else(|| windows::core::Error::from(DV_E_LINDEX))?;
        // Shell targets may request data during DragEnter/DragOver, not just Drop.
        // Never decode while the mouse is held, and never advertise eager CF_HDROP.
        if !self.released.load(Ordering::Acquire) {
            return Err(E_UNEXPECTED.into());
        }
        match self.contents(index) {
            Ok(medium) => Ok(medium),
            Err(error) => {
                let mut state = self.state.lock().expect("drag state poisoned");
                if state.error.is_none() {
                    state.error = Some(format!("{error:#}"));
                }
                Err(E_FAIL.into())
            }
        }
    }

    fn QueryGetData(&self, format: *const FORMATETC) -> HRESULT {
        let Some(format) = (unsafe { format.as_ref() }) else {
            return E_POINTER;
        };
        if format.dwAspect != DVASPECT_CONTENT.0 {
            return DV_E_FORMATETC;
        }
        if format.cfFormat == self.formats.descriptor
            && format.lindex == -1
            && format.tymed & TYMED_HGLOBAL.0 as u32 != 0
        {
            S_OK
        } else if format.cfFormat == self.formats.contents
            && format.tymed & TYMED_ISTREAM.0 as u32 != 0
        {
            if format.lindex == -1
                || usize::try_from(format.lindex).is_ok_and(|index| index < self.entries.len())
            {
                S_OK
            } else {
                DV_E_LINDEX
            }
        } else {
            DV_E_FORMATETC
        }
    }

    fn EnumFormatEtc(&self, direction: u32) -> windows::core::Result<IEnumFORMATETC> {
        if direction != DATADIR_GET.0 as u32 {
            return Err(E_NOTIMPL.into());
        }
        unsafe { SHCreateStdEnumFmtEtc(&[self.formats.descriptor(), self.formats.contents(-1)]) }
    }

    fn GetDataHere(&self, _: *const FORMATETC, _: *mut STGMEDIUM) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn GetCanonicalFormatEtc(&self, _: *const FORMATETC, output: *mut FORMATETC) -> HRESULT {
        if let Some(output) = unsafe { output.as_mut() } {
            output.ptd = std::ptr::null_mut();
        }
        E_NOTIMPL
    }
    fn SetData(
        &self,
        _: *const FORMATETC,
        _: *const STGMEDIUM,
        _: BOOL,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn DAdvise(
        &self,
        _: *const FORMATETC,
        _: u32,
        _: Ref<'_, IAdviseSink>,
    ) -> windows::core::Result<u32> {
        Err(E_NOTIMPL.into())
    }
    fn DUnadvise(&self, _: u32) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
        Err(E_NOTIMPL.into())
    }
}

#[derive(Default)]
struct DirectDrag {
    enabled: bool,
    feedback_resolving: bool,
    names: Vec<PathBuf>,
    target: isize,
    effect: DROPEFFECT,
    destination: Option<PathBuf>,
}

#[implement(IDropSource, IDropSourceNotify)]
struct DragSource {
    control: JobControl,
    released: Arc<AtomicBool>,
    direct: Arc<Mutex<DirectDrag>>,
}

fn drag_button_down() -> bool {
    let button = if unsafe { GetSystemMetrics(SM_SWAPBUTTON) } != 0 {
        VK_RBUTTON
    } else {
        VK_LBUTTON
    };
    unsafe { GetAsyncKeyState(i32::from(button.0)) < 0 }
}

impl DragSource {
    fn claim_direct_drop(&self, target: isize, path: PathBuf, file_icon: bool) -> bool {
        let mut direct = self.direct.lock().expect("direct drag state poisoned");
        if !direct.enabled
            || direct.target != target
            || !(accepts_copy(direct.effect) || (file_icon && file_parent_effect(direct.effect)))
            || self.control.is_cancelled()
        {
            return false;
        }
        direct.destination = Some(path);
        self.released.store(false, Ordering::Release);
        true
    }

    fn continue_drag(&self, escape: bool, button_down: bool) -> HRESULT {
        if escape || self.control.is_cancelled() {
            self.released.store(false, Ordering::Release);
            DRAGDROP_S_CANCEL
        } else if !button_down {
            self.released.store(true, Ordering::Release);
            DRAGDROP_S_DROP
        } else {
            S_OK
        }
    }
}

impl IDropSource_Impl for DragSource_Impl {
    fn QueryContinueDrag(&self, escape: BOOL, _: MODIFIERKEYS_FLAGS) -> HRESULT {
        // This STA did not receive the original mouse-down. Its queued key state can
        // report "released" while the user is still holding the button on the UI thread.
        let escape = escape.as_bool() || escape_down();
        let button_down = drag_button_down();
        if !escape && !button_down && !self.control.is_cancelled() {
            let (enabled, target, effect, names) = {
                let direct = self.direct.lock().expect("direct drag state poisoned");
                (
                    direct.enabled,
                    direct.target,
                    direct.effect,
                    direct.names.clone(),
                )
            };
            if enabled {
                eprintln!(
                    "Direct drag release: target={target:#x}, effect={:#x}",
                    effect.0
                );
            }
            if enabled && (accepts_copy(effect) || file_parent_effect(effect)) {
                match direct_target::destination(HWND(target as *mut _), &names) {
                    Ok(Some(destination)) => {
                        if escape_down() || self.control.is_cancelled() {
                            return self.continue_drag(true, false);
                        }
                        if drag_button_down() {
                            return self.continue_drag(false, true);
                        }
                        // Cancel OLE before Drop/GetData so Explorer cannot also write the files.
                        if self.claim_direct_drop(
                            target,
                            destination.directory,
                            destination.file_icon,
                        ) {
                            eprintln!("Direct drag: taking over from Explorer");
                            return DRAGDROP_S_CANCEL;
                        }
                    }
                    Ok(None) => {
                        eprintln!("Direct drag: unconfirmed background; using Shell transfer")
                    }
                    Err(error) => eprintln!("Direct drag: using Shell transfer: {error:#}"),
                }
            }
        }
        self.continue_drag(escape || escape_down(), button_down)
    }

    fn GiveFeedback(&self, effect: DROPEFFECT) -> HRESULT {
        let mut direct = self.direct.lock().expect("direct drag state poisoned");
        if direct.enabled && direct.effect != effect {
            eprintln!("Direct drag feedback: {:#x}", effect.0);
        }
        direct.effect = effect;
        let (enabled, target, names) = (direct.enabled, direct.target, direct.names.clone());
        let preview = enabled && file_parent_effect(effect) && !direct.feedback_resolving;
        if preview {
            direct.feedback_resolving = true;
        }
        drop(direct);
        if preview {
            let result = direct_target::destination(HWND(target as *mut _), &names);
            self.direct
                .lock()
                .expect("direct drag state poisoned")
                .feedback_resolving = false;
            match result {
                Ok(Some(destination)) if destination.file_icon => {
                    match unsafe { LoadCursorW(None, IDC_ARROW) } {
                        Ok(cursor) => {
                            unsafe { SetCursor(Some(cursor)) };
                            return S_OK;
                        }
                        Err(error) => eprintln!("Direct drag: cannot display drop cursor: {error}"),
                    }
                }
                Ok(_) => {}
                Err(error) => eprintln!("Direct drag: cannot preview file drop: {error:#}"),
            }
        }
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

fn escape_down() -> bool {
    unsafe { GetAsyncKeyState(i32::from(VK_ESCAPE.0)) < 0 }
}

fn accepts_copy(effect: DROPEFFECT) -> bool {
    effect.0 & !DROPEFFECT_SCROLL.0 == DROPEFFECT_COPY.0
}

fn file_parent_effect(effect: DROPEFFECT) -> bool {
    effect.0 & !DROPEFFECT_SCROLL.0 == 0
}

impl IDropSourceNotify_Impl for DragSource_Impl {
    fn DragEnterTarget(&self, target: HWND) -> windows::core::Result<()> {
        let mut direct = self.direct.lock().expect("direct drag state poisoned");
        direct.target = target.0 as isize;
        direct.effect = DROPEFFECT::default();
        if direct.enabled {
            eprintln!("Direct drag: entered target {target:?}");
        }
        Ok(())
    }

    fn DragLeaveTarget(&self) -> windows::core::Result<()> {
        let mut direct = self.direct.lock().expect("direct drag state poisoned");
        direct.target = 0;
        direct.effect = DROPEFFECT::default();
        if direct.enabled {
            eprintln!("Direct drag: left target");
        }
        Ok(())
    }
}

#[derive(Default)]
pub(super) struct DragOutcome {
    pub dropped: bool,
    pub entries: Option<ExportedEntries>,
    pub destination: Option<PathBuf>,
}

pub(super) fn drag_archive(
    info: ArchiveInfo,
    paths: Vec<String>,
    password: Option<String>,
    control: JobControl,
    ui_thread: u32,
    dialog: super::overwrite::OverwriteDialog,
) -> Result<DragOutcome> {
    unsafe {
        OleInitialize(None).context("无法启动 Windows 文件拖放")?;
    }
    let _apartment = OleApartment;
    control.check_cancelled()?;
    let entries = virtual_entries(&info, &paths, &control)?;
    let formats = DragFormats::new()?;
    let _input = AttachedInput::new(ui_thread)?;
    control.check_cancelled()?;
    // Preparing a large selection must not start a new drag after the user has let go.
    if !drag_button_down() {
        return Ok(DragOutcome::default());
    }
    let state = Arc::new(Mutex::new(DragState::default()));
    let released = Arc::new(AtomicBool::new(false));
    let direct_request = (info.clone(), paths.clone(), password.clone());
    eprintln!("Archive drag: {:?}, direct enabled=true", info.format);
    let direct = Arc::new(Mutex::new(DirectDrag {
        enabled: true,
        names: entries
            .iter()
            .filter_map(|entry| {
                let name = entry.descriptor.cFileName;
                let len = name.iter().position(|c| *c == 0).unwrap_or(name.len());
                let name = String::from_utf16_lossy(&name[..len]);
                (!name.contains('\\')).then(|| PathBuf::from(name))
            })
            .collect(),
        ..Default::default()
    }));
    let object: IDataObject = ArchiveData {
        info,
        paths,
        password,
        control: control.clone(),
        state: state.clone(),
        released: released.clone(),
        formats,
        entries,
    }
    .into();
    let source: IDropSource = DragSource {
        control: control.clone(),
        released,
        direct: direct.clone(),
    }
    .into();
    let mut effect = DROPEFFECT::default();
    let result = unsafe { DoDragDrop(&object, &source, DROPEFFECT_COPY, &mut effect) };
    drop(object);
    drop(source);
    drop(_input);
    let mut state = state.lock().map_err(|_| anyhow::anyhow!("拖放状态异常"))?;
    if control.is_cancelled() {
        control.check_cancelled()?;
    }
    if let Some(error) = state.error.take() {
        bail!("{error}");
    }
    result.ok().context("Windows 文件拖放失败")?;
    let destination = direct
        .lock()
        .expect("direct drag state poisoned")
        .destination
        .take();
    if result == DRAGDROP_S_CANCEL
        && let Some(destination) = destination
    {
        drop(state);
        control.check_cancelled()?;
        if escape_down() {
            return Ok(DragOutcome::default());
        }
        let (info, paths, password) = direct_request;
        let conflicts = drag_conflicts(&info, &paths, &destination, &control)?;
        let overwrite = !conflicts.is_empty();
        if overwrite && !dialog.confirm(&conflicts)? {
            return Ok(DragOutcome::default());
        }
        control.check_cancelled()?;
        extract_drag_entries(
            &info,
            &paths,
            &destination,
            password.as_deref(),
            overwrite,
            &control,
        )?;
        return Ok(DragOutcome {
            dropped: true,
            entries: None,
            destination: Some(destination),
        });
    }
    if result == DRAGDROP_S_DROP && effect == DROPEFFECT_COPY {
        Ok(DragOutcome {
            dropped: true,
            entries: state.exported.take(),
            destination: None,
        })
    } else {
        Ok(DragOutcome::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mzip::archive::{
        ArchiveFormat, CancellationToken, CreateOptions, IntegrityMode, create_archive,
        inspect_archive,
    };
    use std::{fs, io::Write, path::Path};
    use windows::Win32::{
        Foundation::{POINTL, S_FALSE, STG_E_ACCESSDENIED},
        System::{
            Com::{
                COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize, IStream, STATFLAG_NONAME,
                STATSTG, STREAM_SEEK_CUR, STREAM_SEEK_SET,
            },
            Ole::{IDropTarget, ReleaseStgMedium},
            SystemServices::MK_LBUTTON,
        },
        UI::Shell::{BHID_SFUIObject, IShellItem, SHCreateItemFromParsingName},
    };

    #[test]
    fn copy_with_shell_scroll_flag_is_still_a_copy() {
        assert!(accepts_copy(DROPEFFECT_COPY));
        assert!(accepts_copy(DROPEFFECT_COPY | DROPEFFECT_SCROLL));
        assert!(!accepts_copy(DROPEFFECT::default()));
        assert!(!accepts_copy(DROPEFFECT_SCROLL));
        let direct = Arc::new(Mutex::new(DirectDrag {
            enabled: true,
            effect: DROPEFFECT_COPY | DROPEFFECT_SCROLL,
            ..Default::default()
        }));
        let source = DragSource {
            control: JobControl::default(),
            released: Arc::new(AtomicBool::new(false)),
            direct: direct.clone(),
        };
        assert!(source.claim_direct_drop(0, PathBuf::from(r"C:\probe"), false));
        direct.lock().unwrap().effect = DROPEFFECT::default();
        assert!(!source.claim_direct_drop(0, PathBuf::from(r"C:\probe"), false));
        assert!(source.claim_direct_drop(0, PathBuf::from(r"C:\probe"), true));
        direct.lock().unwrap().effect = DROPEFFECT_SCROLL;
        assert!(!source.claim_direct_drop(0, PathBuf::from(r"C:\probe"), false));
        assert!(source.claim_direct_drop(0, PathBuf::from(r"C:\probe"), true));
    }

    #[test]
    fn direct_drop_claim_requires_unchanged_accepting_target_and_never_releases_contents() {
        let token = CancellationToken::default();
        let direct = Arc::new(Mutex::new(DirectDrag::default()));
        let released = Arc::new(AtomicBool::new(false));
        let source = DragSource {
            control: JobControl::new(token.clone(), |_| {}),
            released: released.clone(),
            direct: direct.clone(),
        };
        let destination = PathBuf::from(r"C:\probe");
        assert!(!source.claim_direct_drop(1, destination.clone(), false));
        {
            let mut state = direct.lock().unwrap();
            state.enabled = true;
            state.target = 1;
        }
        assert!(!source.claim_direct_drop(1, destination.clone(), false));
        direct.lock().unwrap().effect = DROPEFFECT_COPY;
        assert!(!source.claim_direct_drop(2, destination.clone(), false));
        assert!(source.claim_direct_drop(1, destination.clone(), false));
        assert!(!released.load(Ordering::Acquire));
        assert_eq!(
            direct.lock().unwrap().destination.as_ref(),
            Some(&destination)
        );
        direct.lock().unwrap().destination = None;
        token.cancel();
        assert!(!source.claim_direct_drop(1, destination, false));
        assert!(direct.lock().unwrap().destination.is_none());
    }

    #[test]
    fn direct_drop_notifications_reset_stale_feedback_without_losing_claimed_destination() {
        use windows::core::Interface;
        let direct = Arc::new(Mutex::new(DirectDrag {
            enabled: true,
            ..Default::default()
        }));
        let source: IDropSource = DragSource {
            control: JobControl::default(),
            released: Arc::new(AtomicBool::new(false)),
            direct: direct.clone(),
        }
        .into();
        let notify: IDropSourceNotify = source.cast().unwrap();
        unsafe {
            notify.DragEnterTarget(HWND(1 as *mut _)).unwrap();
            source.GiveFeedback(DROPEFFECT_COPY).ok().unwrap();
            notify.DragEnterTarget(HWND(2 as *mut _)).unwrap();
        }
        assert_eq!(direct.lock().unwrap().target, 2);
        assert_eq!(direct.lock().unwrap().effect, DROPEFFECT::default());
        direct.lock().unwrap().destination = Some(PathBuf::from(r"C:\probe"));
        unsafe { notify.DragLeaveTarget().unwrap() };
        assert_eq!(direct.lock().unwrap().target, 0);
        assert!(direct.lock().unwrap().destination.is_some());
    }

    fn shell_target(destination: &Path) -> IDropTarget {
        fs::create_dir(destination).unwrap();
        let wide = destination
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        unsafe {
            let folder: IShellItem =
                SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None).unwrap();
            folder.BindToHandler(None, &BHID_SFUIObject).unwrap()
        }
    }

    #[test]
    fn virtual_iso_entries_omit_links_but_browsing_keeps_them() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("folder");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("hello.txt"), b"hello").unwrap();
        fs::write(source.join("alias"), b"unused").unwrap();
        let path = temp.path().join("test.zip");
        create_archive(
            &CreateOptions {
                sources: vec![source],
                destination: path.clone(),
                format: ArchiveFormat::Zip,
                password: None,
                overwrite: false,
            },
            &JobControl::default(),
        )
        .unwrap();
        let mut info = inspect_archive(&path, None, &JobControl::default()).unwrap();
        info.format = ArchiveKind::Iso;
        let link = info
            .entries
            .iter_mut()
            .find(|entry| entry.name == "folder/alias")
            .unwrap();
        link.is_symbolic_link = true;
        link.unsupported_kind = true;
        let control = JobControl::default();
        let tree = ArchiveTree::new(&info, &control).unwrap();
        assert_eq!(tree.children("folder").unwrap().len(), 2);
        let entries = virtual_entries(&info, &["folder".to_owned()], &control).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|entry| entry.source != "folder/alias"));
        assert!(virtual_entries(&info, &["folder/alias".to_owned()], &control).is_err());
    }

    #[test]
    fn native_drag_object_only_extracts_contents_after_release() {
        unsafe {
            OleInitialize(None).unwrap();
        }
        let _apartment = OleApartment;
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("folder");
        fs::create_dir_all(source.join("nested")).unwrap();
        fs::write(source.join("nested").join("hello.txt"), b"hello").unwrap();
        fs::write(source.join("unselected.txt"), b"do not export").unwrap();
        let path = temp.path().join("test.zip");
        create_archive(
            &CreateOptions {
                sources: vec![source],
                destination: path.clone(),
                format: ArchiveFormat::Zip,
                password: None,
                overwrite: false,
            },
            &JobControl::default(),
        )
        .unwrap();
        let info = inspect_archive(&path, None, &JobControl::default()).unwrap();
        let state = Arc::new(Mutex::new(DragState::default()));
        let released = Arc::new(AtomicBool::new(false));
        let formats = DragFormats::new().unwrap();
        let paths = vec!["folder/nested".into()];
        let entries = virtual_entries(&info, &paths, &JobControl::default()).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].source, "folder/nested");
        assert_eq!(entries[1].source, "folder/nested/hello.txt");
        let object: IDataObject = ArchiveData {
            info,
            paths,
            password: None,
            control: JobControl::default(),
            state: state.clone(),
            released: released.clone(),
            formats,
            entries,
        }
        .into();
        unsafe {
            object.QueryGetData(&formats.descriptor()).ok().unwrap();
            object.EnumFormatEtc(DATADIR_GET.0 as u32).unwrap();
            let hdrop = FORMATETC {
                cfFormat: 15,
                ..file_format(TYMED_HGLOBAL.0 as u32)
            };
            assert_eq!(object.QueryGetData(&hdrop), DV_E_FORMATETC);
            assert_eq!(object.GetData(&hdrop).err().unwrap().code(), DV_E_FORMATETC);
            let mut metadata = object.GetData(&formats.descriptor()).unwrap();
            let buffer = GlobalLock(metadata.u.hGlobal).cast::<u8>();
            assert!(!buffer.is_null());
            assert_eq!(buffer.cast::<u32>().read_unaligned(), 2);
            let file = buffer
                .add(offset_of!(FILEGROUPDESCRIPTORW, fgd) + size_of::<FILEDESCRIPTORW>())
                .cast::<FILEDESCRIPTORW>()
                .read_unaligned();
            let filename = file.cFileName;
            let len = filename.iter().position(|ch| *ch == 0).unwrap();
            assert_eq!(
                String::from_utf16(&filename[..len]).unwrap(),
                "nested\\hello.txt"
            );
            assert_eq!({ file.nFileSizeLow }, 5);
            let _ = GlobalUnlock(metadata.u.hGlobal);
            ReleaseStgMedium(&mut metadata);
            assert_eq!(
                object.GetData(&formats.contents(1)).err().unwrap().code(),
                E_UNEXPECTED
            );
            assert_eq!(
                object.GetData(&formats.contents(-1)).err().unwrap().code(),
                DV_E_LINDEX
            );
            assert_eq!(object.QueryGetData(&formats.contents(2)), DV_E_LINDEX);
            assert_eq!(object.QueryGetData(std::ptr::null()), E_POINTER);
        }
        assert!(state.lock().unwrap().exported.is_none());
        assert!(state.lock().unwrap().zip.is_none());
        assert!(state.lock().unwrap().error.is_none());
        let destination = temp.path().join("windows-destination");
        let target = shell_target(&destination);
        let mut effect = DROPEFFECT_COPY;
        let drop_source = DragSource {
            control: JobControl::default(),
            released,
            direct: Arc::default(),
        };
        unsafe {
            target
                .DragEnter(&object, MK_LBUTTON, POINTL::default(), &mut effect)
                .unwrap();
            assert_eq!(effect, DROPEFFECT_COPY);
            target
                .DragOver(MK_LBUTTON, POINTL::default(), &mut effect)
                .unwrap();
            assert_eq!(effect, DROPEFFECT_COPY);
            assert!(state.lock().unwrap().exported.is_none());
            assert!(state.lock().unwrap().zip.is_none());
            assert!(state.lock().unwrap().error.is_none());
            assert!(fs::read_dir(&destination).unwrap().next().is_none());
            assert_eq!(drop_source.continue_drag(false, true), S_OK);
            assert_eq!(drop_source.continue_drag(false, false), DRAGDROP_S_DROP);
            target
                .Drop(
                    &object,
                    MODIFIERKEYS_FLAGS(0),
                    POINTL::default(),
                    &mut effect,
                )
                .unwrap();
            assert_eq!(effect, DROPEFFECT_COPY);
            // Each request starts a fresh stream without materializing a temporary copy.
            let mut medium = object.GetData(&formats.contents(1)).unwrap();
            let stream = medium.u.pstm.as_ref().unwrap();
            let mut bytes = [0; 5];
            let mut read = 0;
            stream
                .Read(
                    bytes.as_mut_ptr().cast(),
                    bytes.len() as u32,
                    Some(&mut read),
                )
                .ok()
                .unwrap();
            assert_eq!(read, 5);
            assert_eq!(&bytes, b"hello");
            ReleaseStgMedium(&mut medium);
        }
        assert_eq!(
            fs::read(destination.join("nested").join("hello.txt")).unwrap(),
            b"hello"
        );
        assert!(!destination.join("unselected.txt").exists());
        let state = state.lock().unwrap();
        assert!(state.error.is_none());
        assert!(state.exported.is_none());
        assert!(state.zip.is_some());
    }

    #[test]
    fn native_drag_preserves_implicit_unicode_and_empty_directories() {
        unsafe { OleInitialize(None).unwrap() };
        let _apartment = OleApartment;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("test.zip");
        let mut zip = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        zip.add_directory(
            "folder/nested/empty/",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        zip.start_file(
            "folder/nested/文件.txt",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        zip.write_all(b"unicode").unwrap();
        zip.finish().unwrap();
        let info = inspect_archive(&path, None, &JobControl::default()).unwrap();
        let paths = vec!["folder/nested/文件.txt".into(), "folder/nested".into()];
        let entries = virtual_entries(&info, &paths, &JobControl::default()).unwrap();
        assert_eq!(entries.len(), 3);
        let state = Arc::new(Mutex::new(DragState::default()));
        let released = Arc::new(AtomicBool::new(false));
        let formats = DragFormats::new().unwrap();
        let object: IDataObject = ArchiveData {
            info: info.clone(),
            paths,
            password: None,
            control: JobControl::default().with_integrity(IntegrityMode::Normal),
            state: state.clone(),
            released: released.clone(),
            formats,
            entries,
        }
        .into();
        let destination = temp.path().join("destination");
        let target = shell_target(&destination);
        let mut effect = DROPEFFECT_COPY;
        unsafe {
            target
                .DragEnter(&object, MK_LBUTTON, POINTL::default(), &mut effect)
                .unwrap();
            assert_eq!(effect, DROPEFFECT_COPY);
            assert!(state.lock().unwrap().exported.is_none());
            released.store(true, Ordering::Release);
            target
                .Drop(
                    &object,
                    MODIFIERKEYS_FLAGS(0),
                    POINTL::default(),
                    &mut effect,
                )
                .unwrap();
        }
        assert!(state.lock().unwrap().error.is_none(), "ZIP 拖放不应失败");
        assert!(destination.join("nested").join("empty").is_dir());
        assert_eq!(
            fs::read(destination.join("nested").join("文件.txt")).unwrap(),
            b"unicode"
        );
        assert!(state.lock().unwrap().exported.is_none());
        assert!(state.lock().unwrap().zip.is_some());
        drop(target);
        drop(object);
        drop(state);

        let paths = vec!["folder/nested/empty".into()];
        let entries = virtual_entries(&info, &paths, &JobControl::default()).unwrap();
        assert_eq!(entries.len(), 1);
        let state = Arc::new(Mutex::new(DragState::default()));
        let object: IDataObject = ArchiveData {
            info,
            paths,
            password: None,
            control: JobControl::default(),
            state: state.clone(),
            released: Arc::new(AtomicBool::new(true)),
            formats,
            entries,
        }
        .into();
        fs::remove_file(path).unwrap();
        let destination = temp.path().join("empty-destination");
        let target = shell_target(&destination);
        let mut effect = DROPEFFECT_COPY;
        unsafe {
            target
                .DragEnter(&object, MK_LBUTTON, POINTL::default(), &mut effect)
                .unwrap();
            target
                .Drop(
                    &object,
                    MODIFIERKEYS_FLAGS(0),
                    POINTL::default(),
                    &mut effect,
                )
                .unwrap();
        }
        assert_eq!(effect, DROPEFFECT_COPY);
        assert!(destination.join("empty").is_dir());
        assert!(state.lock().unwrap().exported.is_none());
        assert!(state.lock().unwrap().error.is_none());
    }

    #[test]
    fn native_drag_cancellation_interrupts_extraction_and_keeps_source() {
        unsafe { OleInitialize(None).unwrap() };
        let _apartment = OleApartment;
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("file.txt");
        fs::write(&source, vec![b'x'; 1024 * 1024]).unwrap();
        let path = temp.path().join("test.zip");
        create_archive(
            &CreateOptions {
                sources: vec![source],
                destination: path.clone(),
                format: ArchiveFormat::Zip,
                password: None,
                overwrite: false,
            },
            &JobControl::default(),
        )
        .unwrap();
        let original = fs::read(&path).unwrap();
        let info = inspect_archive(&path, None, &JobControl::default()).unwrap();
        let paths = vec!["file.txt".into()];
        let entries = virtual_entries(&info, &paths, &JobControl::default()).unwrap();
        let token = CancellationToken::default();
        let control = JobControl::new(token.clone(), |_| {}).with_integrity(IntegrityMode::Normal);
        let state = Arc::new(Mutex::new(DragState::default()));
        let formats = DragFormats::new().unwrap();
        let object: IDataObject = ArchiveData {
            info,
            paths,
            password: None,
            control,
            state: state.clone(),
            released: Arc::new(AtomicBool::new(true)),
            formats,
            entries,
        }
        .into();
        unsafe {
            let mut medium = object.GetData(&formats.contents(0)).unwrap();
            let stream = medium.u.pstm.as_ref().unwrap();
            let mut buffer = vec![0u8; 32 * 1024];
            let mut read = 0;
            stream
                .Read(
                    buffer.as_mut_ptr().cast(),
                    buffer.len() as u32,
                    Some(&mut read),
                )
                .ok()
                .unwrap();
            assert_eq!(read, buffer.len() as u32);
            token.cancel();
            assert_eq!(
                stream.Read(
                    buffer.as_mut_ptr().cast(),
                    buffer.len() as u32,
                    Some(&mut read)
                ),
                windows::Win32::Foundation::STG_E_READFAULT
            );
            assert!(token.is_cancelled());
            ReleaseStgMedium(&mut medium);
            assert_eq!(
                object.GetData(&formats.contents(0)).err().unwrap().code(),
                E_FAIL
            );
        }
        assert!(state.lock().unwrap().exported.is_none());
        assert!(
            state
                .lock()
                .unwrap()
                .error
                .as_ref()
                .unwrap()
                .contains("取消")
        );
        assert_eq!(fs::read(path).unwrap(), original);
    }

    #[test]
    fn zip_com_stream_supports_seek_clone_stat_and_copy() {
        unsafe { OleInitialize(None).unwrap() };
        let _apartment = OleApartment;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("test.zip");
        let mut zip = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        zip.start_file("file.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"abcdefghij").unwrap();
        zip.finish().unwrap();
        let info = inspect_archive(&path, None, &JobControl::default()).unwrap();
        let paths = vec!["file.txt".into()];
        let entries = virtual_entries(&info, &paths, &JobControl::default()).unwrap();
        let formats = DragFormats::new().unwrap();
        let state = Arc::new(Mutex::new(DragState::default()));
        let object: IDataObject = ArchiveData {
            info,
            paths,
            password: None,
            control: JobControl::default().with_integrity(IntegrityMode::Normal),
            state: state.clone(),
            released: Arc::new(AtomicBool::new(true)),
            formats,
            entries,
        }
        .into();
        unsafe {
            let mut medium = object.GetData(&formats.contents(0)).unwrap();
            let stream = medium.u.pstm.as_ref().unwrap();
            let mut stat = STATSTG::default();
            stream.Stat(&mut stat, STATFLAG_NONAME).unwrap();
            assert_eq!(stat.cbSize, 10);
            assert!(stat.pwcsName.is_null());

            let mut bytes = [0; 4];
            let mut read = 0;
            assert_eq!(
                stream.Read(bytes.as_mut_ptr().cast(), 4, Some(&mut read)),
                S_OK
            );
            assert_eq!(&bytes, b"abcd");
            let clone = stream.Clone().unwrap();
            assert_eq!(
                stream.Read(bytes.as_mut_ptr().cast(), 3, Some(&mut read)),
                S_OK
            );
            assert_eq!(&bytes[..3], b"efg");
            assert_eq!(
                clone.Read(bytes.as_mut_ptr().cast(), 4, Some(&mut read)),
                S_OK
            );
            assert_eq!(&bytes, b"efgh");
            let mut position = 0;
            clone
                .Seek(-2, STREAM_SEEK_CUR, Some(&mut position))
                .unwrap();
            assert_eq!(position, 6);
            assert_eq!(
                clone.Read(bytes.as_mut_ptr().cast(), 4, Some(&mut read)),
                S_OK
            );
            assert_eq!(&bytes, b"ghij");
            assert_eq!(
                clone.Read(bytes.as_mut_ptr().cast(), 4, Some(&mut read)),
                S_FALSE
            );
            assert_eq!(read, 0);
            assert_eq!(clone.Read(std::ptr::null_mut(), 0, None), S_OK);
            assert!(clone.Seek(-1, STREAM_SEEK_SET, None).is_err());
            let mut written = 1;
            assert_eq!(
                clone.Write(bytes.as_ptr().cast(), 4, Some(&mut written)),
                STG_E_ACCESSDENIED
            );
            assert_eq!(written, 0);
            assert!(clone.SetSize(0).is_err());

            stream.Seek(0, STREAM_SEEK_SET, None).unwrap();
            let destination: IStream = CreateStreamOnHGlobal(Default::default(), true).unwrap();
            let (mut copied, mut saved) = (0, 0);
            stream
                .CopyTo(&destination, 20, Some(&mut copied), Some(&mut saved))
                .unwrap();
            assert_eq!((copied, saved), (10, 10));
            destination.Seek(0, STREAM_SEEK_SET, None).unwrap();
            let mut all = [0; 10];
            destination
                .Read(all.as_mut_ptr().cast(), 10, Some(&mut read))
                .ok()
                .unwrap();
            assert_eq!(&all, b"abcdefghij");
            ReleaseStgMedium(&mut medium);
        }
        let state = state.lock().unwrap();
        assert!(state.error.is_none());
        assert!(state.exported.is_none());
        assert!(state.zip.is_some());
    }

    #[test]
    fn zip_drag_interfaces_are_safe_across_com_apartments() {
        use windows::core::AgileReference;

        unsafe { OleInitialize(None).unwrap() };
        let _apartment = OleApartment;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("test.zip");
        let mut zip = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        zip.start_file("file.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"across apartments").unwrap();
        zip.finish().unwrap();
        let info = inspect_archive(&path, None, &JobControl::default()).unwrap();
        let paths = vec!["file.txt".into()];
        let entries = virtual_entries(&info, &paths, &JobControl::default()).unwrap();
        let formats = DragFormats::new().unwrap();
        let state = Arc::new(Mutex::new(DragState::default()));
        let object: IDataObject = ArchiveData {
            info,
            paths,
            password: None,
            control: JobControl::default().with_integrity(IntegrityMode::Normal),
            state: state.clone(),
            released: Arc::new(AtomicBool::new(true)),
            formats,
            entries,
        }
        .into();
        let agile = AgileReference::new(&object).unwrap();
        let handles = [agile.clone(), agile].map(|agile| {
            std::thread::spawn(move || {
                unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok().unwrap() };
                {
                    let object: IDataObject = agile.resolve().unwrap();
                    unsafe {
                        let mut medium = object.GetData(&formats.contents(0)).unwrap();
                        let stream = medium.u.pstm.as_ref().unwrap();
                        let mut bytes = [0; 17];
                        let mut read = 0;
                        stream
                            .Read(
                                bytes.as_mut_ptr().cast(),
                                bytes.len() as u32,
                                Some(&mut read),
                            )
                            .ok()
                            .unwrap();
                        assert_eq!(read, bytes.len() as u32);
                        assert_eq!(&bytes, b"across apartments");
                        ReleaseStgMedium(&mut medium);
                    }
                }
                drop(agile);
                unsafe { CoUninitialize() };
            })
        });
        for handle in handles {
            handle.join().unwrap();
        }
        let state = state.lock().unwrap();
        assert!(state.error.is_none());
        assert!(state.exported.is_none());
        assert!(state.zip.is_some());
    }

    #[test]
    fn non_zip_drag_retains_the_existing_temporary_export() {
        unsafe { OleInitialize(None).unwrap() };
        let _apartment = OleApartment;
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("folder");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("file.txt"), b"sevenzip").unwrap();
        let path = temp.path().join("test.7z");
        create_archive(
            &CreateOptions {
                sources: vec![source],
                destination: path.clone(),
                format: ArchiveFormat::SevenZip,
                password: None,
                overwrite: false,
            },
            &JobControl::default(),
        )
        .unwrap();
        let info = inspect_archive(&path, None, &JobControl::default()).unwrap();
        let paths = vec!["folder".into()];
        let entries = virtual_entries(&info, &paths, &JobControl::default()).unwrap();
        let formats = DragFormats::new().unwrap();
        let state = Arc::new(Mutex::new(DragState::default()));
        let object: IDataObject = ArchiveData {
            info,
            paths,
            password: None,
            control: JobControl::default(),
            state: state.clone(),
            released: Arc::new(AtomicBool::new(true)),
            formats,
            entries,
        }
        .into();
        let destination = temp.path().join("destination");
        let target = shell_target(&destination);
        let mut effect = DROPEFFECT_COPY;
        unsafe {
            target
                .DragEnter(&object, MK_LBUTTON, POINTL::default(), &mut effect)
                .unwrap();
            target
                .Drop(
                    &object,
                    MODIFIERKEYS_FLAGS(0),
                    POINTL::default(),
                    &mut effect,
                )
                .unwrap();
        }
        assert_eq!(effect, DROPEFFECT_COPY);
        assert_eq!(
            fs::read(destination.join("folder").join("file.txt")).unwrap(),
            b"sevenzip"
        );
        let state = state.lock().unwrap();
        assert!(state.error.is_none());
        assert!(state.zip.is_none());
        let exported = state.exported.as_ref().unwrap();
        assert_eq!(
            fs::read(exported.paths[0].join("file.txt")).unwrap(),
            b"sevenzip"
        );
    }

    #[test]
    fn native_drag_source_supports_copy_drop_escape_and_cancellation() {
        let cancellation = CancellationToken::default();
        let released = Arc::new(AtomicBool::new(false));
        let source = DragSource {
            control: JobControl::new(cancellation.clone(), |_| {}),
            released: released.clone(),
            direct: Arc::default(),
        };
        assert_eq!(source.continue_drag(false, true), S_OK);
        assert!(!released.load(Ordering::Acquire));
        assert_eq!(source.continue_drag(false, false), DRAGDROP_S_DROP);
        assert!(released.load(Ordering::Acquire));
        assert_eq!(source.continue_drag(true, true), DRAGDROP_S_CANCEL);
        assert!(!released.load(Ordering::Acquire));
        cancellation.cancel();
        assert_eq!(source.continue_drag(false, true), DRAGDROP_S_CANCEL);
        assert_eq!(source.continue_drag(false, false), DRAGDROP_S_CANCEL);
    }
}
