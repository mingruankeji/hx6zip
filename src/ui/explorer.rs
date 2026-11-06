use std::{
    collections::BTreeSet,
    fs,
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

use anyhow::{Context as _, Result};
use gpui_kit::{
    component::{
        ActiveTheme, Disableable, Icon, IconName, Sizable,
        button::{Button, ButtonVariants},
        checkbox::Checkbox,
        input::{Input, InputEvent, InputState},
        menu::{ContextMenuExt, DropdownMenu, PopupMenu, PopupMenuItem},
    },
    prelude::FluentBuilder,
    *,
};

use super::{AppView, Page, format_size};
use mzip::archive::ArchiveKind;

const FILE_ROW_HEIGHT: Pixels = px(36.0);
const MARQUEE_THRESHOLD: f64 = 4.0;
const MARQUEE_SCROLL_INTERVAL: Duration = Duration::from_millis(32);
const MARQUEE_SCROLL_MARGIN: Pixels = px(28.0);
const MARQUEE_SCROLL_MAX_STEP: f32 = 24.0;

#[derive(Clone, Debug, Eq, PartialEq)]
enum Location {
    Computer,
    Directory(PathBuf),
    Archive { path: PathBuf, directory: String },
}

impl Location {
    fn label(&self) -> String {
        match self {
            Self::Computer => "此电脑".to_owned(),
            Self::Directory(path) => path.display().to_string(),
            Self::Archive { path, directory } => {
                format!("{}\\{}", path.display(), directory.replace('/', "\\"))
            }
        }
    }

    fn parent(&self) -> Option<Self> {
        match self {
            Self::Computer => None,
            Self::Archive { path, directory } => {
                if directory.is_empty() {
                    Some(
                        path.parent()
                            .map(|p| Self::Directory(p.to_path_buf()))
                            .unwrap_or(Self::Computer),
                    )
                } else {
                    Some(Self::Archive {
                        path: path.clone(),
                        directory: directory
                            .rsplit_once('/')
                            .map(|(parent, _)| parent.to_owned())
                            .unwrap_or_default(),
                    })
                }
            }
            Self::Directory(path) => Some(
                path.parent()
                    .map(|parent| Self::Directory(parent.to_path_buf()))
                    .unwrap_or(Self::Computer),
            ),
        }
    }
}

struct FileEntry {
    path: PathBuf,
    name: String,
    directory: bool,
    size: u64,
    modified: Option<SystemTime>,
}

fn archive_file_entries(
    tree: &mzip::archive::ArchiveTree,
    directory: &str,
) -> Result<Vec<FileEntry>> {
    let entries = tree.children(directory)?;
    Ok(entries
        .iter()
        .map(|entry| FileEntry {
            name: entry
                .name
                .rsplit('/')
                .next()
                .unwrap_or(&entry.name)
                .to_owned(),
            path: PathBuf::from(&entry.name),
            directory: entry.is_directory,
            size: entry.size,
            modified: entry.modified,
        })
        .collect())
}

struct MarqueeSelection {
    id: u64,
    start_pointer: Point<Pixels>,
    start_content: Point<Pixels>,
    pointer: Point<Pixels>,
    additive: bool,
    original_selection: Option<BTreeSet<usize>>,
    original_anchor: Option<usize>,
    original_cursor: Option<usize>,
    rows: Range<usize>,
    last_viewport: Bounds<Pixels>,
    last_offset: Point<Pixels>,
}

impl MarqueeSelection {
    fn is_active(&self) -> bool {
        self.original_selection.is_some()
    }
}

fn marquee_rows(
    start_content: Point<Pixels>,
    pointer: Point<Pixels>,
    viewport: Bounds<Pixels>,
    offset: Point<Pixels>,
    count: usize,
) -> Range<usize> {
    if count == 0 || viewport.size.width <= px(0.0) || viewport.size.height <= px(0.0) {
        return 0..0;
    }
    let start_x = viewport.left() + offset.x + start_content.x;
    if start_x.max(pointer.x) < viewport.left() || start_x.min(pointer.x) > viewport.right() {
        return 0..0;
    }
    let end_y = pointer.y.clamp(viewport.top(), viewport.bottom()) - viewport.top() - offset.y;
    let top = start_content.y.min(end_y);
    let bottom = start_content.y.max(end_y);
    let content_height = FILE_ROW_HEIGHT * count;
    if bottom < px(0.0) || top >= content_height {
        return 0..0;
    }
    let first = (top.max(px(0.0)) / FILE_ROW_HEIGHT).floor() as usize;
    let end = if top == bottom {
        first + 1
    } else {
        (bottom.max(px(0.0)) / FILE_ROW_HEIGHT).ceil() as usize
    };
    first.min(count)..end.min(count).max(first.min(count))
}

fn range_difference(range: &Range<usize>, other: &Range<usize>) -> [Range<usize>; 2] {
    [
        range.start..range.end.min(other.start).max(range.start),
        range.start.max(other.end).min(range.end)..range.end,
    ]
}

fn update_marquee_rows(
    selected: &mut BTreeSet<usize>,
    original_selection: &BTreeSet<usize>,
    additive: bool,
    visible: &[usize],
    old_rows: &Range<usize>,
    new_rows: &Range<usize>,
) {
    for row in range_difference(old_rows, new_rows).into_iter().flatten() {
        let index = visible[row];
        if !additive || !original_selection.contains(&index) {
            selected.remove(&index);
        }
    }
    for row in range_difference(new_rows, old_rows).into_iter().flatten() {
        selected.insert(visible[row]);
    }
}

fn marquee_scroll_delta(pointer_y: Pixels, viewport: Bounds<Pixels>) -> Pixels {
    let margin = MARQUEE_SCROLL_MARGIN.min(viewport.size.height / 2.0);
    if margin <= px(0.0) {
        return px(0.0);
    }
    let top = viewport.top() + margin;
    let bottom = viewport.bottom() - margin;
    if pointer_y < top {
        let strength = ((top - pointer_y) / margin).clamp(0.0, 1.0);
        px(4.0 + (MARQUEE_SCROLL_MAX_STEP - 4.0) * strength)
    } else if pointer_y > bottom {
        let strength = ((pointer_y - bottom) / margin).clamp(0.0, 1.0);
        -px(4.0 + (MARQUEE_SCROLL_MAX_STEP - 4.0) * strength)
    } else {
        px(0.0)
    }
}

fn read_location(location: &Location) -> Result<Vec<FileEntry>> {
    let mut entries = match location {
        Location::Archive { .. } => anyhow::bail!("压缩包目录必须通过归档读取"),
        Location::Computer => {
            #[cfg(windows)]
            let roots = (b'A'..=b'Z')
                .map(|letter| PathBuf::from(format!("{}:\\", letter as char)))
                .filter(|path| path.is_dir())
                .collect::<Vec<_>>();
            #[cfg(not(windows))]
            let roots = vec![PathBuf::from("/")];
            roots
                .into_iter()
                .map(|path| FileEntry {
                    name: format!("本地磁盘 ({})", path.display()),
                    path,
                    directory: true,
                    size: 0,
                    modified: None,
                })
                .collect()
        }
        Location::Directory(path) => fs::read_dir(path)
            .with_context(|| format!("无法读取文件夹：{}", path.display()))?
            .map(|entry| {
                let entry = entry.context("无法读取目录条目")?;
                let metadata = entry
                    .metadata()
                    .with_context(|| format!("无法读取文件信息：{}", entry.path().display()))?;
                Ok(FileEntry {
                    name: entry.file_name().to_string_lossy().into_owned(),
                    path: entry.path(),
                    directory: metadata.is_dir(),
                    size: metadata.len(),
                    modified: metadata.modified().ok(),
                })
            })
            .collect::<Result<Vec<_>>>()?,
    };
    entries.sort_by_cached_key(|entry| (!entry.directory, entry.name.to_lowercase()));
    Ok(entries)
}

pub(super) struct Explorer {
    archive_tree: Option<Arc<mzip::archive::ArchiveTree>>,
    location: Location,
    entries: Vec<FileEntry>,
    selected: BTreeSet<usize>,
    anchor: Option<usize>,
    cursor: Option<usize>,
    last_clicked_row: Option<usize>,
    scroll: UniformListScrollHandle,
    marquee: Option<MarqueeSelection>,
    next_marquee_id: u64,
    suppress_marquee_release: bool,
    visible: Vec<usize>,
    filter: String,
    address: Entity<InputState>,
    search: Entity<InputState>,
    displayed_address: String,
    history: Vec<Location>,
    history_index: usize,
    request: u64,
    loading: bool,
    initialized: bool,
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
    drag_origin: Option<(usize, Point<Pixels>)>,
}

impl Explorer {
    pub(super) fn prepare_archive_open(&mut self) {
        self.cancel_marquee();
        self.drag_origin = None;
        self.initialized = true;
        self.loading = false;
        self.request += 1;
    }

    pub(super) fn new(window: &mut Window, cx: &mut Context<AppView>) -> Self {
        let address = cx.new(|cx| InputState::new(window, cx).placeholder("输入文件夹路径"));
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("搜索当前文件夹"));
        let address_subscription = cx.subscribe(&address, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.go_to_address(cx);
            }
        });
        let search_subscription = cx.observe(&search, |this, _, cx| {
            this.explorer.filter_entries(cx);
            cx.notify();
        });
        let activation_subscription = cx.observe_window_activation(window, |this, window, cx| {
            if !window.is_window_active() {
                this.explorer.drag_origin = None;
                if this.explorer.cancel_marquee() {
                    cx.notify();
                }
            }
        });
        Self {
            archive_tree: None,
            location: Location::Computer,
            entries: Vec::new(),
            selected: BTreeSet::new(),
            anchor: None,
            cursor: None,
            last_clicked_row: None,
            scroll: UniformListScrollHandle::new(),
            marquee: None,
            next_marquee_id: 0,
            suppress_marquee_release: false,
            visible: Vec::new(),
            filter: String::new(),
            address,
            search,
            displayed_address: String::new(),
            history: vec![Location::Computer],
            history_index: 0,
            request: 0,
            loading: false,
            initialized: false,
            focus: cx.focus_handle(),
            _subscriptions: vec![
                address_subscription,
                search_subscription,
                activation_subscription,
            ],
            drag_origin: None,
        }
    }

    fn filter_entries(&mut self, cx: &App) {
        self.cancel_marquee();
        self.filter = self.search.read(cx).value().trim().to_lowercase();
        self.visible = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.name.to_lowercase().contains(&self.filter))
            .map(|(index, _)| index)
            .collect();
        self.selected.retain(|index| {
            self.entries[*index]
                .name
                .to_lowercase()
                .contains(&self.filter)
        });
        self.anchor = None;
        self.cursor = None;
        self.last_clicked_row = None;
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
    }

    fn begin_marquee(&mut self, event: &MouseDownEvent) {
        self.cancel_marquee();
        self.suppress_marquee_release = false;
        if self.loading || self.visible.is_empty() {
            return;
        }
        let handle = self.scroll.0.borrow().base_handle.clone();
        let viewport = handle.bounds();
        if !viewport.contains(&event.position) {
            return;
        }
        let offset = handle.offset();
        self.next_marquee_id = self.next_marquee_id.wrapping_add(1);
        self.marquee = Some(MarqueeSelection {
            id: self.next_marquee_id,
            start_pointer: event.position,
            start_content: event.position - viewport.origin - offset,
            pointer: event.position,
            additive: event.modifiers.control,
            original_selection: None,
            original_anchor: self.anchor,
            original_cursor: self.cursor,
            rows: 0..0,
            last_viewport: viewport,
            last_offset: offset,
        });
    }

    fn update_marquee(&mut self, pointer: Point<Pixels>) -> Option<(u64, bool)> {
        let handle = self.scroll.0.borrow().base_handle.clone();
        let viewport = handle.bounds();
        let offset = handle.offset();
        let marquee = self.marquee.as_mut()?;
        marquee.pointer = pointer;
        let started = !marquee.is_active();
        if started {
            if (pointer - marquee.start_pointer).magnitude() <= MARQUEE_THRESHOLD {
                return None;
            }
            self.last_clicked_row = None;
            let original = std::mem::take(&mut self.selected);
            if marquee.additive {
                self.selected.clone_from(&original);
            }
            marquee.original_selection = Some(original);
        }
        let rows = marquee_rows(
            marquee.start_content,
            pointer,
            viewport,
            offset,
            self.visible.len(),
        );
        update_marquee_rows(
            &mut self.selected,
            marquee.original_selection.as_ref().expect("active marquee"),
            marquee.additive,
            &self.visible,
            &marquee.rows,
            &rows,
        );
        if rows.is_empty() {
            self.anchor = marquee
                .additive
                .then_some(marquee.original_anchor)
                .flatten();
            self.cursor = marquee
                .additive
                .then_some(marquee.original_cursor)
                .flatten();
        } else {
            let start_row =
                (marquee.start_content.y.max(px(0.0)) / FILE_ROW_HEIGHT).floor() as usize;
            self.anchor = Some(start_row.min(self.visible.len() - 1));
            let end_y =
                pointer.y.clamp(viewport.top(), viewport.bottom()) - viewport.top() - offset.y;
            self.cursor = Some(if end_y >= marquee.start_content.y {
                rows.end - 1
            } else {
                rows.start
            });
        }
        marquee.rows = rows;
        marquee.last_viewport = viewport;
        marquee.last_offset = offset;
        Some((marquee.id, started))
    }

    fn tick_marquee_scroll(&mut self, id: u64) -> Option<bool> {
        let marquee = self.marquee.as_ref()?;
        if marquee.id != id || !marquee.is_active() {
            return None;
        }
        let pointer = marquee.pointer;
        let last_viewport = marquee.last_viewport;
        let last_offset = marquee.last_offset;
        let handle = self.scroll.0.borrow().base_handle.clone();
        let viewport = handle.bounds();
        let offset = handle.offset();
        let max_offset = handle.max_offset().y.max(px(0.0));
        let next_y =
            (offset.y + marquee_scroll_delta(pointer.y, viewport)).clamp(-max_offset, px(0.0));
        let next_offset = point(offset.x, next_y);
        if next_offset != offset {
            handle.set_offset(next_offset);
        }
        if next_offset != last_offset || viewport != last_viewport {
            self.update_marquee(pointer);
            Some(true)
        } else {
            Some(false)
        }
    }

    fn finish_marquee(&mut self) -> bool {
        let active = self
            .marquee
            .take()
            .is_some_and(|marquee| marquee.is_active());
        std::mem::take(&mut self.suppress_marquee_release) || active
    }

    fn cancel_marquee(&mut self) -> bool {
        let Some(marquee) = self.marquee.take() else {
            return false;
        };
        if let Some(original) = marquee.original_selection {
            self.selected = original;
            self.anchor = marquee.original_anchor;
            self.cursor = marquee.original_cursor;
        }
        self.suppress_marquee_release = true;
        true
    }

    fn selected_paths(&self) -> Vec<PathBuf> {
        self.selected
            .iter()
            .filter_map(|index| self.entries.get(*index))
            .map(|entry| entry.path.clone())
            .collect()
    }
}

