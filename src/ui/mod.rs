#[cfg(windows)]
mod archive_drag;
mod explorer;
mod overwrite;
mod password;
mod progress;
mod render;
mod settings;

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};
use async_channel::Receiver;
use gpui_kit::{
    component::{
        IndexPath,
        input::InputState,
        searchable_list::SearchableVec,
        select::SelectState,
        theme::{Theme, ThemeMode},
    },
    *,
};
use mzip::archive::{
    ArchiveFormat, ArchiveInfo, ArchiveTree, CancellationToken, Cancelled, CreateOptions,
    ExtractOptions, IntegrityMode, JobControl, Progress, SUPPORTED_EXTENSIONS, create_archive,
    create_archive_auto, create_archives_separately, inspect_archive, smart_destination,
};
use progress::{CompressionTiming, is_verification};

#[derive(Clone, Copy, Eq, PartialEq)]
enum Page {
    Explorer,
    Browse,
    Create,
    Settings,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum JobKind {
    Open,
    Create,
    CreateEach,
    Extract,
    Drag,
    Split,
    Merge,
    Update,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ShellSetup {
    Create,
    Extract,
    Split,
}

enum JobOutput {
    Updated(ArchiveInfo, ArchiveTree),
    Exported {
        entries: Option<mzip::archive::ExportedEntries>,
        open: bool,
    },
    Dragged {
        entries: Option<mzip::archive::ExportedEntries>,
        dropped: bool,
        destination: Option<PathBuf>,
    },
    FileProcessed(PathBuf),
    Opened(ArchiveInfo, ArchiveTree),
    Created(ArchiveInfo),
    AutoCreated(PathBuf),
    SeparatelyCreated(Vec<PathBuf>),
    Extracted {
        destination: PathBuf,
        open_after: bool,
    },
}

enum JobEvent {
    Progress(Progress),
    Finished {
        result: Result<JobOutput>,
        warnings: Vec<String>,
        last_progress: Option<Progress>,
        finished_at: Instant,
    },
}

#[derive(Default)]
struct ProgressDispatch {
    last_sent: Option<Instant>,
    latest: Option<Progress>,
}

impl ProgressDispatch {
    fn report(
        &mut self,
        progress: Progress,
        sender: &async_channel::Sender<JobEvent>,
        now: Instant,
    ) {
        if self
            .last_sent
            .is_none_or(|previous| now.duration_since(previous) >= Duration::from_millis(100))
        {
            // Intermediate updates may be dropped; the result includes the latest snapshot.
            match sender.try_send(JobEvent::Progress(progress.clone())) {
                Ok(()) => self.last_sent = Some(now),
                Err(async_channel::TrySendError::Full(_))
                | Err(async_channel::TrySendError::Closed(_)) => {}
            }
        }
        self.latest = Some(progress);
    }
}

pub struct AppView {
    explorer: explorer::Explorer,
    exported_entries: Vec<mzip::archive::ExportedEntries>,
    api_tool_launcher: fn() -> Result<()>,
    shell_setup: Option<ShellSetup>,
    shell_progress: bool,
    shell_extract: bool,
    shell_custom: bool,
    close_deadline: Option<Instant>,
    close_requested: bool,
    page: Page,
    archive: Option<ArchiveInfo>,
    last_open_path: Option<PathBuf>,
    selected: BTreeSet<usize>,
    search: Entity<InputState>,
    archive_password: Entity<InputState>,
    create_password: Entity<InputState>,
    confirm_password: Entity<InputState>,
    split_destination: Entity<InputState>,
    split_size: Entity<SelectState<SearchableVec<&'static str>>>,
    sources: Vec<PathBuf>,
    destination: Option<PathBuf>,
    format: ArchiveFormat,
    overwrite_create: bool,
    overwrite_dialog: overwrite::OverwriteDialog,
    password_prompt: mzip::archive::PasswordPrompt,
    password_request: Option<password::PasswordRequest>,
    smart_extract: bool,
    open_after_extract: bool,
    dark: bool,
    integrity_mode: IntegrityMode,
    settings_path: Option<PathBuf>,
    status: String,
    status_is_error: bool,
    progress: Option<Progress>,
    verify_progress: Option<Progress>,
    job_kind: JobKind,
    job_integrity: IntegrityMode,
    job_timing: Option<CompressionTiming>,
    running: Option<CancellationToken>,
    visible_entries: Vec<usize>,
    last_filter: String,
    filter_dirty: bool,
    _subscriptions: Vec<Subscription>,
}

impl AppView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("搜索压缩包中的文件"));
        let archive_password = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("压缩包密码（如需要）")
                .masked(true)
        });
        let create_password = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("留空则不加密")
                .masked(true)
        });
        let confirm_password = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("再次输入密码")
                .masked(true)
        });
        let search_subscription = cx.observe(&search, |_, _, cx| cx.notify());
        let split_destination = cx.new(|cx| InputState::new(window, cx));
        let split_size = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(
                    mzip_shell_protocol::SPLIT_SIZES_MB
                        .iter()
                        .map(|(label, _)| *label)
                        .collect::<Vec<_>>(),
                ),
                Some(IndexPath::new(0)),
                window,
                cx,
            )
        });
        let explorer = explorer::Explorer::new(window, cx);
        // Tests must not read or write the user's real settings.
        let settings_path = if cfg!(test) {
            None
        } else {
            settings::settings_path()
        };
        let password_prompt = Self::password_prompt(cx);
        Self {
            explorer,
            exported_entries: Vec::new(),
            api_tool_launcher: explorer::launch_api_tester,
            shell_setup: None,
            shell_progress: false,
            shell_extract: false,
            shell_custom: false,
            close_deadline: None,
            close_requested: false,
            page: Page::Explorer,
            archive: None,
            last_open_path: None,
            selected: BTreeSet::new(),
            search,
            archive_password,
            create_password,
            confirm_password,
            split_destination,
            split_size,
            sources: Vec::new(),
            destination: None,
            format: ArchiveFormat::Zip,
            overwrite_create: false,
            overwrite_dialog: overwrite::OverwriteDialog::new(window),
            password_prompt,
            password_request: None,
            smart_extract: true,
            open_after_extract: false,
            dark: false,
            integrity_mode: settings::load_integrity_mode(settings_path.as_deref()),
            settings_path,
            status: "双击浏览文件，右键选择压缩或解压操作。".to_owned(),
            status_is_error: false,
            progress: None,
            verify_progress: None,
            job_kind: JobKind::Open,
            job_integrity: IntegrityMode::Normal,
            job_timing: None,
            running: None,
            visible_entries: Vec::new(),
            last_filter: String::new(),
            filter_dirty: true,
            _subscriptions: vec![search_subscription],
        }
    }

    fn set_status(&mut self, message: impl Into<String>, is_error: bool, cx: &mut Context<Self>) {
        self.status = message.into();
        self.status_is_error = is_error;
        cx.notify();
    }

    fn password(input: &Entity<InputState>, cx: &App) -> Option<String> {
        let value = input.read(cx).value().to_string();
        (!value.is_empty()).then_some(value)
    }

    pub fn configure_split(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.shell_setup = Some(ShellSetup::Split);
        self.shell_custom = true;
        let path = if path.is_absolute() {
            path
        } else {
            match std::env::current_dir() {
                Ok(directory) => directory.join(path),
                Err(error) => {
                    self.set_status(format!("无法确定当前文件夹：{error}"), true, cx);
                    return;
                }
            }
        };
        if !path.is_file() {
            self.set_status("请选择一个普通文件进行分割。", true, cx);
            return;
        }
        let Some(parent) = path.parent() else {
            self.set_status("无法确定源文件所在文件夹。", true, cx);
            return;
        };
        let directory = parent.to_string_lossy().into_owned();
        self.split_destination
            .update(cx, |input, cx| input.set_value(directory, window, cx));
        self.sources = vec![path];
        self.set_status("请选择分割文件夹和分卷大小。", false, cx);
    }

    fn choose_split_destination(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.split_destination.read(cx).value().to_string();
        if let Some(path) = rfd::FileDialog::new()
            .set_title("分割文件到")
            .set_directory(current)
            .pick_folder()
        {
            self.split_destination.update(cx, |input, cx| {
                input.set_value(path.to_string_lossy().into_owned(), window, cx);
            });
        }
    }

    fn start_split(&mut self, cx: &mut Context<Self>) {
        let Some(source) = self.sources.first().cloned() else {
            self.set_status("请先选择要分割的文件。", true, cx);
            return;
        };
        let directory = self.split_destination.read(cx).value().to_string();
        if directory.trim().is_empty() {
            self.set_status("请输入目标文件夹。", true, cx);
            return;
        }
        let Some(label) = self.split_size.read(cx).selected_value() else {
            self.set_status("请选择分卷大小。", true, cx);
            return;
        };
        let Some((_, mb)) = mzip_shell_protocol::SPLIT_SIZES_MB
            .iter()
            .find(|(value, _)| value == label)
        else {
            self.set_status("分卷大小无效。", true, cx);
            return;
        };
        let bytes = mb * 1024 * 1024;
        let destination = PathBuf::from(directory);
        self.destination = Some(destination.clone());
        self.shell_setup = None;
        self.shell_progress = true;
        self.start_job("正在准备分割…", JobKind::Split, cx, move |control| {
            mzip::archive::split_file(&source, &destination, bytes, &control)?;
            Ok(JobOutput::FileProcessed(destination))
        });
    }

    pub fn start_merge(&mut self, sources: Vec<PathBuf>, cx: &mut Context<Self>) {
        self.shell_progress = true;
        self.shell_custom = true;
        self.sources = sources.clone();
        self.destination = mzip_shell_protocol::merge_parts(&sources).map(|(path, _)| path);
        self.start_job("正在准备合并…", JobKind::Merge, cx, move |control| {
            mzip::archive::merge_files(&sources, &control).map(JobOutput::FileProcessed)
        });
    }

    pub fn open_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let path = match std::path::absolute(path) {
            Ok(path) => path,
            Err(error) => {
                self.set_status(format!("无法确定压缩包路径：{error}"), true, cx);
                return;
            }
        };
        if path.is_dir() {
            self.open_directory(path, cx);
            return;
        }
        if self.running.is_some() {
            self.set_status("请等待当前任务完成，或先取消任务。", true, cx);
            return;
        }
        self.page = Page::Explorer;
        self.explorer.prepare_archive_open();
        self.last_open_path = Some(path.clone());
        let password = Self::password(&self.archive_password, cx);
        self.start_job(
            "正在打开压缩包…",
            JobKind::Open,
            cx,
            move |control| {
                let info = inspect_archive(&path, password.as_deref(), &control)?;
                let tree = ArchiveTree::new(&info, &control)?;
                Ok(JobOutput::Opened(info, tree))
            },
        );
    }

    pub fn start_create_from_sources(
        &mut self,
        sources: Vec<PathBuf>,
        format: ArchiveFormat,
        cx: &mut Context<Self>,
    ) {
        self.shell_progress = true;
        self.page = Page::Create;
        self.format = format;
        self.destination = match mzip::cli::automatic_archive_path(&sources, format) {
            Ok(path) => Some(path),
            Err(error) => {
                self.set_status(format!("{error:#}"), true, cx);
                return;
            }
        };
        self.add_sources(sources, cx);
        let sources = self.sources.clone();
        self.start_job(
            "正在准备压缩…",
            JobKind::Create,
            cx,
            move |control| {
                create_archive_auto(sources, format, &control).map(JobOutput::AutoCreated)
            },
        );
    }

    pub fn start_create_each(&mut self, sources: Vec<PathBuf>, cx: &mut Context<Self>) {
        self.shell_progress = true;
        self.page = Page::Create;
        self.format = ArchiveFormat::Zip;
        self.destination = None;
        self.sources = sources.clone();
        self.start_job(
            "正在准备逐个压缩…",
            JobKind::CreateEach,
            cx,
            move |control| {
                create_archives_separately(sources, &control).map(JobOutput::SeparatelyCreated)
            },
        );
    }

    pub fn configure_create_from_sources(&mut self, sources: Vec<PathBuf>, cx: &mut Context<Self>) {
        self.shell_setup = Some(ShellSetup::Create);
        self.shell_custom = true;
        self.page = Page::Create;
        self.format = ArchiveFormat::Zip;
        self.destination = match mzip::cli::automatic_archive_path(&sources, self.format) {
            Ok(path) => Some(path),
            Err(error) => {
                self.set_status(format!("{error:#}"), true, cx);
                return;
            }
        };
        self.add_sources(sources, cx);
        self.set_status("请选择压缩格式和保存位置，密码可留空。", false, cx);
    }

    pub fn configure_extract_from_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.shell_setup = Some(ShellSetup::Extract);
        self.shell_custom = true;
        self.shell_extract = true;
        self.last_open_path = Some(path.clone());
        match path.parent() {
            Some(parent) => {
                self.destination = Some(parent.to_path_buf());
                self.set_status("请选择解压目标文件夹；加密压缩包请先输入密码。", false, cx);
            }
            None => self.set_status("无法确定压缩包所在文件夹。", true, cx),
        }
    }

    pub fn request_close(&mut self, cx: &mut Context<Self>) -> bool {
        if self.running.is_none() {
            return true;
        }
        self.close_requested = true;
        self.cancel(cx);
        false
    }

    pub fn start_extract_from_path(
        &mut self,
        path: PathBuf,
        named_folder: bool,
        cx: &mut Context<Self>,
    ) {
        self.shell_progress = true;
        self.shell_extract = true;
        self.last_open_path = Some(path.clone());
        let destination = (|| {
            let base = path.parent().context("无法确定压缩包所在文件夹")?;
            if named_folder {
                mzip::archive::named_destination(&path, base)
            } else {
                Ok(base.to_path_buf())
            }
        })();
        let destination = match destination {
            Ok(destination) => destination,
            Err(error) => {
                self.set_status(format!("{error:#}"), true, cx);
                return;
            }
        };
        self.destination = Some(destination.clone());
        let dialog = self.overwrite_dialog.clone();
        self.start_job(
            "正在准备解压…",
            JobKind::Extract,
            cx,
            move |control| {
                let info = inspect_archive(&path, None, &control)?;
                overwrite::extract_with_confirmation(
                    &info,
                    ExtractOptions {
                        destination: destination.clone(),
                        password: None,
                        selected: None,
                        overwrite: false,
                    },
                    &control,
                    &dialog,
                )?;
                Ok(JobOutput::Extracted {
                    destination,
                    open_after: false,
                })
            },
        );
    }

    fn choose_archive(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = rfd::FileDialog::new()
            .set_title("打开压缩包")
            .add_filter("压缩文件", SUPPORTED_EXTENSIONS)
            .add_filter("所有文件（尝试作为压缩包打开）", &["*"])
            .pick_file()
        {
            self.open_path(path, cx);
        }
    }

    #[cfg(windows)]
    fn open_default_apps(&mut self, cx: &mut Context<Self>) {
        use windows::{
            Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL},
            core::w,
        };

        let result = unsafe {
            ShellExecuteW(
                None,
                w!("open"),
                w!("ms-settings:defaultapps"),
                None,
                None,
                SW_SHOWNORMAL,
            )
        };
        if result.0 as isize <= 32 {
            self.set_status("无法打开 Windows 默认应用设置。", true, cx);
        } else {
            self.set_status(
                "请在 Windows 设置中选择 MZip 作为所需压缩格式的默认应用。",
                false,
                cx,
            );
        }
    }

    fn handle_drop(&mut self, paths: &[PathBuf], cx: &mut Context<Self>) {
        if self.running.is_some() {
            self.set_status("请等待当前任务完成，或先取消任务。", true, cx);
        } else if self.in_archive() {
            self.add_to_open_archive(paths.to_vec(), cx);
        } else if paths.len() == 1
            && paths[0].is_file()
            && mzip_shell_protocol::can_attempt_extraction(&paths[0])
        {
            self.open_path(paths[0].clone(), cx);
        } else if !paths.is_empty() {
            self.page = Page::Create;
            self.add_sources(paths.to_vec(), cx);
        }
    }

    fn add_sources(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let mut added = 0usize;
        for path in paths {
            if !self.sources.contains(&path) {
                self.sources.push(path);
                added += 1;
            }
        }
        self.set_status(format!("已添加 {added} 个源文件或文件夹。"), false, cx);
    }

    fn choose_files(&mut self, cx: &mut Context<Self>) {
        if let Some(paths) = rfd::FileDialog::new().set_title("添加文件").pick_files() {
            self.add_sources(paths, cx);
        }
    }

    fn choose_directory(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = rfd::FileDialog::new().set_title("添加文件夹").pick_folder() {
            self.add_sources(vec![path], cx);
        }
    }

    fn choose_destination(&mut self, cx: &mut Context<Self>) {
        let default_name = format!("archive.{}", self.format.extension());
        let mut dialog = rfd::FileDialog::new()
            .set_title("保存压缩包")
            .add_filter("压缩包", &[self.format.extension()]);
        let current_name = self
            .destination
            .as_ref()
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or(default_name);
        dialog = dialog.set_file_name(&current_name);
        if let Some(parent) = self
            .destination
            .as_ref()
            .and_then(|path| path.parent())
            .or_else(|| self.sources.first().and_then(|path| path.parent()))
        {
            dialog = dialog.set_directory(parent);
        }
        if let Some(mut path) = dialog.save_file() {
            if path.extension().is_none() {
                path.set_extension(self.format.extension());
            }
            match ArchiveFormat::from_path(&path) {
                Ok(format) if format == self.format => {
                    self.destination = Some(path);
                    cx.notify();
                }
                _ => self.set_status("保存文件的扩展名必须与选中的格式一致。", true, cx),
            }
        }
    }

    fn choose_extract_destination(&mut self, cx: &mut Context<Self>) {
        let mut dialog = rfd::FileDialog::new().set_title("选择解压目标文件夹");
        if let Some(destination) = &self.destination {
            dialog = dialog.set_directory(destination);
        }
        if let Some(destination) = dialog.pick_folder() {
            self.destination = Some(destination);
            cx.notify();
        }
    }

    fn set_format(&mut self, format: ArchiveFormat, cx: &mut Context<Self>) {
        self.format = format;
        if let Some(path) = &mut self.destination {
            path.set_extension(format.extension());
        }
        cx.notify();
    }

    fn create(&mut self, cx: &mut Context<Self>) {
        let password = Self::password(&self.create_password, cx);
        let confirmation = Self::password(&self.confirm_password, cx);
        if password != confirmation {
            self.set_status("两次输入的密码不一致。", true, cx);
            return;
        }
        let Some(destination) = self.destination.clone() else {
            self.set_status("请先选择压缩包的保存位置。", true, cx);
            return;
        };
        if self.sources.is_empty() {
            self.set_status("请先添加需要压缩的文件或文件夹。", true, cx);
            return;
        }
        let options = CreateOptions {
            sources: self.sources.clone(),
            destination,
            format: self.format,
            password,
            overwrite: self.overwrite_create,
        };
        let shell_custom = self.shell_setup == Some(ShellSetup::Create);
        if shell_custom {
            self.shell_setup = None;
            self.shell_progress = true;
            self.shell_extract = false;
        }
        self.start_job(
            "正在准备压缩…",
            JobKind::Create,
            cx,
            move |control| {
                create_archive(&options, &control)?;
                if shell_custom {
                    return Ok(JobOutput::AutoCreated(options.destination));
                }
                let info = inspect_archive(
                    &options.destination,
                    options.password.as_deref(),
                    &JobControl::default(),
                )
                .context("压缩包已创建，但无法重新打开")?;
                Ok(JobOutput::Created(info))
            },
        );
    }

    fn extract_to_destination(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.last_open_path.clone() else {
            self.set_status("请先选择压缩包。", true, cx);
            return;
        };
        let Some(destination) = self.destination.clone() else {
            self.set_status("请选择解压目标文件夹。", true, cx);
            return;
        };
        let password = Self::password(&self.archive_password, cx);
        self.shell_setup = None;
        self.shell_progress = true;
        self.shell_extract = true;
        let dialog = self.overwrite_dialog.clone();
        self.start_job(
            "正在准备解压…",
            JobKind::Extract,
            cx,
            move |control| {
                let info = inspect_archive(&path, password.as_deref(), &control)?;
                overwrite::extract_with_confirmation(
                    &info,
                    ExtractOptions {
                        destination: destination.clone(),
                        password,
                        selected: None,
                        overwrite: false,
                    },
                    &control,
                    &dialog,
                )?;
                Ok(JobOutput::Extracted {
                    destination,
                    open_after: false,
                })
            },
        );
    }

    fn extract(&mut self, selected_only: bool, cx: &mut Context<Self>) {
        let Some(info) = self.archive.clone() else {
            self.set_status("请先打开一个压缩包。", true, cx);
            return;
        };
        if selected_only && self.selected.is_empty() {
            self.set_status("请先勾选要解压的文件或文件夹。", true, cx);
            return;
        }
        let mut dialog = rfd::FileDialog::new().set_title("选择解压目标文件夹");
        if let Some(parent) = info.path.parent() {
            dialog = dialog.set_directory(parent);
        }
        let Some(base) = dialog.pick_folder() else {
            return;
        };
        let destination = if self.smart_extract {
            match smart_destination(&info, &base) {
                Ok(path) => path,
                Err(error) => {
                    self.set_status(format!("{error:#}"), true, cx);
                    return;
                }
            }
        } else {
            base
        };
        let options = ExtractOptions {
            destination: destination.clone(),
            password: Self::password(&self.archive_password, cx),
            selected: selected_only.then(|| self.selected.clone()),
            overwrite: false,
        };
        let open_after = self.open_after_extract;
        let dialog = self.overwrite_dialog.clone();
        self.start_job(
            "正在准备解压…",
            JobKind::Extract,
            cx,
            move |control| {
                overwrite::extract_with_confirmation(&info, options, &control, &dialog)?;
                Ok(JobOutput::Extracted {
                    destination,
                    open_after,
                })
            },
        );
    }

    fn start_job(
        &mut self,
        message: &str,
        kind: JobKind,
        cx: &mut Context<Self>,
        operation: impl FnOnce(JobControl) -> Result<JobOutput> + Send + 'static,
    ) {
        if self.running.is_some() {
            self.set_status("请等待当前任务完成，或先取消任务。", true, cx);
            return;
        }
        let cancellation = CancellationToken::default();
        let worker_cancellation = cancellation.clone();
        let integrity = self.integrity_mode;
        let password_prompt = self.password_prompt.clone();
        let (sender, receiver) = async_channel::bounded(1);
        let started_at = Instant::now();
        let spawn = thread::Builder::new()
            .name("mzip-archive-worker".to_owned())
            .spawn(move || {
                let progress_sender = sender.clone();
                let progress_state = Arc::new(Mutex::new(ProgressDispatch::default()));
                let reported_state = progress_state.clone();
                let control = JobControl::new(worker_cancellation, move |progress| {
                    reported_state
                        .lock()
                        .expect("progress state poisoned")
                        .report(progress, &progress_sender, Instant::now());
                })
                .with_integrity(integrity)
                .with_password_prompt(password_prompt);
                let result = operation(control.clone());
                let finished_at = Instant::now();
                let last_progress = progress_state
                    .lock()
                    .expect("progress state poisoned")
                    .latest
                    .take();
                let _ = sender.send_blocking(JobEvent::Finished {
                    result,
                    warnings: control.warnings(),
                    last_progress,
                    finished_at,
                });
            });
        if let Err(error) = spawn {
            self.set_status(format!("无法启动后台任务：{error}"), true, cx);
            return;
        }
        self.running = Some(cancellation);
        self.close_deadline = None;
        self.progress = None;
        self.verify_progress = None;
        self.job_kind = kind;
        self.job_integrity = integrity;
        self.job_timing = (!matches!(kind, JobKind::Open | JobKind::CreateEach | JobKind::Drag))
            .then(|| CompressionTiming::new(started_at));
        self.set_status(message, false, cx);
        Self::receive_job_events(receiver, cx);
        if self.job_timing.is_some() || kind == JobKind::Drag {
            Self::refresh_job_progress(cx);
        }
    }

    fn receive_job_events(receiver: Receiver<JobEvent>, cx: &mut Context<Self>) {
        cx.spawn(async move |view, cx| {
            while let Ok(event) = receiver.recv().await {
                let finished = matches!(&event, JobEvent::Finished { .. });
                if view
                    .update(cx, |this, cx| this.handle_job_event(event, cx))
                    .is_err()
                {
                    return;
                }
                if finished {
                    return;
                }
            }
            let _ = view.update(cx, |this, cx| {
                this.running = None;
                this.progress = None;
                this.verify_progress = None;
                if let Some(timing) = &mut this.job_timing {
                    timing.finish(Instant::now(), false);
                }
                this.set_status("后台任务异常退出，请重试。", true, cx);
            });
        })
        .detach();
    }

    fn refresh_job_progress(cx: &mut Context<Self>) {
        cx.spawn(async move |view, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                match view.update(cx, |this, cx| {
                    if this.running.is_some() {
                        cx.notify();
                        true
                    } else {
                        false
                    }
                }) {
                    Ok(true) => {}
                    Ok(false) | Err(_) => return,
                }
            }
        })
        .detach();
    }

    fn schedule_auto_close(&mut self, cx: &mut Context<Self>) {
        let deadline = Instant::now() + Duration::from_secs(5);
        self.close_deadline = Some(deadline);
        cx.notify();
        cx.spawn(async move |view, cx| {
            loop {
                let delay = deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_secs(1));
                cx.background_executor().timer(delay).await;
                match view.update(cx, |this, cx| {
                    if this.close_deadline != Some(deadline) {
                        return false;
                    }
                    if Instant::now() >= deadline {
                        cx.quit();
                        return false;
                    }
                    cx.notify();
                    true
                }) {
                    Ok(true) => {}
                    Ok(false) | Err(_) => return,
                }
            }
        })
        .detach();
    }

    fn handle_job_event(&mut self, event: JobEvent, cx: &mut Context<Self>) {
        match event {
            JobEvent::Progress(progress) => {
                self.apply_progress(progress);
                cx.notify();
            }
            JobEvent::Finished {
                result,
                warnings,
                last_progress,
                finished_at,
            } => {
                let completed = matches!(
                    &result,
                    Ok(JobOutput::Created(_)
                        | JobOutput::Updated(..)
                        | JobOutput::Exported { .. }
                        | JobOutput::Dragged { dropped: true, .. }
                        | JobOutput::FileProcessed(_)
                        | JobOutput::AutoCreated(_)
                        | JobOutput::SeparatelyCreated(_)
                        | JobOutput::Extracted { .. })
                );
                let cancelled = result.as_ref().err().is_some_and(|error| {
                    error.downcast_ref::<Cancelled>().is_some()
                        || error
                            .downcast_ref::<overwrite::ConfirmationCancelled>()
                            .is_some()
                });
                self.running = None;
                if (self.job_timing.is_some() || self.job_kind == JobKind::Drag) && completed {
                    if let Some(progress) = last_progress {
                        self.apply_progress(progress);
                    }
                    if let Some(verify) = &mut self.verify_progress {
                        verify.completed_bytes = verify.total_bytes;
                    }
                } else {
                    self.progress = None;
                    self.verify_progress = None;
                }
                if let Some(timing) = &mut self.job_timing {
                    timing.finish(finished_at, completed);
                }
                match result {
                    Ok(JobOutput::Updated(info, tree)) => self.refresh_open_archive(info, tree, cx),
                    Ok(JobOutput::Dragged {
                        entries,
                        dropped,
                        destination,
                    }) => {
                        if let Some(entries) = entries {
                            self.exported_entries.push(entries);
                        }
                        if let Some(destination) = destination {
                            self.set_status(
                                format!(
                                    "已直接解压到：{}；未交给 Windows 再次复制，压缩包未改变。",
                                    destination.display()
                                ),
                                false,
                                cx,
                            );
                        } else {
                            self.set_status(
                                if dropped {
                                    "拖放已结束；文件由 Windows 复制到目标文件夹，压缩包未改变。"
                                } else {
                                    "拖放已取消，压缩包未改变。"
                                },
                                false,
                                cx,
                            );
                        }
                    }
                    Ok(JobOutput::Exported { entries, open }) => {
                        if let Some(entries) = entries {
                            let path = entries.paths.first().cloned();
                            self.exported_entries.push(entries);
                            if open && let Some(path) = path {
                                if mzip_shell_protocol::can_attempt_extraction(&path) {
                                    self.open_path(path, cx);
                                } else if let Err(error) = explorer::open_default(&path) {
                                    self.set_status(
                                        format!("无法打开解压后的文件：{error:#}"),
                                        true,
                                        cx,
                                    );
                                } else {
                                    self.set_status(
                                        "已打开临时副本；外部编辑不会自动写回压缩包。",
                                        false,
                                        cx,
                                    );
                                }
                            } else {
                                self.set_status(
                                    "拖放已结束；文件由 Windows 复制到目标文件夹，压缩包未改变。",
                                    false,
                                    cx,
                                );
                            }
                        } else {
                            self.set_status("拖放已取消，压缩包未改变。", false, cx);
                        }
                    }
                    Ok(JobOutput::FileProcessed(path)) => {
                        self.destination = Some(path.clone());
                        self.set_status(
                            format!(
                                "{}完成：{}",
                                if self.job_kind == JobKind::Split {
                                    "分割"
                                } else {
                                    "合并"
                                },
                                path.display()
                            ),
                            false,
                            cx,
                        );
                    }
                    Ok(JobOutput::Opened(info, tree)) => {
                        self.show_archive_with_tree(info, tree, false, cx)
                    }
                    Ok(JobOutput::Created(info)) => self.show_archive(info, true, cx),
                    Ok(JobOutput::AutoCreated(path)) => {
                        self.destination = Some(path.clone());
                        self.set_status(format!("压缩完成：{}", path.display()), false, cx);
                    }
                    Ok(JobOutput::SeparatelyCreated(paths)) => {
                        self.set_status(
                            format!(
                                "压缩完成：已生成 {} 个 ZIP，分别保存在源文件所在目录。",
                                paths.len()
                            ),
                            false,
                            cx,
                        );
                    }
                    Ok(JobOutput::Extracted {
                        destination,
                        open_after,
                    }) => {
                        self.destination = Some(destination.clone());
                        if open_after && let Err(error) = open_in_file_manager(&destination) {
                            self.set_status(
                                format!("解压完成，但无法打开目标文件夹：{error:#}"),
                                true,
                                cx,
                            );
                        } else {
                            self.set_status(
                                format!("解压完成：{}", destination.display()),
                                false,
                                cx,
                            );
                        }
                    }
                    Err(error)
                        if error
                            .downcast_ref::<overwrite::ConfirmationCancelled>()
                            .is_some() =>
                    {
                        self.set_status("已取消解压；未写入目标文件。", false, cx);
                    }
                    Err(error) if error.downcast_ref::<Cancelled>().is_some() => {
                        self.set_status(
                            if self.job_kind == JobKind::Update {
                                "修改已取消；原压缩包保持不变。"
                            } else if self.job_kind == JobKind::Create {
                                "压缩已取消；未生成压缩包，临时文件已清理。"
                            } else if self.job_kind == JobKind::Drag {
                                "拖放已取消，压缩包未改变；目标位置可能保留部分文件。"
                            } else if self.job_kind == JobKind::Extract {
                                if self.job_integrity == IntegrityMode::Safe {
                                    "解压已取消；当前压缩包的输出已回滚，原有文件已保留。"
                                } else {
                                    "解压已取消（高速模式）；已解压的文件会保留，结果可能不完整。"
                                }
                            } else {
                                "任务已取消；已完成的文件会保留。"
                            },
                            false,
                            cx,
                        );
                    }
                    Err(error) if self.job_kind == JobKind::Drag => self.set_status(
                        format!("拖放失败：{error:#}；目标位置可能保留部分文件。"),
                        true,
                        cx,
                    ),
                    Err(error) => self.set_status(format!("{error:#}"), true, cx),
                }
                if !warnings.is_empty() {
                    self.set_status(
                        format!("{}；{}", self.status, warnings.join("；")),
                        self.status_is_error,
                        cx,
                    );
                }
                if self.close_requested && !self.status_is_error && warnings.is_empty() {
                    cx.quit();
                } else if self.shell_progress
                    && !self.status_is_error
                    && warnings.is_empty()
                    && (completed || cancelled)
                {
                    self.schedule_auto_close(cx);
                }
                self.close_requested = false;
            }
        }
    }

    fn apply_progress(&mut self, progress: Progress) {
        if self.job_kind == JobKind::Drag && self.job_timing.is_none() {
            self.job_timing = Some(CompressionTiming::new(Instant::now()));
            self.status = "正在提取拖放文件…".to_owned();
        }
        // Verification events go to their own progress section; separate compression keeps a
        // single combined line.
        if self.job_kind == JobKind::CreateEach || !is_verification(progress.operation) {
            self.progress = Some(progress);
            return;
        }
        let main = self.progress.get_or_insert_with(|| Progress {
            operation: if matches!(self.job_kind, JobKind::Extract | JobKind::Drag) {
                "正在解压"
            } else {
                "正在压缩"
            },
            current_file: String::new(),
            completed_bytes: 0,
            total_bytes: progress.total_bytes,
            archive_size: None,
        });
        main.completed_bytes = main.total_bytes;
        if progress.archive_size.is_some() {
            main.archive_size = progress.archive_size;
        }
        self.verify_progress = Some(progress);
    }

    fn show_archive(&mut self, info: ArchiveInfo, created: bool, cx: &mut Context<Self>) {
        match ArchiveTree::new(&info, &JobControl::default()) {
            Ok(tree) => self.show_archive_with_tree(info, tree, created, cx),
            Err(error) => self.set_status(format!("{error:#}"), true, cx),
        }
    }

    fn show_archive_with_tree(
        &mut self,
        info: ArchiveInfo,
        tree: ArchiveTree,
        created: bool,
        cx: &mut Context<Self>,
    ) {
        let count = info.entries.len();
        if let Err(error) = self.enter_archive_root(info, tree, cx) {
            self.set_status(format!("{error:#}"), true, cx);
            return;
        }
        self.selected.clear();
        self.filter_dirty = true;
        let message = if created {
            format!("压缩包已创建，包含 {count} 个条目。加密文件解压时请在下方输入密码。")
        } else {
            format!("压缩包已打开，包含 {count} 个条目。")
        };
        self.set_status(message, false, cx);
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        if let Some(cancellation) = &self.running {
            cancellation.cancel();
            self.set_status("正在取消，请稍候…", false, cx);
        }
    }

    fn toggle_pause(&mut self, cx: &mut Context<Self>) {
        if let Some(control) = &self.running {
            let paused = !control.is_paused();
            control.set_paused(paused);
            if let Some(timing) = &mut self.job_timing {
                timing.set_paused(paused, Instant::now());
            }
            self.set_status(
                if paused {
                    "已请求暂停；当前数据块处理完后暂停。"
                } else {
                    "正在继续…"
                },
                false,
                cx,
            );
        }
    }

    fn refresh_filter(&mut self, cx: &App) {
        let filter = self.search.read(cx).value().trim().to_lowercase();
        if self.filter_dirty || filter != self.last_filter {
            self.visible_entries = self
                .archive
                .as_ref()
                .map(|info| {
                    info.entries
                        .iter()
                        .filter(|entry| {
                            filter.is_empty() || entry.name.to_lowercase().contains(&filter)
                        })
                        .map(|entry| entry.index)
                        .collect()
                })
                .unwrap_or_default();
            self.last_filter = filter;
            self.filter_dirty = false;
        }
    }

    fn set_integrity_mode(&mut self, mode: IntegrityMode, cx: &mut Context<Self>) {
        if self.running.is_some() {
            self.set_status("请等待当前任务完成后再更改设置。", true, cx);
            return;
        }
        if mode == self.integrity_mode {
            return;
        }
        match settings::save_integrity_mode(self.settings_path.as_deref(), mode) {
            Ok(()) => {
                self.integrity_mode = mode;
                self.set_status(
                    format!("已切换为{}，此后的任务将使用该模式。", mode.label()),
                    false,
                    cx,
                );
            }
            Err(error) => self.set_status(format!("{error:#}"), true, cx),
        }
    }

    fn change_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.dark = !self.dark;
        Theme::change(
            if self.dark {
                ThemeMode::Dark
            } else {
                ThemeMode::Light
            },
            Some(window),
            cx,
        );
        cx.notify();
    }
}

