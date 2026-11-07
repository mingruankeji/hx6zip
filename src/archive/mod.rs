mod common;
mod create;
mod edit;
mod extract;
mod inspect;
mod iso;
mod other;
mod password;
mod rar;
mod sfx;
mod special;
mod timestamps;
mod update;
mod volumes;
mod zip_drag;

use std::{
    collections::BTreeSet,
    fmt,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
    time::SystemTime,
};

use anyhow::{Result, bail};

pub use create::{create_archive, create_archive_auto, create_archives_separately};
pub use edit::{
    ArchiveEdit, ArchiveTree, ExportedEntries, archive_children, archive_selection, edit_archive,
    editable_format, export_entries, import_conflicts,
};
pub use extract::{
    drag_conflicts, extract_archive, extract_conflicts, extract_drag_entries, named_destination,
    smart_destination,
};
pub use inspect::inspect_archive;
pub use password::PasswordPrompt;
pub use volumes::{merge_files, split_file};
pub use zip_drag::{ZipDragReader, ZipDragSource};

/// Entry limit for CAB, ISO, RAR, and TAR; ZIP and 7z have no fixed entry limit.
pub const MAX_ARCHIVE_ENTRIES: usize = 100_000;
const IO_BUFFER_SIZE: usize = 64 * 1024;
const SEVENZIP_CHUNK_SIZE: u64 = 16 * 1024 * 1024;
const SEVENZIP_PARALLEL_THRESHOLD: u64 = 2 * SEVENZIP_CHUNK_SIZE;
pub use hx6zip_shell_protocol::SUPPORTED_EXTENSIONS;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArchiveFormat {
    Zip,
    SevenZip,
    Tar,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Compression {
    None,
    Gzip,
    Bzip2,
    Xz,
    Zstd,
    Brotli,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArchiveKind {
    Zip,
    SevenZip,
    Cab,
    Rar,
    Iso,
    Tar(Compression),
    Stream(Compression),
}

impl ArchiveKind {
    pub fn from_path(path: &Path) -> Result<Self> {
        let name = path
            .file_name()
            .map(|value| value.to_string_lossy())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let kind = if [
            ".zip", ".zipx", ".jar", ".odt", ".ods", ".odp", ".odg", ".odf", ".odb", ".sxw",
            ".sxc", ".sxi", ".sxd",
        ]
        .iter()
        .any(|extension| name.ends_with(extension))
        {
            Self::Zip
        } else if name.ends_with(".7z") {
            Self::SevenZip
        } else if name.ends_with(".cab") {
            Self::Cab
        } else if name.ends_with(".rar") {
            Self::Rar
        } else if name.ends_with(".iso") {
            Self::Iso
        } else if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
            Self::Tar(Compression::Gzip)
        } else if name.ends_with(".tar.bz2") || name.ends_with(".tbz") || name.ends_with(".tbz2") {
            Self::Tar(Compression::Bzip2)
        } else if name.ends_with(".tar.xz") || name.ends_with(".txz") {
            Self::Tar(Compression::Xz)
        } else if name.ends_with(".tar.zst")
            || name.ends_with(".tar.zstd")
            || name.ends_with(".tzst")
        {
            Self::Tar(Compression::Zstd)
        } else if name.ends_with(".tar.br")
            || name.ends_with(".tar.brotli")
            || name.ends_with(".tbr")
        {
            Self::Tar(Compression::Brotli)
        } else if name.ends_with(".tar") {
            Self::Tar(Compression::None)
        } else if name.ends_with(".gz") {
            Self::Stream(Compression::Gzip)
        } else if name.ends_with(".bz2") {
            Self::Stream(Compression::Bzip2)
        } else if name.ends_with(".xz") {
            Self::Stream(Compression::Xz)
        } else if name.ends_with(".zst") || name.ends_with(".zstd") {
            Self::Stream(Compression::Zstd)
        } else if name.ends_with(".br") || name.ends_with(".brotli") {
            Self::Stream(Compression::Brotli)
        } else {
            bail!("暂不支持此格式，请选择支持的压缩文件或 ISO 镜像");
        };
        Ok(kind)
    }
}

impl ArchiveFormat {
    pub fn from_path(path: &Path) -> Result<Self> {
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        if extension.eq_ignore_ascii_case("zip") {
            Ok(Self::Zip)
        } else if extension.eq_ignore_ascii_case("7z") {
            Ok(Self::SevenZip)
        } else if extension.eq_ignore_ascii_case("tar") {
            Ok(Self::Tar)
        } else {
            bail!("暂不支持此格式，请选择 ZIP、7z 或 TAR 文件")
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Zip => "zip",
            Self::SevenZip => "7z",
            Self::Tar => "tar",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArchiveEntry {
    pub index: usize,
    pub name: String,
    pub size: u64,
    pub compressed_size: Option<u64>,
    pub is_directory: bool,
    pub is_encrypted: Option<bool>,
    pub unsupported_kind: bool,
    pub is_symbolic_link: bool,
    pub modified: Option<SystemTime>,
}

#[derive(Clone, Debug)]
pub struct ArchiveInfo {
    pub path: PathBuf,
    pub format: ArchiveKind,
    pub entries: Vec<ArchiveEntry>,
    pub total_size: u64,
    pub archive_size: u64,
    modified: SystemTime,
}

impl ArchiveInfo {
    pub fn skips_iso_symlink(&self, entry: &ArchiveEntry) -> bool {
        self.format == ArchiveKind::Iso && entry.is_symbolic_link
    }
}

#[derive(Clone)]
pub struct CreateOptions {
    pub sources: Vec<PathBuf>,
    pub destination: PathBuf,
    pub format: ArchiveFormat,
    pub password: Option<String>,
    pub overwrite: bool,
}

#[derive(Clone)]
pub struct ExtractOptions {
    pub destination: PathBuf,
    pub password: Option<String>,
    pub selected: Option<BTreeSet<usize>>,
    pub overwrite: bool,
}

#[derive(Clone, Debug)]
pub struct Progress {
    pub operation: &'static str,
    pub current_file: String,
    pub completed_bytes: u64,
    pub total_bytes: u64,
    /// Current archive length during creation, including buffered output.
    pub archive_size: Option<u64>,
}

#[derive(Clone, Default)]
pub struct CancellationToken(Arc<TaskState>);

#[derive(Default)]
struct TaskState {
    flags: Mutex<TaskFlags>,
    changed: Condvar,
}

#[derive(Default)]
struct TaskFlags {
    cancelled: bool,
    paused: bool,
}

impl CancellationToken {
    pub fn cancel(&self) {
        self.0.flags.lock().expect("task state poisoned").cancelled = true;
        self.0.changed.notify_all();
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.flags.lock().expect("task state poisoned").cancelled
    }

    pub fn set_paused(&self, paused: bool) {
        self.0.flags.lock().expect("task state poisoned").paused = paused;
        self.0.changed.notify_all();
    }

    pub fn is_paused(&self) -> bool {
        self.0.flags.lock().expect("task state poisoned").paused
    }

    fn checkpoint(&self) -> Result<()> {
        let flags = self.0.flags.lock().expect("task state poisoned");
        let flags = self
            .0
            .changed
            .wait_while(flags, |flags| flags.paused && !flags.cancelled)
            .expect("task state poisoned");
        if flags.cancelled {
            Err(Cancelled.into())
        } else {
            Ok(())
        }
    }
}

#[derive(Debug)]
pub struct Cancelled;

impl fmt::Display for Cancelled {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "操作已取消")
    }
}

impl std::error::Error for Cancelled {}

/// How strictly archive jobs guarantee complete and identical results.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum IntegrityMode {
    /// Re-read created archives and compare them with the sources byte by byte; extract into a
    /// staging area and roll back the whole archive when any entry fails.
    #[default]
    Safe,
    /// Behaves like common archivers such as 7-Zip: no read-back comparison after creation, and
    /// entries extracted before a failure are kept.
    Normal,
}

impl IntegrityMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Safe => "安全模式",
            Self::Normal => "高速模式",
        }
    }
}