#[derive(Clone, Copy)]
enum FileAction {
    Open,
    Zip,
    EachZip,
    SevenZip,
    Add,
    ExtractHere,
    ExtractFolder,
    ExtractTo,
    Split,
    Merge,
    ShowInExplorer,
}

fn select_row(
    selected: &mut BTreeSet<usize>,
    anchor: &mut Option<usize>,
    visible: &[usize],
    row: usize,
    control: bool,
    shift: bool,
) {
    let Some(&index) = visible.get(row) else {
        return;
    };
    if shift {
        let start = anchor.unwrap_or(row).min(visible.len() - 1);
        if !control {
            selected.clear();
        }
        selected.extend(&visible[start.min(row)..=start.max(row)]);
        anchor.get_or_insert(row);
    } else {
        if !control {
            selected.clear();
        }
        if !selected.insert(index) {
            selected.remove(&index);
        }
        *anchor = Some(row);
    }
}

impl AppView {
    pub(super) fn in_archive(&self) -> bool {
        self.page == Page::Explorer && matches!(self.explorer.location, Location::Archive { .. })
    }

    fn archive_paths(&self) -> Vec<String> {
        self.explorer
            .selected_paths()
            .iter()
            .map(|path| path.to_string_lossy().replace('\\', "/"))
            .collect()
    }

    pub(super) fn enter_archive_root(
        &mut self,
        info: mzip::archive::ArchiveInfo,
        tree: mzip::archive::ArchiveTree,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        let entries = archive_file_entries(&tree, "")?;
        let location = Location::Archive {
            path: info.path.clone(),
            directory: String::new(),
        };
        self.explorer.initialized = true;
        self.explorer.request += 1;
        self.explorer.loading = false;
        self.explorer
            .history
            .truncate(self.explorer.history_index + 1);
        if self.explorer.location != location {
            self.explorer.history.push(location.clone());
            self.explorer.history_index = self.explorer.history.len() - 1;
        }
        self.explorer.location = location;
        self.page = Page::Explorer;
        self.last_open_path = Some(info.path.clone());
        self.archive = Some(info);
        self.explorer.archive_tree = Some(Arc::new(tree));
        self.explorer.cancel_marquee();
        self.explorer.entries = entries;
        self.explorer.selected.clear();
        self.explorer.filter_entries(cx);
        Ok(())
    }

    pub(super) fn refresh_open_archive(
        &mut self,
        info: mzip::archive::ArchiveInfo,
        tree: mzip::archive::ArchiveTree,
        cx: &mut Context<Self>,
    ) {
        let directory = match &self.explorer.location {
            Location::Archive { directory, .. } => directory.clone(),
            _ => String::new(),
        };
        match archive_file_entries(&tree, &directory) {
            Ok(entries) => {
                self.last_open_path = Some(info.path.clone());
                self.archive = Some(info);
                self.explorer.archive_tree = Some(Arc::new(tree));
                self.selected.clear();
                self.filter_dirty = true;
                self.explorer.cancel_marquee();
                self.explorer.entries = entries;
                self.explorer.selected.clear();
                self.explorer.filter_entries(cx);
                self.set_status(
                    "压缩包目录已更新。拖入添加，拖出解压；Delete 删除所选项目。",
                    false,
                    cx,
                );
            }
            Err(error) => self.set_status(format!("{error:#}"), true, cx),
        }
    }

    fn choose_archive_add(&mut self, cx: &mut Context<Self>) {
        if let Some(paths) = rfd::FileDialog::new()
            .set_title("添加到当前压缩包目录")
            .pick_files()
        {
            self.add_to_open_archive(paths, cx);
        }
    }

    pub(super) fn add_to_open_archive(&mut self, sources: Vec<PathBuf>, cx: &mut Context<Self>) {
        if self.explorer.loading {
            self.set_status("请等待当前目录加载完成。", true, cx);
            return;
        }
        let (Some(info), Location::Archive { directory, .. }) =
            (&self.archive, &self.explorer.location)
        else {
            self.set_status("请先打开压缩包。", true, cx);
            return;
        };
        let info = info.clone();
        let directory = directory.clone();
        let password = Self::password(&self.archive_password, cx);
        self.start_job("正在检查待添加项目…", super::JobKind::Update, cx, move |control| {
            let conflicts = mzip::archive::import_conflicts(&info, &sources, &directory, &control)?;
            let overwrite = !conflicts.is_empty();
            if overwrite {
                let preview = conflicts.iter().take(8).cloned().collect::<Vec<_>>().join("\n");
                let result = rfd::MessageDialog::new().set_title("替换压缩包中的同名项目")
                    .set_description(format!("存在 {} 个同名项目：\n{preview}\n\n是否全部替换？同名文件夹合并，文件/文件夹类型冲突时替换整个项目。", conflicts.len()))
                    .set_level(rfd::MessageLevel::Warning).set_buttons(rfd::MessageButtons::YesNo).show();
                if result != rfd::MessageDialogResult::Yes { return Err(mzip::archive::Cancelled.into()); }
            }
            let edit = mzip::archive::ArchiveEdit::Add { sources, directory, overwrite };
            let info = mzip::archive::edit_archive(&info, &edit, password.as_deref(), &control)?;
            let tree = mzip::archive::ArchiveTree::new(&info, &control)?;
            Ok(super::JobOutput::Updated(info, tree))
        });
    }

    fn delete_archive_selection(&mut self, cx: &mut Context<Self>) {
        if self.running.is_some() {
            self.set_status("请先等待当前任务完成。", true, cx);
            return;
        }
        let Some(info) = &self.archive else { return };
        if let Err(error) = mzip::archive::editable_format(info) {
            self.set_status(format!("{error:#}"), true, cx);
            return;
        }
        let paths = self.archive_paths();
        if paths.is_empty() {
            self.set_status("请选择要删除的项目。", true, cx);
            return;
        }
        if rfd::MessageDialog::new()
            .set_title("删除压缩包中的项目")
            .set_description(format!(
                "永久删除所选 {} 个项目及文件夹中的全部内容？此操作不经过回收站。",
                paths.len()
            ))
            .set_level(rfd::MessageLevel::Warning)
            .set_buttons(rfd::MessageButtons::YesNo)
            .show()
            != rfd::MessageDialogResult::Yes
        {
            return;
        }
        let info = info.clone();
        let password = Self::password(&self.archive_password, cx);
        self.start_job(
            "正在删除压缩包中的项目…",
            super::JobKind::Update,
            cx,
            move |control| {
                let info = mzip::archive::edit_archive(
                    &info,
                    &mzip::archive::ArchiveEdit::Delete { paths },
                    password.as_deref(),
                    &control,
                )?;
                let tree = mzip::archive::ArchiveTree::new(&info, &control)?;
                Ok(super::JobOutput::Updated(info, tree))
            },
        );
    }

    fn extract_archive_selection(&mut self, cx: &mut Context<Self>) {
        let Some(info) = &self.archive else { return };
        let selected = match mzip::archive::archive_selection(info, &self.archive_paths()) {
            Ok(selected) => selected,
            Err(error) => {
                self.set_status(format!("{error:#}"), true, cx);
                return;
            }
        };
        let Some(destination) = rfd::FileDialog::new()
            .set_title("解压所选项目到")
            .pick_folder()
        else {
            return;
        };
        let info = info.clone();
        let password = Self::password(&self.archive_password, cx);
        let dialog = self.overwrite_dialog.clone();
        self.start_job(
            "正在解压所选项目…",
            super::JobKind::Extract,
            cx,
            move |control| {
                super::overwrite::extract_with_confirmation(
                    &info,
                    mzip::archive::ExtractOptions {
                        destination: destination.clone(),
                        selected: Some(selected),
                        password,
                        overwrite: false,
                    },
                    &control,
                    &dialog,
                )?;
                Ok(super::JobOutput::Extracted {
                    destination,
                    open_after: false,
                })
            },
        );
    }

    fn open_archive_file(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(info) = self.archive.clone() else {
            return;
        };
        let path = self.explorer.entries[index]
            .path
            .to_string_lossy()
            .replace('\\', "/");
        let password = Self::password(&self.archive_password, cx);
        self.start_job(
            "正在提取文件的临时副本…",
            super::JobKind::Extract,
            cx,
            move |control| {
                let entries =
                    mzip::archive::export_entries(&info, &[path], password.as_deref(), &control)?;
                Ok(super::JobOutput::Exported {
                    entries: Some(entries),
                    open: true,
                })
            },
        );
    }