impl Drop for AppView {
    fn drop(&mut self) {
        if let Some(cancellation) = &self.running {
            cancellation.cancel();
        }
    }
}

fn open_in_file_manager(path: &Path) -> Result<()> {
    #[cfg(windows)]
    let mut command = std::process::Command::new("explorer.exe");
    #[cfg(target_os = "macos")]
    let mut command = std::process::Command::new("open");
    #[cfg(all(not(windows), not(target_os = "macos")))]
    let mut command = std::process::Command::new("xdg-open");
    command.arg(path).spawn().context("无法启动文件管理器")?;
    Ok(())
}

const AUTHOR: &str = "大明哥";

const ACKNOWLEDGED_LIBRARIES: &[(&str, &str)] = &[
    ("GPUI / gpui-kit", "界面框架"),
    ("rfd", "系统文件对话框"),
    ("reqwest", "HTTP 请求"),
    ("directories", "用户数据目录"),
    ("serde / serde_json", "请求数据序列化"),
    ("url", "URL 解析"),
    ("windows-rs", "Windows 资源管理器集成"),
    ("zip", "ZIP 读写与 AES 加密"),
    ("sevenz-rust2", "7z 读写"),
    ("UnRAR（unrar-ng-sys）", "RAR 解压，© Alexander Roshal"),
    ("iso9660-forensic-core", "ISO 镜像读取"),
    ("cab", "CAB 读取"),
    ("tar", "TAR 读写"),
    ("flate2 / zlib-rs", "Deflate、GZip"),
    ("bzip2", "BZip2"),
    ("lzma-rust2", "LZMA、XZ"),
    ("zstd", "Zstandard"),
    ("brotli", "Brotli"),
    ("crc32fast", "CRC32 校验"),
    ("chrono", "时间处理"),
    ("walkdir", "目录遍历"),
    ("tempfile", "安全临时文件"),
    ("anyhow", "错误处理"),
    ("async-channel", "后台任务通信"),
];