#[derive(Clone)]
pub struct JobControl {
    cancellation: CancellationToken,
    progress: Arc<dyn Fn(Progress) + Send + Sync>,
    integrity: IntegrityMode,
    warnings: Arc<Mutex<Vec<String>>>,
    password_prompt: Option<PasswordPrompt>,
}

impl Default for JobControl {
    fn default() -> Self {
        Self::new(CancellationToken::default(), |_| {})
    }
}

impl JobControl {
    pub fn new(
        cancellation: CancellationToken,
        progress: impl Fn(Progress) + Send + Sync + 'static,
    ) -> Self {
        Self {
            cancellation,
            progress: Arc::new(progress),
            integrity: IntegrityMode::default(),
            warnings: Arc::new(Mutex::new(Vec::new())),
            password_prompt: None,
        }
    }

    pub fn with_integrity(mut self, integrity: IntegrityMode) -> Self {
        self.integrity = integrity;
        self
    }

    fn with_shared_warnings(mut self, parent: &Self) -> Self {
        self.warnings = parent.warnings.clone();
        self
    }

    pub fn integrity(&self) -> IntegrityMode {
        self.integrity
    }

    pub fn warnings(&self) -> Vec<String> {
        self.warnings
            .lock()
            .expect("task warnings poisoned")
            .clone()
    }

    fn warn(&self, message: String) {
        let mut warnings = self.warnings.lock().expect("task warnings poisoned");
        if !warnings.contains(&message) {
            eprintln!("{message}");
            warnings.push(message);
        }
    }

    pub fn check_cancelled(&self) -> Result<()> {
        self.cancellation.checkpoint()
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    fn report(&self, progress: Progress) {
        (self.progress)(progress);
    }
}