    fn begin_archive_drag(&mut self, index: usize, cx: &mut Context<Self>) {
        if !self.in_archive() || self.running.is_some() {
            return;
        }
        if !self.explorer.selected.contains(&index) {
            self.explorer.selected.clear();
            self.explorer.selected.insert(index);
        }
        self.explorer.last_clicked_row = None;
        let paths = self.archive_paths();
        let Some(info) = self.archive.clone() else {
            return;
        };
        let password = Self::password(&self.archive_password, cx);
        #[cfg(windows)]
        {
            let ui_thread = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
            let dialog = self.overwrite_dialog.clone();
            unsafe {
                let _ = windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture();
            }
            self.start_job(
                "拖到 Windows 文件夹后松开鼠标即可解压；Esc 取消。",
                super::JobKind::Drag,
                cx,
                move |control| {
                    let outcome = super::archive_drag::drag_archive(
                        info, paths, password, control, ui_thread, dialog,
                    )?;
                    Ok(super::JobOutput::Dragged {
                        entries: outcome.entries,
                        dropped: outcome.dropped,
                        destination: outcome.destination,
                    })
                },
            );
        }
        #[cfg(not(windows))]
        {
            let _ = (info, paths, password);
            self.set_status(
                "当前平台暂不支持向外部文件夹拖放，请使用解压所选。",
                true,
                cx,
            );
        }
    }

    pub(super) fn open_directory(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.explorer.initialized = true;
        self.navigate(Location::Directory(path), None, cx);
    }

    pub(super) fn initialize_explorer(&mut self, cx: &mut Context<Self>) {
        if !self.explorer.initialized {
            self.explorer.initialized = true;
            self.navigate(Location::Computer, None, cx);
        }
    }