fn about_text() -> String {
    let mut text = format!(
        "MZip {}\n本地桌面压缩文件管理器与 API 测试工具\n\n作者：{AUTHOR}\n\n感谢以下开源项目：\n",
        env!("CARGO_PKG_VERSION")
    );
    for (name, purpose) in ACKNOWLEDGED_LIBRARIES {
        text.push_str(&format!("• {name} — {purpose}\n"));
    }
    text.push_str(
        "\n以及它们依赖的众多开源库。各项目的许可证见安装目录中的 THIRD_PARTY_NOTICES.txt。",
    );
    text
}

fn show_about() {
    rfd::MessageDialog::new()
        .set_title("关于 MZip")
        .set_description(about_text())
        .set_level(rfd::MessageLevel::Info)
        .show();
}

fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use mzip::archive::Progress;

    use super::{JobEvent, ProgressDispatch};

    #[test]
    fn about_names_author_version_and_direct_dependencies() {
        let text = super::about_text();
        assert!(text.contains("作者：大明哥"));
        assert!(text.contains(env!("CARGO_PKG_VERSION")));
        for dependency in [
            "gpui-kit",
            "rfd",
            "reqwest",
            "directories",
            "serde",
            "serde_json",
            "url",
            "windows",
            "zip",
            "sevenz-rust2",
            "unrar-ng-sys",
            "iso9660-forensic-core",
            "cab",
            "tar",
            "flate2",
            "bzip2",
            "lzma-rust2",
            "zstd",
            "brotli",
            "crc32fast",
            "chrono",
            "walkdir",
            "tempfile",
            "anyhow",
            "async-channel",
        ] {
            assert!(text.contains(dependency), "{dependency} not acknowledged");
        }
    }

    #[gpui_kit::test]
    fn split_setup_defaults_to_source_folder_and_ten_mb(cx: &mut gpui_kit::TestAppContext) {
        use super::{AppView, ShellSetup};
        use gpui_kit::{base::Root, *};

        let temp = tempfile::TempDir::new().unwrap();
        let source = temp.path().join("input.bin");
        std::fs::write(&source, b"source").unwrap();
        cx.update(gpui_kit::init);
        let (root, cx) = cx.add_window_view(move |window, cx| {
            let view = cx.new(|cx| {
                let mut app = AppView::new(window, cx);
                app.configure_split(source, window, cx);
                app
            });
            Root::new(view, window, cx)
        });
        let view = root.read_with(cx, |root, _| {
            root.view().clone().downcast::<AppView>().unwrap()
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        view.read_with(cx, |app, cx| {
            assert!(app.shell_setup == Some(ShellSetup::Split));
            assert_eq!(
                app.split_destination.read(cx).value().as_ref(),
                temp.path().to_str().unwrap()
            );
            assert_eq!(app.split_size.read(cx).selected_value(), Some(&"10M"));
            assert!(!app.status_is_error);
        });
        cx.update(|window, cx| {
            view.update(cx, |app, cx| {
                app.split_destination
                    .update(cx, |input, cx| input.set_value("", window, cx));
                app.start_split(cx);
                assert!(app.status_is_error);
                assert!(app.running.is_none());
                assert!(app.shell_setup == Some(ShellSetup::Split));
                app.split_size.update(cx, |select, cx| {
                    select.set_selected_index(
                        Some(gpui_kit::component::IndexPath::new(8)),
                        window,
                        cx,
                    );
                });
                assert_eq!(app.split_size.read(cx).selected_value(), Some(&"23040M-BD"));
            });
        });
    }

    #[gpui_kit::test]
    fn split_and_merge_completion_updates_progress_and_renders(cx: &mut gpui_kit::TestAppContext) {
        use super::{AppView, JobKind, JobOutput, progress::CompressionTiming};
        use gpui_kit::{base::Root, *};

        cx.update(gpui_kit::init);
        let (root, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|cx| AppView::new(window, cx));
            Root::new(view, window, cx)
        });
        let view = root.read_with(cx, |root, _| {
            root.view().clone().downcast::<AppView>().unwrap()
        });
        for (kind, label) in [(JobKind::Split, "分割完成"), (JobKind::Merge, "合并完成")] {
            view.update(cx, |app, cx| {
                app.shell_progress = true;
                app.job_kind = kind;
                app.job_timing = Some(CompressionTiming::new(Instant::now()));
                app.handle_job_event(
                    JobEvent::Finished {
                        result: Ok(JobOutput::FileProcessed("output".into())),
                        warnings: Vec::new(),
                        last_progress: Some(Progress {
                            operation: "文件处理",
                            current_file: "part".into(),
                            completed_bytes: 100,
                            total_bytes: 100,
                            archive_size: None,
                        }),
                        finished_at: Instant::now(),
                    },
                    cx,
                );
                assert!(app.running.is_none());
                assert!(!app.status_is_error);
                assert!(app.status.starts_with(label));
                assert!(app.job_timing.as_ref().unwrap().completed());
                assert_eq!(app.progress.as_ref().unwrap().completed_bytes, 100);
            });
            cx.update(|window, cx| window.draw(cx).clear(cx));
        }
    }

    fn progress(completed_bytes: u64, archive_size: u64) -> Progress {
        Progress {
            operation: "正在压缩",
            current_file: "source".to_owned(),
            completed_bytes,
            total_bytes: 100,
            archive_size: Some(archive_size),
        }
    }

    #[gpui_kit::test]
    fn drag_progress_waits_for_extraction_and_empty_folder_drop_succeeds(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        use super::{AppView, JobKind, JobOutput};
        use gpui_kit::{base::Root, *};
        use mzip::archive::CancellationToken;

        cx.update(gpui_kit::init);
        let (root, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|cx| AppView::new(window, cx));
            Root::new(view, window, cx)
        });
        let view = root.read_with(cx, |root, _| {
            root.view().clone().downcast::<AppView>().unwrap()
        });
        view.update(cx, |app, cx| {
            app.job_kind = JobKind::Drag;
            app.running = Some(CancellationToken::default());
            assert!(app.job_timing.is_none());
            assert!(app.progress.is_none());
            assert!(app.verify_progress.is_none());
            app.handle_job_event(
                JobEvent::Finished {
                    result: Ok(JobOutput::Dragged {
                        entries: None,
                        dropped: true,
                        destination: None,
                    }),
                    warnings: Vec::new(),
                    last_progress: None,
                    finished_at: Instant::now(),
                },
                cx,
            );
            assert!(app.status.contains("拖放已结束"));
            assert!(app.running.is_none());
            app.shell_progress = true;
            app.close_requested = true;
            app.handle_job_event(
                JobEvent::Finished {
                    result: Ok(JobOutput::Dragged {
                        entries: None,
                        dropped: true,
                        destination: Some(std::path::PathBuf::from(r"C:\direct-drop")),
                    }),
                    warnings: vec!["已跳过 ISO 符号链接：ubuntu".to_owned()],
                    last_progress: None,
                    finished_at: Instant::now(),
                },
                cx,
            );
            assert!(app.status.contains("已直接解压到"));
            assert!(app.status.contains("未交给 Windows 再次复制"));
            assert!(app.status.contains("已跳过 ISO 符号链接：ubuntu"));
            assert!(app.close_deadline.is_none());
            app.job_kind = JobKind::Drag;
            app.running = Some(CancellationToken::default());
            app.handle_job_event(JobEvent::Progress(progress(1, 0)), cx);
            assert!(app.job_kind == JobKind::Drag);
            assert!(app.job_timing.is_some());
            assert!(app.status.contains("正在提取"));
            let cancellation = app.running.as_ref().unwrap().clone();
            app.cancel(cx);
            assert!(cancellation.is_cancelled());
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
    }

    #[gpui_kit::test]
    fn compression_warnings_keep_shell_progress_open(cx: &mut gpui_kit::TestAppContext) {
        use std::path::PathBuf;

        use super::{AppView, JobKind, JobOutput};
        use gpui_kit::{base::Root, *};

        cx.update(gpui_kit::init);
        let (root, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|cx| AppView::new(window, cx));
            Root::new(view, window, cx)
        });
        let view = root.read_with(cx, |root, _| {
            root.view().clone().downcast::<AppView>().unwrap()
        });
        view.update(cx, |app, cx| {
            for (kind, output) in [
                (
                    JobKind::Create,
                    JobOutput::AutoCreated(PathBuf::from(r"C:\output.zip")),
                ),
                (
                    JobKind::CreateEach,
                    JobOutput::SeparatelyCreated(vec![PathBuf::from(r"C:\output.zip")]),
                ),
            ] {
                app.job_kind = kind;
                app.shell_progress = true;
                app.close_requested = true;
                app.handle_job_event(
                    JobEvent::Finished {
                        result: Ok(output),
                        warnings: vec!["扫描警告（仅保留空文件夹）：broken".to_owned()],
                        last_progress: None,
                        finished_at: Instant::now(),
                    },
                    cx,
                );
                assert!(app.status.contains("扫描警告"));
                assert!(app.status.contains("broken"));
                assert!(!app.status_is_error);
                assert!(app.close_deadline.is_none());
            }
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
    }

    #[gpui_kit::test]
    fn closing_view_cancels_pending_archive_drag(cx: &mut gpui_kit::TestAppContext) {
        use super::{AppView, JobKind};
        use gpui_kit::{base::Root, *};
        use mzip::archive::CancellationToken;

        cx.update(gpui_kit::init);
        let cancellation = CancellationToken::default();
        let token = cancellation.clone();
        cx.add_window_view(move |window, cx| {
            let view = cx.new(|cx| {
                let mut app = AppView::new(window, cx);
                app.job_kind = JobKind::Drag;
                app.running = Some(token);
                drop(app);
                AppView::new(window, cx)
            });
            Root::new(view, window, cx)
        });
        assert!(cancellation.is_cancelled());
    }

    #[gpui_kit::test]
    fn verification_progress_is_shown_separately(cx: &mut gpui_kit::TestAppContext) {
        use super::{AppView, JobKind, JobOutput, progress::CompressionTiming};
        use gpui_kit::{base::Root, *};
        use mzip::archive::IntegrityMode;

        cx.update(gpui_kit::init);
        let (root, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|cx| AppView::new(window, cx));
            Root::new(view, window, cx)
        });
        let view = root.read_with(cx, |root, _| {
            root.view().clone().downcast::<AppView>().unwrap()
        });
        for integrity in [IntegrityMode::Safe, IntegrityMode::Normal] {
            view.update(cx, |app, cx| {
                app.shell_progress = true;
                app.job_kind = JobKind::Create;
                app.job_integrity = integrity;
                app.progress = None;
                app.verify_progress = None;
                app.job_timing = Some(CompressionTiming::new(Instant::now()));
                app.handle_job_event(JobEvent::Progress(progress(60, 20)), cx);
                assert!(app.verify_progress.is_none());
            });
            cx.update(|window, cx| window.draw(cx).clear(cx));
            view.update(cx, |app, cx| {
                app.handle_job_event(
                    JobEvent::Progress(Progress {
                        operation: "正在校验压缩包",
                        current_file: "source".to_owned(),
                        completed_bytes: 30,
                        total_bytes: 100,
                        archive_size: Some(45),
                    }),
                    cx,
                );
                let main = app.progress.as_ref().unwrap();
                assert_eq!(main.operation, "正在压缩");
                assert_eq!(main.completed_bytes, 100);
                assert_eq!(main.archive_size, Some(45));
                assert_eq!(app.verify_progress.as_ref().unwrap().completed_bytes, 30);
            });
            cx.update(|window, cx| window.draw(cx).clear(cx));
            view.update(cx, |app, cx| {
                app.handle_job_event(
                    JobEvent::Finished {
                        result: Ok(JobOutput::AutoCreated("out.7z".into())),
                        warnings: Vec::new(),
                        last_progress: Some(Progress {
                            operation: "正在校验压缩包",
                            current_file: "source".to_owned(),
                            completed_bytes: 90,
                            total_bytes: 100,
                            archive_size: Some(50),
                        }),
                        finished_at: Instant::now(),
                    },
                    cx,
                );
                assert_eq!(app.progress.as_ref().unwrap().archive_size, Some(50));
                assert_eq!(app.verify_progress.as_ref().unwrap().completed_bytes, 100);
                app.close_deadline = None;
            });
            cx.update(|window, cx| window.draw(cx).clear(cx));
        }
    }

    #[gpui_kit::test]
    fn separate_compression_renders_and_preserves_partial_completion_message(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        use super::{AppView, JobKind, JobOutput};
        use gpui_kit::{base::Root, *};

        cx.update(gpui_kit::init);
        let (root, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|cx| AppView::new(window, cx));
            Root::new(view, window, cx)
        });
        let view = root.read_with(cx, |root, _| {
            root.view().clone().downcast::<AppView>().unwrap()
        });
        view.update(cx, |app, cx| {
            app.shell_progress = true;
            app.job_kind = JobKind::CreateEach;
            app.sources = vec!["first.txt".into(), "second.txt".into()];
            app.handle_job_event(JobEvent::Progress(progress(50, 20)), cx);
            assert!(app.job_timing.is_none());
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        for (result, expected) in [
            (
                Ok(JobOutput::SeparatelyCreated(vec![
                    "first.zip".into(),
                    "second.zip".into(),
                ])),
                "已生成 2 个 ZIP",
            ),
            (Err(mzip::archive::Cancelled.into()), "已完成的文件会保留"),
        ] {
            view.update(cx, |app, cx| {
                app.handle_job_event(
                    JobEvent::Finished {
                        result,
                        warnings: Vec::new(),
                        last_progress: Some(progress(100, 40)),
                        finished_at: Instant::now(),
                    },
                    cx,
                );
                assert!(app.progress.is_none());
                assert!(app.job_timing.is_none());
                assert!(!app.status_is_error);
                assert!(app.status.contains(expected));
                assert!(app.close_deadline.is_some());
            });
            cx.update(|window, cx| window.draw(cx).clear(cx));
        }
    }

    #[gpui_kit::test]
    fn settings_page_switches_and_persists_integrity_mode(cx: &mut gpui_kit::TestAppContext) {
        use super::{AppView, JobKind, Page, settings};
        use gpui_kit::{base::Root, *};
        use mzip::archive::IntegrityMode;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        cx.update(gpui_kit::init);
        let (root, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|cx| AppView::new(window, cx));
            Root::new(view, window, cx)
        });
        let view = root.read_with(cx, |root, _| {
            root.view().clone().downcast::<AppView>().unwrap()
        });
        view.update(cx, |app, cx| {
            app.integrity_mode = IntegrityMode::Safe;
            app.settings_path = Some(path.clone());
            app.page = Page::Settings;
            cx.notify();
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        view.update(cx, |app, cx| {
            app.set_integrity_mode(IntegrityMode::Normal, cx);
            assert!(app.integrity_mode == IntegrityMode::Normal);
            assert!(!app.status_is_error);
            assert!(app.status.contains("高速模式"));
        });
        assert_eq!(
            settings::load_integrity_mode(Some(&path)),
            IntegrityMode::Normal
        );
        cx.update(|window, cx| window.draw(cx).clear(cx));
        view.update(cx, |app, cx| {
            app.shell_progress = true;
            app.job_kind = JobKind::Extract;
            app.job_integrity = app.integrity_mode;
            app.handle_job_event(
                JobEvent::Finished {
                    result: Err(mzip::archive::Cancelled.into()),
                    warnings: Vec::new(),
                    last_progress: None,
                    finished_at: Instant::now(),
                },
                cx,
            );
            assert!(app.status.contains("已解压的文件会保留"));
            app.settings_path = None;
            app.set_integrity_mode(IntegrityMode::Safe, cx);
            assert!(app.status_is_error);
            assert!(app.integrity_mode == IntegrityMode::Normal);
        });
    }

    #[gpui_kit::test]
    fn extraction_cancellation_reports_rollback_but_recovery_errors_stay_open(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        use super::{AppView, JobKind};
        use gpui_kit::{base::Root, *};

        cx.update(gpui_kit::init);
        let (root, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|cx| AppView::new(window, cx));
            Root::new(view, window, cx)
        });
        let view = root.read_with(cx, |root, _| {
            root.view().clone().downcast::<AppView>().unwrap()
        });
        for (error, failed, message) in [
            (mzip::archive::Cancelled.into(), false, "输出已回滚"),
            (
                super::overwrite::ConfirmationCancelled.into(),
                false,
                "未写入目标文件",
            ),
            (
                anyhow::anyhow!("回滚或清理未完成"),
                true,
                "回滚或清理未完成",
            ),
        ] {
            view.update(cx, |app, cx| {
                app.shell_progress = true;
                app.job_kind = JobKind::Extract;
                app.job_integrity = mzip::archive::IntegrityMode::Safe;
                app.close_deadline = None;
                app.handle_job_event(
                    JobEvent::Finished {
                        result: Err(error),
                        warnings: Vec::new(),
                        last_progress: None,
                        finished_at: Instant::now(),
                    },
                    cx,
                );
                assert_eq!(app.status_is_error, failed);
                assert!(app.status.contains(message));
                assert_eq!(app.close_deadline.is_none(), failed);
            });
        }
    }

    #[test]
    fn latest_progress_is_retained_when_updates_are_throttled_or_the_channel_is_full() {
        let (sender, receiver) = async_channel::bounded(1);
        let mut dispatch = ProgressDispatch::default();
        let start = Instant::now();
        dispatch.report(progress(0, 0), &sender, start);
        dispatch.report(progress(50, 20), &sender, start + Duration::from_millis(50));
        dispatch.report(
            progress(100, 40),
            &sender,
            start + Duration::from_millis(150),
        );

        match receiver.try_recv().unwrap() {
            JobEvent::Progress(progress) => assert_eq!(progress.completed_bytes, 0),
            JobEvent::Finished { .. } => panic!("unexpected completion event"),
        }
        let last = dispatch.latest.unwrap();
        assert_eq!(last.completed_bytes, 100);
        assert_eq!(last.archive_size, Some(40));
    }
}