    fn navigate(
        &mut self,
        location: Location,
        history_index: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        if self.running.is_some() {
            self.set_status("请等待当前任务完成，或先取消任务。", true, cx);
            return;
        }
        self.explorer.cancel_marquee();
        self.page = Page::Explorer;
        self.explorer.request += 1;
        let request = self.explorer.request;
        self.explorer.loading = true;
        let worker_location = location.clone();
        let password = Self::password(&self.archive_password, cx);
        let cached_tree = match &location {
            Location::Archive { path, .. }
                if self.archive.as_ref().is_some_and(|info| &info.path == path)
                    && (location != self.explorer.location || history_index.is_some()) =>
            {
                self.explorer.archive_tree.clone()
            }
            _ => None,
        };
        let task = cx.background_executor().spawn(async move {
            if let Location::Archive { path, directory } = &worker_location {
                if let Some(tree) = cached_tree {
                    return archive_file_entries(&tree, directory).map(|entries| (entries, None));
                }
                let info = mzip::archive::inspect_archive(
                    path,
                    password.as_deref(),
                    &mzip::archive::JobControl::default(),
                )?;
                let tree =
                    mzip::archive::ArchiveTree::new(&info, &mzip::archive::JobControl::default())?;
                let entries = archive_file_entries(&tree, directory)?;
                Ok((entries, Some((info, tree))))
            } else {
                read_location(&worker_location).map(|entries| (entries, None))
            }
        });
        cx.spawn(async move |view, cx| {
            let result = task.await;
            let _ = view.update(cx, |this, cx| {
                if this.explorer.request != request {
                    return;
                }
                this.explorer.loading = false;
                match result {
                    Ok((entries, archive)) => {
                        if let Some((info, tree)) = archive {
                            this.last_open_path = Some(info.path.clone());
                            this.archive = Some(info);
                            this.explorer.archive_tree = Some(Arc::new(tree));
                            this.selected.clear();
                            this.filter_dirty = true;
                        }
                        if let Some(index) = history_index {
                            this.explorer.history_index = index;
                        } else if this.explorer.location != location {
                            this.explorer
                                .history
                                .truncate(this.explorer.history_index + 1);
                            this.explorer.history.push(location.clone());
                            this.explorer.history_index = this.explorer.history.len() - 1;
                        }
                        this.explorer.location = location;
                        this.explorer.entries = entries;
                        this.explorer.selected.clear();
                        this.explorer.anchor = None;
                        this.explorer.filter_entries(cx);
                        this.set_status(
                            format!(
                                "{} 个项目 · 右键执行压缩或解压操作",
                                this.explorer.entries.len()
                            ),
                            false,
                            cx,
                        );
                    }
                    Err(error) => {
                        // Force the address field back to the last successfully loaded location.
                        this.explorer.displayed_address.clear();
                        this.set_status(format!("{error:#}"), true, cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn go_to_address(&mut self, cx: &mut Context<Self>) {
        let value = self.explorer.address.read(cx).value().trim().to_owned();
        if value.is_empty() {
            self.set_status("请输入文件夹路径。", true, cx);
            return;
        }
        let location = if value == "此电脑" {
            Location::Computer
        } else {
            let path = PathBuf::from(value.trim_matches('"'));
            let path = if path.is_absolute() {
                path
            } else {
                match &self.explorer.location {
                    Location::Directory(base) => base.join(path),
                    Location::Archive {
                        path: archive,
                        directory,
                    } => archive.join(directory).join(path),
                    Location::Computer => {
                        self.set_status("请在此电脑中输入完整文件夹路径。", true, cx);
                        return;
                    }
                }
            };
            let path = match std::path::absolute(path) {
                Ok(path) => path,
                Err(error) => {
                    self.set_status(format!("无法确定路径：{error}"), true, cx);
                    return;
                }
            };
            if path.is_file() {
                self.open_path(path, cx);
                return;
            }
            let archive = path.ancestors().find(|ancestor| ancestor.is_file());
            if let Some(archive) = archive {
                Location::Archive {
                    path: archive.to_path_buf(),
                    directory: path
                        .strip_prefix(archive)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                }
            } else {
                Location::Directory(path)
            }
        };
        self.navigate(location, None, cx);
    }

    fn explorer_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        if self.explorer.loading {
            return;
        }
        let modifiers = event.keystroke.modifiers;
        if event.keystroke.key == "escape" {
            self.explorer.drag_origin = None;
        }
        if event.keystroke.key == "escape" && self.explorer.cancel_marquee() {
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if (matches!(
            event.keystroke.key.as_str(),
            "f5" | "enter" | "backspace" | "up" | "down"
        ) || (event.keystroke.key == "a" && modifiers.control)
            || (matches!(event.keystroke.key.as_str(), "left" | "right") && modifiers.alt))
            && self.explorer.cancel_marquee()
        {
            cx.notify();
        }
        match event.keystroke.key.as_str() {
            "delete" if self.in_archive() => self.delete_archive_selection(cx),
            "f5" => self.navigate(self.explorer.location.clone(), None, cx),
            "a" if modifiers.control => {
                self.explorer
                    .selected
                    .extend(self.explorer.visible.iter().copied());
            }
            "enter" if self.explorer.selected.len() == 1 => {
                self.file_action(FileAction::Open, cx);
            }
            "backspace" => {
                if let Some(parent) = self.explorer.location.parent() {
                    self.navigate(parent, None, cx);
                }
            }
            "left" if modifiers.alt => self.travel(false, cx),
            "right" if modifiers.alt => self.travel(true, cx),
            "escape" => self.explorer.selected.clear(),
            "up" | "down" if !self.explorer.visible.is_empty() => {
                let current = self.explorer.cursor;
                let row = match (event.keystroke.key.as_str(), current) {
                    ("up", Some(row)) => row.saturating_sub(1),
                    ("down", Some(row)) => (row + 1).min(self.explorer.visible.len() - 1),
                    _ => 0,
                };
                select_row(
                    &mut self.explorer.selected,
                    &mut self.explorer.anchor,
                    &self.explorer.visible,
                    row,
                    modifiers.control,
                    modifiers.shift,
                );
                self.explorer.cursor = Some(row);
                self.explorer
                    .scroll
                    .scroll_to_item(row, ScrollStrategy::Nearest);
            }
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn start_marquee_scroll(id: u64, cx: &mut Context<Self>) {
        cx.spawn(async move |view, cx| {
            loop {
                cx.background_executor()
                    .timer(MARQUEE_SCROLL_INTERVAL)
                    .await;
                let keep_running = view.update(cx, |this, cx| {
                    if !this
                        .explorer
                        .marquee
                        .as_ref()
                        .is_some_and(|marquee| marquee.id == id && marquee.is_active())
                    {
                        return false;
                    }
                    if this.page != Page::Explorer || this.explorer.loading {
                        this.explorer.cancel_marquee();
                        cx.notify();
                        return false;
                    }
                    match this.explorer.tick_marquee_scroll(id) {
                        Some(changed) => {
                            if changed {
                                cx.notify();
                            }
                            true
                        }
                        None => false,
                    }
                });
                if !matches!(keep_running, Ok(true)) {
                    return;
                }
            }
        })
        .detach();
    }

    fn travel(&mut self, forward: bool, cx: &mut Context<Self>) {
        let index = if forward {
            self.explorer.history_index.checked_add(1)
        } else {
            self.explorer.history_index.checked_sub(1)
        };
        if let Some(index) = index
            && let Some(location) = self.explorer.history.get(index).cloned()
        {
            self.navigate(location, Some(index), cx);
        }
    }

    fn open_file_entry(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(entry) = self.explorer.entries.get(index) else {
            self.set_status("该文件已不在列表中，请刷新后重试。", true, cx);
            return;
        };
        let path = entry.path.clone();
        if let Location::Archive { path: archive, .. } = &self.explorer.location {
            if entry.directory {
                self.navigate(
                    Location::Archive {
                        path: archive.clone(),
                        directory: path.to_string_lossy().replace('\\', "/"),
                    },
                    None,
                    cx,
                );
            } else {
                self.open_archive_file(index, cx);
            }
            return;
        }
        if entry.directory {
            self.navigate(Location::Directory(path), None, cx);
        } else if ArchiveKind::from_path(&path).is_ok() {
            self.open_path(path, cx);
        } else if let Err(error) = open_default(&path) {
            self.set_status(format!("{error:#}"), true, cx);
        }
    }

    fn file_action(&mut self, action: FileAction, cx: &mut Context<Self>) {
        let paths = self.explorer.selected_paths();
        let flag = match action {
            FileAction::Open => {
                if let Some(index) = self.explorer.selected.first().copied() {
                    self.open_file_entry(index, cx);
                } else {
                    self.set_status("请先选择文件或文件夹。", true, cx);
                }
                return;
            }
            FileAction::ShowInExplorer => {
                match &self.explorer.location {
                    Location::Directory(path) => {
                        if let Err(error) = super::open_in_file_manager(path) {
                            self.set_status(format!("{error:#}"), true, cx);
                        }
                    }
                    Location::Computer | Location::Archive { .. } => {
                        self.set_status("请先进入一个磁盘或文件夹。", true, cx);
                    }
                }
                return;
            }
            _ if paths.is_empty() => {
                self.set_status("请先选择文件或文件夹。", true, cx);
                return;
            }
            FileAction::Zip => "--create-zip",
            FileAction::EachZip => "--create-each-zip",
            FileAction::SevenZip => "--create-7z",
            FileAction::Add => "--add-to-archive",
            FileAction::Split => {
                if !mzip_shell_protocol::can_split(&paths) {
                    self.set_status("请选择一个文件进行分割。", true, cx);
                    return;
                }
                "--split-file"
            }
            FileAction::Merge => {
                if !mzip_shell_protocol::can_merge(&paths) {
                    self.set_status("请选择同名且从 .001 开始的连续分片。", true, cx);
                    return;
                }
                "--merge-files"
            }
            FileAction::ExtractHere | FileAction::ExtractFolder | FileAction::ExtractTo => {
                if !mzip_shell_protocol::can_extract_selection(&paths) {
                    self.set_status("请选择一个可尝试解压的文件。", true, cx);
                    return;
                }
                match action {
                    FileAction::ExtractHere => "--extract-here",
                    FileAction::ExtractFolder => "--extract-folder",
                    _ => "--extract-to",
                }
            }
        };
        let task = cx
            .background_executor()
            .spawn(async move { launch_task(flag, &paths) });
        cx.spawn(async move |view, cx| {
            let result = task.await;
            let _ = view.update(cx, |this, cx| match result {
                Ok(()) => this.set_status("已打开操作窗口；完成后可刷新文件列表。", false, cx),
                Err(error) => this.set_status(format!("{error:#}"), true, cx),
            });
        })
        .detach();
    }

    fn file_menu(&self, mut menu: PopupMenu, view: Entity<Self>) -> PopupMenu {
        if self.in_archive() {
            let busy = self.running.is_some() || self.explorer.loading;
            let selected = !self.explorer.selected.is_empty();
            let editable = self.archive.as_ref().is_some_and(|info| {
                matches!(info.format, ArchiveKind::Zip | ArchiveKind::SevenZip)
            });
            let open_view = view.clone();
            let add_view = view.clone();
            let extract_view = view.clone();
            return menu
                .item(
                    PopupMenuItem::new("打开")
                        .disabled(busy || self.explorer.selected.len() != 1)
                        .on_click(move |_, _, cx| {
                            open_view.update(cx, |this, cx| this.file_action(FileAction::Open, cx))
                        }),
                )
                .item(
                    PopupMenuItem::new("添加文件…")
                        .disabled(busy || !editable)
                        .on_click(move |_, _, cx| {
                            add_view.update(cx, |this, cx| this.choose_archive_add(cx))
                        }),
                )
                .item(
                    PopupMenuItem::new("解压所选到…")
                        .disabled(busy || !selected)
                        .on_click(move |_, _, cx| {
                            extract_view.update(cx, |this, cx| this.extract_archive_selection(cx))
                        }),
                )
                .item(
                    PopupMenuItem::new("删除")
                        .disabled(busy || !selected || !editable)
                        .on_click(move |_, _, cx| {
                            view.update(cx, |this, cx| this.delete_archive_selection(cx))
                        }),
                );
        }
        let paths = self.explorer.selected_paths();
        let selected = !paths.is_empty() && !self.explorer.loading;
        let archive = paths.len() == 1
            && self.explorer.selected.first().is_some_and(|index| {
                !self.explorer.entries[*index].directory
                    && mzip_shell_protocol::can_attempt_extraction(&paths[0])
            });
        let compressible = selected && paths.iter().all(|path| path.file_name().is_some());
        for (label, action, enabled) in [
            (
                "打开",
                FileAction::Open,
                paths.len() == 1 && selected && self.running.is_none(),
            ),
            ("创建 ZIP", FileAction::Zip, compressible),
            (
                "压缩每个到单独的压缩包中",
                FileAction::EachZip,
                compressible,
            ),
            ("创建 7z", FileAction::SevenZip, compressible),
            ("添加到压缩文件…", FileAction::Add, compressible),
            (
                "解压到当前文件夹",
                FileAction::ExtractHere,
                archive && selected,
            ),
            (
                "解压到压缩文件名文件夹",
                FileAction::ExtractFolder,
                archive && selected,
            ),
            ("解压到…", FileAction::ExtractTo, archive && selected),
        ] {
            if !archive
                && matches!(
                    action,
                    FileAction::ExtractHere | FileAction::ExtractFolder | FileAction::ExtractTo
                )
            {
                continue;
            }
            let view = view.clone();
            menu = menu.item(PopupMenuItem::new(label).disabled(!enabled).on_click(
                move |_, _, cx| view.update(cx, |this, cx| this.file_action(action, cx)),
            ));
        }
        for (label, action, visible) in [
            (
                "分割文件",
                FileAction::Split,
                selected && mzip_shell_protocol::can_split(&paths),
            ),
            (
                "合并文件",
                FileAction::Merge,
                selected && mzip_shell_protocol::can_merge(&paths),
            ),
        ] {
            if visible {
                let view = view.clone();
                menu = menu.item(PopupMenuItem::new(label).on_click(move |_, _, cx| {
                    view.update(cx, |this, cx| this.file_action(action, cx));
                }));
            }
        }
        menu.separator().item(
            PopupMenuItem::new("在 Windows 资源管理器中打开")
                .disabled(!matches!(self.explorer.location, Location::Directory(_)))
                .on_click(move |_, _, cx| {
                    view.update(cx, |this, cx| {
                        this.file_action(FileAction::ShowInExplorer, cx)
                    });
                }),
        )
    }

    pub(super) fn render_main_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity();
        let tools = Button::new("main-file-menu")
            .ghost()
            .small()
            .label("文件")
            .dropdown_menu(move |menu, _, _| {
                let mut menu = menu;
                for (label, page) in [
                    ("资源管理器", Page::Explorer),
                    ("浏览与解压（原界面）", Page::Browse),
                    ("创建压缩包（原界面）", Page::Create),
                    ("设置", Page::Settings),
                ] {
                    let view = view.clone();
                    menu = menu.item(PopupMenuItem::new(label).on_click(move |_, _, cx| {
                        view.update(cx, |this, cx| {
                            this.page = page;
                            if page == Page::Explorer {
                                this.initialize_explorer(cx);
                            }
                            cx.notify();
                        });
                    }));
                }
                let view = view.clone();
                let folder_view = view.clone();
                menu.separator()
                    .item(PopupMenuItem::new("打开文件夹…").on_click(move |_, _, cx| {
                        folder_view.update(cx, |this, cx| {
                            if let Some(path) =
                                rfd::FileDialog::new().set_title("打开文件夹").pick_folder()
                            {
                                this.open_directory(path, cx);
                            }
                        });
                    }))
                    .item(PopupMenuItem::new("打开压缩包…").on_click(move |_, _, cx| {
                        view.update(cx, |this, cx| this.choose_archive(cx));
                    }))
            });
        let view = cx.entity();
        let operations = Button::new("main-operation-menu")
            .ghost()
            .small()
            .label("操作")
            .disabled(self.page != Page::Explorer)
            .dropdown_menu(move |menu, _, cx| view.read(cx).file_menu(menu, view.clone()));
        let view = cx.entity();
        let api_tools = Button::new("main-tools-menu")
            .ghost()
            .small()
            .label("工具")
            .dropdown_menu(move |menu, _, _| {
                let view = view.clone();
                menu.item(
                    PopupMenuItem::new("API 测试工具").on_click(move |_, _, cx| {
                        view.update(cx, |this, cx| match (this.api_tool_launcher)() {
                            Ok(()) => this.set_status("正在打开 API 测试工具。", false, cx),
                            Err(error) => this.set_status(format!("{error:#}"), true, cx),
                        });
                    }),
                )
            });
        let help = Button::new("main-help-menu")
            .ghost()
            .small()
            .label("帮助")
            .dropdown_menu(|menu, _, _| {
                menu.item(PopupMenuItem::new("关于 MZip").on_click(|_, _, _| {
                    super::show_about();
                }))
            });
        div()
            .h(px(42.0))
            .px_3()
            .flex()
            .items_center()
            .gap_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .text_size(px(14.0))
                    .font_weight(FontWeight::BOLD)
                    .child("MZip"),
            )
            .child(tools)
            .child(operations)
            .child(api_tools)
            .child(help)
            .child(div().flex_1())
            .when(cfg!(windows), |row| {
                #[cfg(windows)]
                let row = row.child(
                    Button::new("menu-default-apps")
                        .ghost()
                        .small()
                        .label("默认应用")
                        .on_click(cx.listener(|this, _, _, cx| this.open_default_apps(cx))),
                );
                row
            })
            .child(
                Button::new("menu-settings")
                    .ghost()
                    .small()
                    .icon(IconName::Settings)
                    .label(self.integrity_mode.label())
                    .tooltip("打开设置：安全模式 / 高速模式")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.page = Page::Settings;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("menu-theme")
                    .ghost()
                    .small()
                    .icon(if self.dark {
                        IconName::Sun
                    } else {
                        IconName::Moon
                    })
                    .label(if self.dark { "浅色" } else { "深色" })
                    .on_click(cx.listener(|this, _, window, cx| this.change_theme(window, cx))),
            )
    }

    pub(super) fn render_explorer_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut locations = vec![("此电脑", Location::Computer)];
        if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
            let home = PathBuf::from(home);
            locations.push(("用户文件夹", Location::Directory(home)));
        }
        div()
            .w(px(180.0))
            .h_full()
            .p_3()
            .flex()
            .flex_col()
            .gap_1()
            .bg(cx.theme().sidebar)
            .border_r_1()
            .border_color(cx.theme().border)
            .child(div().px_2().py_2().text_size(px(11.0)).child("常用位置"))
            .children(
                locations
                    .into_iter()
                    .enumerate()
                    .map(|(index, (label, location))| {
                        Button::new(("explorer-location", index))
                            .ghost()
                            .w_full()
                            .icon(IconName::FolderClosed)
                            .label(label)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.explorer.initialized = true;
                                this.navigate(location.clone(), None, cx);
                            }))
                    }),
            )
            .children(
                ["桌面", "下载", "文档"]
                    .into_iter()
                    .enumerate()
                    .map(|(index, label)| {
                        Button::new(("explorer-known-folder", index))
                            .ghost()
                            .w_full()
                            .icon(IconName::FolderClosed)
                            .label(label)
                            .on_click(cx.listener(move |this, _, _, cx| match user_folder(index) {
                                Ok(path) => this.open_directory(path, cx),
                                Err(error) => this.set_status(format!("{error:#}"), true, cx),
                            }))
                    }),
            )
            .child(div().flex_1())
            .child(div().text_size(px(11.0)).child("选择文件后右键操作"))
    }

    pub(super) fn render_explorer(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.initialize_explorer(cx);
        let address = self.explorer.location.label();
        if self.explorer.displayed_address != address {
            self.explorer
                .address
                .update(cx, |input, cx| input.set_value(address.clone(), window, cx));
            self.explorer.displayed_address = address;
        }
        let busy = self.explorer.loading || self.running.is_some();
        let hide_list = self.explorer.loading
            || (self.running.is_some() && self.job_kind != super::JobKind::Drag);
        let writable = self.archive.as_ref().is_some_and(|info| {
            matches!(info.format, ArchiveKind::Zip | ArchiveKind::SevenZip)
                && mzip::archive::ArchiveFormat::from_path(&info.path).is_ok()
        });
        let view = cx.entity();
        let marquee_geometry = self
            .explorer
            .marquee
            .as_ref()
            .filter(|marquee| marquee.is_active())
            .map(|marquee| (marquee.start_content, marquee.pointer));
        let marquee_scroll = self.explorer.scroll.0.borrow().base_handle.clone();
        let marquee_view = view.downgrade();
        let marquee_color = cx.theme().primary;
        let marquee_layer = canvas(
            |_, _, _| (),
            move |viewport, _, window, _| {
                if let Some((start_content, pointer)) = marquee_geometry {
                    let start = viewport.origin + marquee_scroll.offset() + start_content;
                    let left = start.x.min(pointer.x);
                    let top = start.y.min(pointer.y);
                    let right = start.x.max(pointer.x).max(left + px(1.0));
                    let bottom = start.y.max(pointer.y).max(top + px(1.0));
                    let bounds = Bounds::from_corners(point(left, top), point(right, bottom));
                    window.with_content_mask(Some(ContentMask { bounds: viewport }), |window| {
                        window.paint_quad(
                            fill(bounds, marquee_color.opacity(0.16))
                                .border_widths(px(1.0))
                                .border_color(marquee_color),
                        );
                    });
                }
                let down_view = marquee_view.clone();
                window.on_mouse_event(move |event: &MouseDownEvent, phase, _, cx| {
                    if phase.capture() && event.button == MouseButton::Left {
                        let _ = down_view.update(cx, |this, cx| {
                            this.explorer.drag_origin = None;
                            if this.explorer.cancel_marquee() {
                                cx.notify();
                            }
                            this.explorer.suppress_marquee_release = false;
                        });
                    }
                });
                let move_view = marquee_view.clone();
                window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                    if !phase.capture() {
                        return;
                    }
                    let _ = move_view.update(cx, |this, cx| {
                        if event.pressed_button == Some(MouseButton::Left) {
                            if let Some((index, origin)) = this.explorer.drag_origin {
                                if (event.position - origin).magnitude() > MARQUEE_THRESHOLD {
                                    if !this.explorer.selected.contains(&index) {
                                        this.explorer.selected.clear();
                                        this.explorer.selected.insert(index);
                                    }
                                    this.explorer.last_clicked_row = None;
                                    this.explorer.suppress_marquee_release = true;
                                    this.explorer.drag_origin = None;
                                    this.begin_archive_drag(index, cx);
                                    cx.notify();
                                    cx.stop_propagation();
                                }
                                return;
                            }
                            if let Some((id, started)) =
                                this.explorer.update_marquee(event.position)
                            {
                                if started {
                                    Self::start_marquee_scroll(id, cx);
                                }
                                cx.notify();
                            }
                        } else {
                            this.explorer.drag_origin = None;
                            if this.explorer.finish_marquee() {
                                cx.notify();
                            }
                        }
                    });
                });
                let mut suppress_release = false;
                // Let every control clear its pressed state before suppressing the click.
                window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                    if event.button != MouseButton::Left {
                        return;
                    }
                    if phase.capture() {
                        if let Ok(suppress) = marquee_view.update(cx, |this, cx| {
                            this.explorer.drag_origin = None;
                            this.explorer.update_marquee(event.position);
                            let suppress = this.explorer.finish_marquee();
                            if suppress {
                                cx.notify();
                            }
                            suppress
                        }) {
                            suppress_release = suppress;
                        }
                    } else if suppress_release {
                        suppress_release = false;
                        cx.stop_propagation();
                    }
                });
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full();
        let list = uniform_list(
            "explorer-files",
            self.explorer.visible.len(),
            cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                range
                    .filter_map(|row| {
                        let index = *this.explorer.visible.get(row)?;
                        let entry = &this.explorer.entries[index];
                        let selected = this.explorer.selected.contains(&index);
                        let name = entry.name.clone();
                        let kind = if entry.directory {
                            "文件夹".to_owned()
                        } else if ArchiveKind::from_path(&entry.path).is_ok() {
                            "压缩文件".to_owned()
                        } else {
                            entry
                                .path
                                .extension()
                                .map(|ext| format!("{} 文件", ext.to_string_lossy().to_uppercase()))
                                .unwrap_or_else(|| "文件".to_owned())
                        };
                        let modified = entry
                            .modified
                            .map(|time| {
                                chrono::DateTime::<chrono::Local>::from(time)
                                    .format("%Y-%m-%d %H:%M")
                                    .to_string()
                            })
                            .unwrap_or_else(|| "—".to_owned());
                        let size = if entry.directory {
                            "—".to_owned()
                        } else {
                            format_size(entry.size)
                        };
                        let view = cx.entity();
                        Some(
                            div()
                                .id(("explorer-row", index))
                                .h(FILE_ROW_HEIGHT)
                                .w_full()
                                .px_3()
                                .flex()
                                .items_center()
                                .gap_2()
                                .text_size(px(12.0))
                                .bg(if selected {
                                    cx.theme().accent
                                } else {
                                    cx.theme().background
                                })
                                .hover(|style| style.bg(cx.theme().muted))
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                                        if this.in_archive() && this.running.is_none() {
                                            this.explorer.cancel_marquee();
                                            this.explorer.suppress_marquee_release = false;
                                            this.explorer.drag_origin =
                                                Some((index, event.position));
                                            this.explorer.focus.focus(window, cx);
                                            cx.stop_propagation();
                                        }
                                    }),
                                )
                                .on_mouse_down(
                                    MouseButton::Right,
                                    cx.listener(move |this, _, _, cx| {
                                        this.explorer.cancel_marquee();
                                        this.explorer.last_clicked_row = None;
                                        if !this.explorer.selected.contains(&index) {
                                            this.explorer.selected.clear();
                                            this.explorer.selected.insert(index);
                                            this.explorer.anchor = Some(row);
                                        }
                                        this.explorer.cursor = Some(row);
                                        cx.notify();
                                    }),
                                )
                                .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                                    if this.explorer.loading {
                                        return;
                                    }
                                    if event.click_count() > 1
                                        && this.explorer.last_clicked_row == Some(index)
                                    {
                                        this.explorer.last_clicked_row = None;
                                        this.open_file_entry(index, cx);
                                    } else {
                                        this.explorer.last_clicked_row = Some(index);
                                        let modifiers = event.modifiers();
                                        select_row(
                                            &mut this.explorer.selected,
                                            &mut this.explorer.anchor,
                                            &this.explorer.visible,
                                            row,
                                            modifiers.control,
                                            modifiers.shift,
                                        );
                                        this.explorer.cursor = Some(row);
                                        cx.notify();
                                    }
                                }))
                                .child(
                                    div()
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(|this, _, window, cx| {
                                                this.explorer.last_clicked_row = None;
                                                this.explorer.focus.focus(window, cx);
                                                cx.stop_propagation();
                                            }),
                                        )
                                        .child(
                                            Checkbox::new(("explorer-check", index))
                                                .checked(selected)
                                                .accessibility_label(format!("选择 {name}"))
                                                .disabled(this.explorer.loading)
                                                .on_click(cx.listener(
                                                    move |this, checked, _, cx| {
                                                        cx.stop_propagation();
                                                        this.explorer.last_clicked_row = None;
                                                        if *checked {
                                                            this.explorer.selected.insert(index);
                                                        } else {
                                                            this.explorer.selected.remove(&index);
                                                        }
                                                        this.explorer.anchor = Some(row);
                                                        this.explorer.cursor = Some(row);
                                                        cx.notify();
                                                    },
                                                )),
                                        ),
                                )
                                .child(
                                    Icon::new(if entry.directory {
                                        IconName::FolderClosed
                                    } else {
                                        IconName::File
                                    })
                                    .small(),
                                )
                                .child(div().flex_1().min_w_0().truncate().child(name))
                                .child(div().w(px(110.0)).truncate().child(kind))
                                .child(div().w(px(90.0)).child(size))
                                .child(div().w(px(145.0)).child(modified))
                                .context_menu(move |menu, _, cx| {
                                    view.read(cx).file_menu(menu, view.clone())
                                }),
                        )
                    })
                    .collect()
            }),
        )
        .size_full()
        .track_scroll(&self.explorer.scroll);
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_2()
            .track_focus(&self.explorer.focus)
            .on_key_down(cx.listener(|this, event, _, cx| this.explorer_key(event, cx)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .py_2()
                    .child(
                        Button::new("explorer-back")
                            .ghost()
                            .small()
                            .label("←")
                            .accessibility_label("后退")
                            .disabled(self.explorer.history_index == 0 || busy)
                            .on_click(cx.listener(|this, _, _, cx| this.travel(false, cx))),
                    )
                    .child(
                        Button::new("explorer-forward")
                            .ghost()
                            .small()
                            .label("→")
                            .accessibility_label("前进")
                            .disabled(
                                self.explorer.history_index + 1 >= self.explorer.history.len()
                                    || busy,
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.travel(true, cx))),
                    )
                    .child(
                        Button::new("explorer-up")
                            .ghost()
                            .small()
                            .label("↑")
                            .accessibility_label("上一级")
                            .disabled(self.explorer.location.parent().is_none() || busy)
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(parent) = this.explorer.location.parent() {
                                    this.navigate(parent, None, cx);
                                }
                            })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(&self.explorer.address)),
                    )
                    .child(
                        Button::new("explorer-go")
                            .ghost()
                            .small()
                            .label("前往")
                            .on_click(cx.listener(|this, _, _, cx| this.go_to_address(cx))),
                    )
                    .child(
                        Button::new("explorer-refresh")
                            .ghost()
                            .small()
                            .label("刷新")
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.navigate(this.explorer.location.clone(), None, cx)
                            })),
                    )
                    .child(
                        div().w(px(210.0)).child(
                            Input::new(&self.explorer.search)
                                .prefix(IconName::Search)
                                .cleanable(true),
                        ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_size(px(12.0))
                    .when(self.in_archive(), |row| {
                        row.child(
                            Button::new("archive-add")
                                .small()
                                .label("添加")
                                .disabled(busy || !writable)
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.choose_archive_add(cx)),
                                ),
                        )
                        .child(
                            Button::new("archive-add-folder")
                                .small()
                                .label("添加文件夹")
                                .disabled(busy || !writable)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(path) = rfd::FileDialog::new().pick_folder() {
                                        this.add_to_open_archive(vec![path], cx);
                                    }
                                })),
                        )
                        .child(
                            Button::new("archive-extract")
                                .small()
                                .label("解压所选")
                                .disabled(busy || self.explorer.selected.is_empty())
                                .on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.extract_archive_selection(cx)
                                    }),
                                ),
                        )
                        .child(
                            Button::new("archive-delete")
                                .small()
                                .label("删除")
                                .disabled(busy || !writable || self.explorer.selected.is_empty())
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.delete_archive_selection(cx)),
                                ),
                        )
                    })
                    .child(format!(
                        "{} 项 · 已选 {}",
                        self.explorer.visible.len(),
                        self.explorer.selected.len()
                    ))
                    .child(div().flex_1())
                    .child(
                        Button::new("explorer-select-all")
                            .ghost()
                            .small()
                            .label("全选")
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.explorer
                                    .selected
                                    .extend(this.explorer.visible.iter().copied());
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("explorer-clear")
                            .ghost()
                            .small()
                            .label("清除选择")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.explorer.selected.clear();
                                cx.notify();
                            })),
                    ),
            )
            .when(self.last_open_path.is_some(), |column| {
                column.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(div().w(px(250.0)).child(Input::new(&self.archive_password)))
                        .child(
                            Button::new("archive-reopen")
                                .small()
                                .label("用密码重新打开")
                                .disabled(busy)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(path) = this.last_open_path.clone() {
                                        this.open_path(path, cx);
                                    }
                                })),
                        )
                        .child(div().text_size(px(12.0)).child(
                            if self.in_archive() && !writable {
                                "此格式只读 · 可浏览和拖出解压"
                            } else {
                                "包内拖出解压 · 拖入添加 · Delete 删除"
                            },
                        )),
                )
            })
            .child(
                div()
                    .h(px(32.0))
                    .px_3()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_size(px(12.0))
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(div().w(px(40.0)))
                    .child(div().flex_1().child("名称"))
                    .child(div().w(px(110.0)).child("类型"))
                    .child(div().w(px(90.0)).child("大小"))
                    .child(div().w(px(145.0)).child("修改时间")),
            )
            .child(
                div()
                    .id("explorer-list-area")
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event, window, cx| {
                            this.explorer.focus.focus(window, cx);
                            this.explorer.begin_marquee(event);
                        }),
                    )
                    .when(hide_list || self.explorer.visible.is_empty(), |element| {
                        element.child(div().p_4().text_size(px(13.0)).child(if hide_list {
                            "正在处理，请稍候…"
                        } else {
                            "此文件夹为空，或没有匹配的项目。"
                        }))
                    })
                    .when(!hide_list && !self.explorer.visible.is_empty(), |element| {
                        element.child(list).child(marquee_layer)
                    })
                    .context_menu(move |menu, _, cx| view.read(cx).file_menu(menu, view.clone())),
            )
            .into_any_element()
    }
}

pub(super) fn launch_api_tester() -> Result<()> {
    launch_task(crate::API_TESTER_FLAG, &[])
}

fn launch_task(flag: &str, paths: &[PathBuf]) -> Result<()> {
    let executable = std::env::current_exe().context("无法确定 MZip 程序路径")?;
    #[cfg(windows)]
    {
        use mzip_shell_protocol::{CompressionAction, launch_compression_paths};
        let action = match flag {
            "--create-zip" => Some(CompressionAction::Zip),
            "--create-each-zip" => Some(CompressionAction::EachZip),
            "--create-7z" => Some(CompressionAction::SevenZip),
            "--add-to-archive" => Some(CompressionAction::Configure),
            "--split-file" => Some(CompressionAction::Split),
            "--merge-files" => Some(CompressionAction::Merge),
            _ => None,
        };
        if let Some(action) = action {
            return launch_compression_paths(&executable, paths, action);
        }
    }
    let mut command = std::process::Command::new(executable);
    command.arg(flag).args(paths);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    command.spawn().context("无法打开 MZip 操作窗口")?;
    Ok(())
}

fn user_folder(index: usize) -> Result<PathBuf> {
    #[cfg(windows)]
    {
        use std::{ffi::OsString, os::windows::ffi::OsStringExt};
        use windows::Win32::{
            System::Com::CoTaskMemFree,
            UI::Shell::{
                FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads, KF_FLAG_DEFAULT,
                SHGetKnownFolderPath,
            },
        };
        let folders = [FOLDERID_Desktop, FOLDERID_Downloads, FOLDERID_Documents];
        let folder = folders.get(index).context("未知的常用文件夹")?;
        let path = unsafe { SHGetKnownFolderPath(folder, KF_FLAG_DEFAULT, None) }
            .context("无法获取系统常用文件夹位置")?;
        let owned = unsafe { OsString::from_wide(path.as_wide()) };
        unsafe { CoTaskMemFree(Some(path.0.cast())) };
        Ok(PathBuf::from(owned))
    }
    #[cfg(not(windows))]
    {
        let names = ["Desktop", "Downloads", "Documents"];
        let name = names.get(index).context("未知的常用文件夹")?;
        let home = std::env::var_os("HOME").context("无法确定用户文件夹")?;
        Ok(PathBuf::from(home).join(name))
    }
}

pub(super) fn open_default(path: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows::{
            Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL},
            core::{PCWSTR, w},
        };
        let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let result = unsafe {
            ShellExecuteW(
                None,
                w!("open"),
                PCWSTR(path.as_ptr()),
                None,
                None,
                SW_SHOWNORMAL,
            )
        };
        anyhow::ensure!(
            result.0 as isize > 32,
            "无法使用默认应用打开文件（Shell 错误 {}）",
            result.0 as isize
        );
        Ok(())
    }
    #[cfg(not(windows))]
    super::open_in_file_manager(path)
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, fs, path::PathBuf, time::Duration};

    use gpui_kit as gpui;
    use gpui_kit::{
        AppContext, Bounds, Entity, Modifiers, MouseButton, MouseDownEvent, MouseUpEvent,
        ScrollDelta, ScrollEvent, TestAppContext, VisualTestContext, base::Root, point, px, size,
    };

    use super::{
        AppView, FILE_ROW_HEIGHT, FileEntry, Location, marquee_rows, marquee_scroll_delta,
        range_difference, read_location, select_row, update_marquee_rows,
    };

    #[test]
    fn directory_listing_sorts_folders_first_and_preserves_metadata() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("a.txt"), b"abc").unwrap();
        fs::create_dir(temp.path().join("z-folder")).unwrap();
        fs::write(temp.path().join("B.zip"), b"zip").unwrap();
        let entries = read_location(&Location::Directory(temp.path().to_path_buf())).unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["z-folder", "a.txt", "B.zip"]
        );
        assert!(entries[0].directory);
        assert_eq!(entries[1].size, 3);
        assert!(entries[1].modified.is_some());
    }

    #[test]
    fn missing_directory_is_an_explicit_error() {
        let temp = tempfile::tempdir().unwrap();
        assert!(read_location(&Location::Directory(temp.path().join("missing"))).is_err());
    }

    #[test]
    fn root_parent_is_computer() {
        #[cfg(windows)]
        let root = PathBuf::from("C:\\");
        #[cfg(not(windows))]
        let root = PathBuf::from("/");
        assert_eq!(Location::Directory(root).parent(), Some(Location::Computer));
        assert_eq!(Location::Computer.parent(), None);
    }

    #[gpui_kit::test]
    fn archive_opens_in_main_explorer_with_folder_navigation_and_history(cx: &mut TestAppContext) {
        use mzip::archive::{
            ArchiveFormat, CreateOptions, JobControl, create_archive, inspect_archive,
        };
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("tree");
        fs::create_dir_all(source.join("nested")).unwrap();
        fs::write(source.join("nested").join("hello.txt"), b"hello").unwrap();
        fs::write(source.join("keep.txt"), b"keep").unwrap();
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
        let (view, cx) = explorer_fixture(cx, 0);
        view.update(cx, |app, cx| app.show_archive(info, false, cx));
        let moved_archive = temp.path().join("moved.zip");
        fs::rename(&path, &moved_archive).unwrap();
        draw(cx);
        view.read_with(cx, |app, _| {
            assert!(app.page == super::Page::Explorer);
            assert_eq!(app.explorer.entries.len(), 1);
            assert_eq!(app.explorer.entries[0].name, "tree");
            assert_eq!(
                app.explorer.location,
                Location::Archive {
                    path: path.clone(),
                    directory: String::new()
                }
            );
        });
        let bounds = viewport(&view, cx);
        let row = point(bounds.left() + px(100.0), bounds.top() + px(18.0));
        cx.simulate_click(row, Modifiers::default());
        draw(cx);
        click_with_count(cx, row, 2);
        cx.run_until_parked();
        draw(cx);
        view.read_with(cx, |app, _| {
            assert_eq!(
                app.explorer.location,
                Location::Archive {
                    path: path.clone(),
                    directory: "tree".into()
                }
            );
            assert_eq!(
                app.explorer
                    .entries
                    .iter()
                    .map(|e| e.name.as_str())
                    .collect::<Vec<_>>(),
                ["nested", "keep.txt"]
            );
        });
        view.update(cx, |app, cx| app.travel(false, cx));
        cx.run_until_parked();
        draw(cx);
        assert_eq!(
            view.read_with(cx, |app, _| app.explorer.location.clone()),
            Location::Archive {
                path: path.clone(),
                directory: String::new()
            }
        );
        view.update(cx, |app, cx| app.travel(true, cx));
        cx.run_until_parked();
        draw(cx);
        assert_eq!(view.read_with(cx, |app, _| app.explorer.entries.len()), 2);
        let location = Location::Archive {
            path: path.clone(),
            directory: String::new(),
        };
        assert_eq!(
            location.parent(),
            Some(Location::Directory(temp.path().to_path_buf()))
        );
        let bounds = viewport(&view, cx);
        let row = point(bounds.left() + px(100.0), bounds.top() + px(18.0));
        cx.simulate_mouse_down(row, MouseButton::Left, Modifiers::default());
        draw(cx);
        view.read_with(cx, |app, _| {
            assert!(app.explorer.drag_origin.is_some());
            assert!(app.explorer.marquee.is_none());
        });
        cx.simulate_mouse_up(row, MouseButton::Left, Modifiers::default());
        draw(cx);
        assert_eq!(selected(&view, cx), BTreeSet::from([0]));
        cx.simulate_mouse_down(row, MouseButton::Left, Modifiers::default());
        draw(cx);
        let moved = point(row.x + px(2.0), row.y);
        cx.simulate_mouse_move(moved, Some(MouseButton::Left), Modifiers::default());
        draw(cx);
        view.read_with(cx, |app, _| {
            assert!(
                app.running.is_none(),
                "small pointer movements must remain clicks"
            );
            assert!(app.explorer.drag_origin.is_some());
            assert!(app.explorer.marquee.is_none());
        });
        cx.simulate_mouse_up(moved, MouseButton::Left, Modifiers::default());
        draw(cx);
        assert_eq!(selected(&view, cx), BTreeSet::from([0]));
        view.update(cx, |app, cx| {
            app.navigate(app.explorer.location.clone(), None, cx)
        });
        cx.run_until_parked();
        draw(cx);
        view.read_with(cx, |app, _| {
            assert!(
                app.status_is_error,
                "refresh must reopen the archive instead of using stale cache"
            );
            assert!(!app.explorer.loading);
        });
        fs::rename(&moved_archive, &path).unwrap();
    }

    #[test]
    fn selection_supports_control_shift_and_filtered_row_order() {
        let visible = [7, 3, 10, 2];
        let mut selected = BTreeSet::new();
        let mut anchor = None;
        select_row(&mut selected, &mut anchor, &visible, 1, false, false);
        assert_eq!(selected, BTreeSet::from([3]));
        select_row(&mut selected, &mut anchor, &visible, 3, false, true);
        assert_eq!(selected, BTreeSet::from([3, 10, 2]));
        select_row(&mut selected, &mut anchor, &visible, 0, true, false);
        assert_eq!(selected, BTreeSet::from([7, 3, 10, 2]));
        select_row(&mut selected, &mut anchor, &visible, 0, true, false);
        assert_eq!(selected, BTreeSet::from([3, 10, 2]));
        select_row(&mut selected, &mut anchor, &visible, 2, false, false);
        assert_eq!(selected, BTreeSet::from([10]));
    }

    #[test]
    fn shift_without_anchor_selects_clicked_row_and_empty_lists_are_safe() {
        let mut selected = BTreeSet::new();
        let mut anchor = None;
        select_row(&mut selected, &mut anchor, &[5, 9], 1, false, true);
        assert_eq!(selected, BTreeSet::from([9]));
        select_row(&mut selected, &mut anchor, &[], 0, false, false);
        assert_eq!(selected, BTreeSet::from([9]));
    }

    #[test]
    fn marquee_geometry_uses_content_coordinates_and_clips_to_the_viewport() {
        let viewport = Bounds::new(point(px(100.0), px(200.0)), size(px(300.0), px(144.0)));
        let offset = point(px(0.0), px(-72.0));
        let start = point(px(50.0), px(82.0));
        assert_eq!(
            marquee_rows(start, point(px(180.0), px(298.0)), viewport, offset, 10),
            2..5
        );
        assert_eq!(
            marquee_rows(
                point(px(50.0), px(170.0)),
                point(px(180.0), px(210.0)),
                viewport,
                offset,
                10,
            ),
            2..5
        );
        assert_eq!(
            marquee_rows(start, point(px(150.0), px(999.0)), viewport, offset, 10),
            2..6
        );
        assert_eq!(
            marquee_rows(start, point(px(160.0), px(210.0)), viewport, offset, 10),
            2..3
        );
        assert_eq!(
            marquee_rows(
                point(px(50.0), px(36.0)),
                point(px(160.0), px(200.0)),
                viewport,
                offset,
                10,
            ),
            1..2
        );
        assert_eq!(
            marquee_rows(
                point(px(-100.0), px(82.0)),
                point(px(0.0), px(298.0)),
                viewport,
                offset,
                10,
            ),
            0..0
        );
        assert_eq!(
            marquee_rows(
                point(px(50.0), px(400.0)),
                point(px(160.0), px(344.0)),
                viewport,
                offset,
                3,
            ),
            0..0
        );
        assert_eq!(
            marquee_rows(start, point(px(180.0), px(298.0)), viewport, offset, 0),
            0..0
        );
    }

    #[test]
    fn marquee_range_difference_matches_set_difference() {
        for start in 0..6 {
            for end in start..6 {
                for other_start in 0..6 {
                    for other_end in other_start..6 {
                        let range = start..end;
                        let other = other_start..other_end;
                        let actual = range_difference(&range, &other)
                            .into_iter()
                            .flatten()
                            .collect::<BTreeSet<_>>();
                        let expected = range.filter(|row| !other.contains(row)).collect();
                        assert_eq!(actual, expected);
                    }
                }
            }
        }
    }

    #[test]
    fn marquee_updates_filtered_rows_incrementally_and_preserves_control_selection() {
        let visible = [7, 3, 10, 2, 8];
        let original = BTreeSet::from([7, 8]);
        let mut selected = original.clone();
        update_marquee_rows(&mut selected, &original, true, &visible, &(0..0), &(1..4));
        assert_eq!(selected, BTreeSet::from([7, 3, 10, 2, 8]));
        update_marquee_rows(&mut selected, &original, true, &visible, &(1..4), &(2..3));
        assert_eq!(selected, BTreeSet::from([7, 10, 8]));
        update_marquee_rows(&mut selected, &original, true, &visible, &(2..3), &(0..1));
        assert_eq!(selected, original);

        selected.clear();
        update_marquee_rows(&mut selected, &original, false, &visible, &(0..0), &(0..3));
        update_marquee_rows(&mut selected, &original, false, &visible, &(0..3), &(1..2));
        assert_eq!(selected, BTreeSet::from([3]));
    }

    #[test]
    fn marquee_auto_scroll_moves_toward_the_near_edge() {
        let viewport = Bounds::new(point(px(10.0), px(100.0)), size(px(200.0), px(200.0)));
        assert!(marquee_scroll_delta(px(100.0), viewport) > px(0.0));
        assert_eq!(marquee_scroll_delta(px(200.0), viewport), px(0.0));
        assert!(marquee_scroll_delta(px(300.0), viewport) < px(0.0));
        assert_eq!(
            marquee_scroll_delta(px(1000.0), viewport),
            marquee_scroll_delta(px(300.0), viewport)
        );
        assert_eq!(
            marquee_scroll_delta(
                px(100.0),
                Bounds::new(point(px(0.0), px(100.0)), size(px(200.0), px(0.0)))
            ),
            px(0.0)
        );
    }

    fn explorer_fixture(
        cx: &mut TestAppContext,
        count: usize,
    ) -> (Entity<AppView>, &mut VisualTestContext) {
        cx.update(gpui_kit::init);
        let (root, cx) = cx.add_window_view(move |window, cx| {
            let view = cx.new(|cx| {
                let mut app = AppView::new(window, cx);
                app.explorer.initialized = true;
                app.explorer.entries = (0..count)
                    .map(|index| FileEntry {
                        path: PathBuf::from(format!("file-{index}.txt")),
                        name: format!("file-{index}.txt"),
                        directory: false,
                        size: 0,
                        modified: None,
                    })
                    .collect();
                app.explorer.visible = (0..count).collect();
                app
            });
            Root::new(view, window, cx)
        });
        let view = root.read_with(cx, |root, _| {
            root.view().clone().downcast::<AppView>().unwrap()
        });
        draw(cx);
        (view, cx)
    }

    fn draw(cx: &mut VisualTestContext) {
        cx.update(|window, cx| window.draw(cx).clear(cx));
    }

    fn click_with_count(cx: &mut VisualTestContext, position: gpui::Point<gpui::Pixels>, n: usize) {
        cx.simulate_event(MouseDownEvent {
            button: MouseButton::Left,
            position,
            modifiers: Modifiers::default(),
            click_count: n,
            first_mouse: false,
        });
        draw(cx);
        cx.simulate_event(MouseUpEvent {
            button: MouseButton::Left,
            position,
            modifiers: Modifiers::default(),
            click_count: n,
        });
        draw(cx);
    }

    fn selected(view: &Entity<AppView>, cx: &VisualTestContext) -> BTreeSet<usize> {
        view.read_with(cx, |app, _| app.explorer.selected.clone())
    }

    fn viewport(view: &Entity<AppView>, cx: &VisualTestContext) -> Bounds<gpui::Pixels> {
        view.read_with(cx, |app, _| {
            app.explorer.scroll.0.borrow().base_handle.bounds()
        })
    }

    fn scroll_y(view: &Entity<AppView>, cx: &VisualTestContext) -> gpui::Pixels {
        view.read_with(cx, |app, _| {
            app.explorer.scroll.0.borrow().base_handle.offset().y
        })
    }

    #[cfg(feature = "ui-tests")]
    #[gpui_kit::test]
    fn api_tool_menu_dispatches_and_reports_launch_errors(cx: &mut TestAppContext) {
        use gpui_kit::test::{TestQueryExt, TestWindowExt};

        let (view, cx) = explorer_fixture(cx, 0);
        let open_menu = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| {
                window.render_frame(cx);
                window.click("main-tools-menu", cx);
                let id = window
                    .find_by_label("API 测试工具")
                    .path()
                    .last()
                    .unwrap()
                    .clone();
                window.within("popup-menu").click(id, cx);
            });
        };

        for page in [
            super::Page::Explorer,
            super::Page::Browse,
            super::Page::Create,
            super::Page::Settings,
        ] {
            cx.update(|_, cx| {
                view.update(cx, |app, cx| {
                    app.page = page;
                    app.status.clear();
                    app.api_tool_launcher = || Ok(());
                    cx.notify();
                })
            });
            open_menu(cx);
            view.read_with(cx, |app, _| {
                assert_eq!(app.status, "正在打开 API 测试工具。");
                assert!(!app.status_is_error);
            });
        }

        cx.update(|_, cx| {
            view.update(cx, |app, _| {
                app.api_tool_launcher = || Err(anyhow::anyhow!("测试启动失败"));
            })
        });
        open_menu(cx);
        view.read_with(cx, |app, _| {
            assert!(app.status.contains("测试启动失败"));
            assert!(app.status_is_error);
        });
    }

    #[gpui_kit::test]
    fn drag_selection_preserves_click_checkbox_and_modifier_behavior(cx: &mut TestAppContext) {
        let (view, cx) = explorer_fixture(cx, 12);
        let viewport = viewport(&view, cx);
        let row = |index: usize| {
            point(
                viewport.left() + px(100.0),
                viewport.top() + FILE_ROW_HEIGHT * index + px(18.0),
            )
        };
        let normal = Modifiers::default();
        let control = Modifiers {
            control: true,
            ..normal
        };
        let shift = Modifiers {
            shift: true,
            ..normal
        };

        cx.simulate_mouse_down(row(0), MouseButton::Left, normal);
        draw(cx);
        cx.simulate_mouse_move(row(2), Some(MouseButton::Left), normal);
        draw(cx);
        assert_eq!(selected(&view, cx), BTreeSet::from([0, 1, 2]));
        cx.simulate_mouse_up(row(2), MouseButton::Left, normal);
        draw(cx);
        assert_eq!(selected(&view, cx), BTreeSet::from([0, 1, 2]));

        cx.simulate_click(row(1), normal);
        draw(cx);
        cx.simulate_click(row(3), control);
        draw(cx);
        assert_eq!(selected(&view, cx), BTreeSet::from([1, 3]));
        cx.simulate_click(row(5), shift);
        draw(cx);
        assert_eq!(selected(&view, cx), BTreeSet::from([3, 4, 5]));

        cx.simulate_mouse_down(row(0), MouseButton::Left, control);
        draw(cx);
        cx.simulate_mouse_move(row(1), Some(MouseButton::Left), control);
        draw(cx);
        assert_eq!(selected(&view, cx), BTreeSet::from([0, 1, 3, 4, 5]));
        let same_row = point(row(0).x + px(10.0), row(0).y);
        cx.simulate_mouse_move(same_row, Some(MouseButton::Left), control);
        draw(cx);
        assert_eq!(selected(&view, cx), BTreeSet::from([0, 3, 4, 5]));
        cx.simulate_mouse_up(same_row, MouseButton::Left, control);
        draw(cx);
        assert_eq!(selected(&view, cx), BTreeSet::from([0, 3, 4, 5]));

        let checkbox = point(viewport.left() + px(20.0), row(0).y);
        cx.simulate_click(checkbox, normal);
        draw(cx);
        assert_eq!(selected(&view, cx), BTreeSet::from([3, 4, 5]));
        cx.simulate_mouse_down(checkbox, MouseButton::Left, normal);
        draw(cx);
        cx.simulate_mouse_move(row(2), Some(MouseButton::Left), normal);
        draw(cx);
        assert_eq!(selected(&view, cx), BTreeSet::from([3, 4, 5]));
        cx.simulate_mouse_up(row(2), MouseButton::Left, normal);
        draw(cx);
        assert_eq!(selected(&view, cx), BTreeSet::from([3, 4, 5]));
    }

    #[gpui_kit::test]
    fn drag_selection_scrolls_at_edges_and_stops_after_outside_release(cx: &mut TestAppContext) {
        let (view, cx) = explorer_fixture(cx, 100);
        let viewport = viewport(&view, cx);
        let normal = Modifiers::default();
        let start = point(viewport.left() + px(100.0), viewport.top() + px(18.0));
        let below = point(viewport.left() + px(120.0), viewport.bottom() + px(30.0));
        cx.simulate_mouse_down(start, MouseButton::Left, normal);
        draw(cx);
        cx.simulate_mouse_move(below, Some(MouseButton::Left), normal);
        draw(cx);
        let before = selected(&view, cx).len();
        for _ in 0..8 {
            cx.executor().advance_clock(Duration::from_millis(32));
            cx.run_until_parked();
            draw(cx);
        }
        assert!(scroll_y(&view, cx) < px(0.0));
        assert!(selected(&view, cx).len() > before);
        cx.simulate_mouse_up(below, MouseButton::Left, normal);
        draw(cx);
        let stopped_at = scroll_y(&view, cx);
        let finished = selected(&view, cx);
        cx.executor().advance_clock(Duration::from_millis(96));
        cx.run_until_parked();
        draw(cx);
        assert_eq!(scroll_y(&view, cx), stopped_at);
        assert_eq!(selected(&view, cx), finished);

        let start = point(viewport.left() + px(100.0), viewport.bottom() - px(18.0));
        let above = point(viewport.left() + px(120.0), viewport.top() - px(30.0));
        cx.simulate_mouse_down(start, MouseButton::Left, normal);
        draw(cx);
        cx.simulate_mouse_move(above, Some(MouseButton::Left), normal);
        draw(cx);
        let before = selected(&view, cx).len();
        for _ in 0..4 {
            cx.executor().advance_clock(Duration::from_millis(32));
            cx.run_until_parked();
            draw(cx);
        }
        assert!(scroll_y(&view, cx) > stopped_at);
        assert!(selected(&view, cx).len() > before);
        cx.simulate_mouse_up(above, MouseButton::Left, normal);
        draw(cx);
    }

    #[gpui_kit::test]
    fn drag_from_blank_space_uses_filtered_order_and_supports_reverse_selection(
        cx: &mut TestAppContext,
    ) {
        let (view, cx) = explorer_fixture(cx, 10);
        view.update(cx, |app, cx| {
            app.explorer.visible = vec![9, 3, 7, 1];
            cx.notify();
        });
        draw(cx);
        let viewport = viewport(&view, cx);
        let row = |index: usize| {
            point(
                viewport.left() + px(100.0),
                viewport.top() + FILE_ROW_HEIGHT * index + px(18.0),
            )
        };
        let blank = point(
            viewport.left() + px(100.0),
            viewport.top() + FILE_ROW_HEIGHT * 4 + px(20.0),
        );
        assert!(blank.y < viewport.bottom());
        let normal = Modifiers::default();
        cx.simulate_mouse_down(blank, MouseButton::Left, normal);
        draw(cx);
        cx.simulate_mouse_move(row(1), Some(MouseButton::Left), normal);
        draw(cx);
        assert_eq!(selected(&view, cx), BTreeSet::from([3, 7, 1]));
        cx.simulate_mouse_up(row(1), MouseButton::Left, normal);
        draw(cx);

        cx.simulate_mouse_down(row(2), MouseButton::Left, normal);
        draw(cx);
        cx.simulate_mouse_move(row(0), Some(MouseButton::Left), normal);
        draw(cx);
        cx.simulate_mouse_up(row(0), MouseButton::Left, normal);
        draw(cx);
        assert_eq!(selected(&view, cx), BTreeSet::from([9, 3, 7]));
        cx.simulate_click(
            row(3),
            Modifiers {
                shift: true,
                ..normal
            },
        );
        draw(cx);
        assert_eq!(selected(&view, cx), BTreeSet::from([7, 1]));
    }

    #[gpui_kit::test]
    fn drag_selection_tracks_mouse_wheel_scrolling(cx: &mut TestAppContext) {
        let (view, cx) = explorer_fixture(cx, 100);
        let viewport = viewport(&view, cx);
        let normal = Modifiers::default();
        let start = point(viewport.left() + px(100.0), viewport.top() + px(18.0));
        let end = point(
            viewport.left() + px(120.0),
            viewport.top() + FILE_ROW_HEIGHT * 2 + px(18.0),
        );
        cx.simulate_mouse_down(start, MouseButton::Left, normal);
        draw(cx);
        cx.simulate_mouse_move(end, Some(MouseButton::Left), normal);
        draw(cx);
        assert_eq!(selected(&view, cx), BTreeSet::from([0, 1, 2]));
        cx.simulate_event(ScrollEvent {
            position: end,
            delta: ScrollDelta::Pixels(point(px(0.0), -FILE_ROW_HEIGHT * 2)),
            ..Default::default()
        });
        draw(cx);
        assert_eq!(scroll_y(&view, cx), -FILE_ROW_HEIGHT * 2);
        cx.executor().advance_clock(Duration::from_millis(32));
        cx.run_until_parked();
        draw(cx);
        assert_eq!(selected(&view, cx), BTreeSet::from([0, 1, 2, 3, 4]));
        cx.simulate_mouse_up(end, MouseButton::Left, normal);
        draw(cx);
    }

    #[gpui_kit::test]
    fn drag_draws_a_clipped_selection_rectangle_only_while_active(cx: &mut TestAppContext) {
        let (view, cx) = explorer_fixture(cx, 12);
        let viewport = viewport(&view, cx);
        let normal = Modifiers::default();
        let start = point(viewport.left() + px(100.0), viewport.top() + px(18.0));
        let end = point(
            viewport.left() + px(180.0),
            viewport.top() + FILE_ROW_HEIGHT * 3 + px(18.0),
        );
        cx.simulate_mouse_down(start, MouseButton::Left, normal);
        draw(cx);
        cx.simulate_mouse_move(end, Some(MouseButton::Left), normal);
        draw(cx);
        let rectangle_is_painted = |cx: &mut VisualTestContext| {
            cx.update(|window, _| {
                let expected = Bounds::from_corners(start, end).scale(window.scale_factor());
                let mask = viewport.scale(window.scale_factor());
                window.painted_quads().iter().any(|quad| {
                    quad.bounds == expected
                        && quad.content_mask.bounds == mask
                        && quad.border_widths.top.0 > 0.0
                })
            })
        };
        assert!(rectangle_is_painted(cx));
        cx.simulate_mouse_up(end, MouseButton::Left, normal);
        draw(cx);
        assert!(!rectangle_is_painted(cx));
    }

    #[gpui_kit::test]
    fn right_click_preserves_a_dragged_multi_selection(cx: &mut TestAppContext) {
        let (view, cx) = explorer_fixture(cx, 12);
        let viewport = viewport(&view, cx);
        let normal = Modifiers::default();
        let row = |index: usize| {
            point(
                viewport.left() + px(100.0),
                viewport.top() + FILE_ROW_HEIGHT * index + px(18.0),
            )
        };
        cx.simulate_mouse_down(row(0), MouseButton::Left, normal);
        draw(cx);
        cx.simulate_mouse_move(row(2), Some(MouseButton::Left), normal);
        draw(cx);
        cx.simulate_mouse_up(row(2), MouseButton::Left, normal);
        draw(cx);
        cx.simulate_mouse_down(row(1), MouseButton::Right, normal);
        draw(cx);
        cx.simulate_mouse_up(row(1), MouseButton::Right, normal);
        draw(cx);
        assert_eq!(selected(&view, cx), BTreeSet::from([0, 1, 2]));
    }

    #[gpui_kit::test]
    fn filtering_cancels_the_drag_before_updating_the_visible_rows(cx: &mut TestAppContext) {
        let (view, cx) = explorer_fixture(cx, 12);
        let viewport = viewport(&view, cx);
        let normal = Modifiers::default();
        let row = |index: usize| {
            point(
                viewport.left() + px(100.0),
                viewport.top() + FILE_ROW_HEIGHT * index + px(18.0),
            )
        };
        cx.simulate_click(row(1), normal);
        draw(cx);
        cx.simulate_mouse_down(row(2), MouseButton::Left, normal);
        draw(cx);
        cx.simulate_mouse_move(row(4), Some(MouseButton::Left), normal);
        draw(cx);
        assert_eq!(selected(&view, cx), BTreeSet::from([2, 3, 4]));
        cx.update(|window, cx| {
            view.update(cx, |app, cx| {
                app.explorer.search.update(cx, |search, cx| {
                    search.set_value("file-1".to_owned(), window, cx);
                });
            });
        });
        cx.run_until_parked();
        draw(cx);
        assert_eq!(selected(&view, cx), BTreeSet::from([1]));
        assert!(
            view.read_with(cx, |app, _| app.explorer.marquee.is_none()),
            "filter changes must cancel the in-progress drag"
        );
    }

    #[gpui_kit::test]
    fn losing_window_focus_cancels_the_drag_and_stops_scrolling(cx: &mut TestAppContext) {
        let (view, cx) = explorer_fixture(cx, 100);
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        draw(cx);
        let viewport = viewport(&view, cx);
        let normal = Modifiers::default();
        let row = point(viewport.left() + px(100.0), viewport.top() + px(18.0));
        let edge = point(viewport.left() + px(120.0), viewport.bottom() - px(1.0));
        cx.simulate_click(row, normal);
        draw(cx);
        cx.simulate_mouse_down(row, MouseButton::Left, normal);
        draw(cx);
        cx.simulate_mouse_move(edge, Some(MouseButton::Left), normal);
        draw(cx);
        assert!(selected(&view, cx).len() > 1);
        cx.deactivate_window();
        draw(cx);
        assert_eq!(selected(&view, cx), BTreeSet::from([0]));
        let stopped_at = scroll_y(&view, cx);
        cx.executor().advance_clock(Duration::from_millis(96));
        cx.run_until_parked();
        draw(cx);
        assert_eq!(scroll_y(&view, cx), stopped_at);
    }

    #[gpui_kit::test]
    fn escape_cancels_drag_without_turning_release_into_a_click(cx: &mut TestAppContext) {
        let (view, cx) = explorer_fixture(cx, 10);
        let viewport = viewport(&view, cx);
        let normal = Modifiers::default();
        let row = |index: usize| {
            point(
                viewport.left() + px(100.0),
                viewport.top() + FILE_ROW_HEIGHT * index + px(18.0),
            )
        };
        cx.simulate_click(row(0), normal);
        draw(cx);
        cx.simulate_mouse_down(row(2), MouseButton::Left, normal);
        draw(cx);
        cx.simulate_mouse_move(row(4), Some(MouseButton::Left), normal);
        draw(cx);
        assert_eq!(selected(&view, cx), BTreeSet::from([2, 3, 4]));
        cx.simulate_keystrokes("escape");
        draw(cx);
        assert_eq!(selected(&view, cx), BTreeSet::from([0]));
        cx.simulate_mouse_up(row(2), MouseButton::Left, normal);
        draw(cx);
        assert_eq!(selected(&view, cx), BTreeSet::from([0]));

        cx.simulate_mouse_down(row(2), MouseButton::Left, normal);
        draw(cx);
        cx.simulate_mouse_move(row(4), Some(MouseButton::Left), normal);
        draw(cx);
        cx.simulate_keystrokes("escape");
        draw(cx);
        let checkbox = point(viewport.left() + px(20.0), row(0).y);
        cx.simulate_click(checkbox, normal);
        draw(cx);
        assert!(selected(&view, cx).is_empty());
    }

    #[gpui_kit::test]
    fn double_click_still_opens_a_folder(cx: &mut TestAppContext) {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().to_path_buf();
        let (view, cx) = explorer_fixture(cx, 2);
        view.update(cx, |app, cx| {
            app.explorer.entries[0].directory = true;
            app.explorer.entries[0].path = path.clone();
            cx.notify();
        });
        draw(cx);
        let viewport = viewport(&view, cx);
        let position = point(viewport.left() + px(100.0), viewport.top() + px(18.0));
        let normal = Modifiers::default();
        cx.simulate_click(position, normal);
        draw(cx);
        click_with_count(cx, position, 2);
        cx.run_until_parked();
        draw(cx);
        assert_eq!(
            view.read_with(cx, |app, _| app.explorer.location.clone()),
            Location::Directory(path)
        );
    }

    #[gpui_kit::test]
    fn a_drag_does_not_count_as_the_first_click_of_a_double_click(cx: &mut TestAppContext) {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().to_path_buf();
        let (view, cx) = explorer_fixture(cx, 4);
        view.update(cx, |app, cx| {
            app.explorer.entries[0].directory = true;
            app.explorer.entries[0].path = path.clone();
            cx.notify();
        });
        draw(cx);
        let viewport = viewport(&view, cx);
        let start = point(viewport.left() + px(100.0), viewport.top() + px(18.0));
        let end = point(
            viewport.left() + px(120.0),
            viewport.top() + FILE_ROW_HEIGHT * 2 + px(18.0),
        );
        let normal = Modifiers::default();
        cx.simulate_mouse_down(start, MouseButton::Left, normal);
        draw(cx);
        cx.simulate_mouse_move(end, Some(MouseButton::Left), normal);
        draw(cx);
        cx.simulate_mouse_up(end, MouseButton::Left, normal);
        draw(cx);

        click_with_count(cx, start, 2);
        assert_eq!(
            view.read_with(cx, |app, _| app.explorer.location.clone()),
            Location::Computer
        );
        assert_eq!(selected(&view, cx), BTreeSet::from([0]));
        click_with_count(cx, start, 3);
        cx.run_until_parked();
        draw(cx);
        assert_eq!(
            view.read_with(cx, |app, _| app.explorer.location.clone()),
            Location::Directory(path)
        );
    }

    #[test]
    fn unicode_names_and_empty_directories_are_supported() {
        let temp = tempfile::tempdir().unwrap();
        let location = Location::Directory(temp.path().to_path_buf());
        assert!(read_location(&location).unwrap().is_empty());
        fs::write(temp.path().join("中文 文件.txt"), b"content").unwrap();
        let entries = read_location(&location).unwrap();
        assert_eq!(entries[0].name, "中文 文件.txt");
        assert_eq!(entries[0].path, temp.path().join("中文 文件.txt"));
    }
}
