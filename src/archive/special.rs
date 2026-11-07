use std::{
    collections::HashSet,
    fs::{self, File},
    io::Write,
    path::Path,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::common::{collision_key, safe_relative_path};

pub(super) const MANIFEST_NAME: &str = "Special_Cases_List.mhx6zip";
pub(super) const MAX_MANIFEST_SIZE: usize = 8 * 1024 * 1024;
const MAGIC: &str = "MHX6ZIP_SPECIAL_CASES_38A3D812_7C85_4FA4_A7AE_96DF27801C19";
const VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Manifest {
    pub magic: String,
    pub version: u32,
    pub entries: Vec<SpecialEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SpecialEntry {
    pub path: String,
    pub kind: SpecialKind,
    pub target: Option<String>,
    pub reason: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum SpecialKind {
    Junction,
    DirectorySymlink,
    FileSymlink,
    Skipped,
}

impl Manifest {
    pub fn new(entries: Vec<SpecialEntry>) -> Self {
        Self {
            magic: MAGIC.to_owned(),
            version: VERSION,
            entries,
        }
    }

    fn validate(&self) -> Result<()> {
        if self.magic != MAGIC || self.version != VERSION {
            bail!("特殊情况清单的标识或版本不受支持");
        }
        let mut paths = HashSet::new();
        for entry in &self.entries {
            let path = safe_relative_path(&entry.path)?;
            let key = collision_key(&path);
            if key == MANIFEST_NAME.to_ascii_lowercase() || !paths.insert(key) {
                bail!("特殊情况清单包含重复或保留路径：{}", entry.path);
            }
            if entry.kind != SpecialKind::Skipped
                && entry
                    .target
                    .as_deref()
                    .is_none_or(|target| target.is_empty() || target.contains('\0'))
            {
                bail!("特殊情况清单缺少有效的链接目标：{}", entry.path);
            }
        }
        Ok(())
    }
}

pub(super) fn parse(bytes: &[u8]) -> Result<Manifest> {
    if bytes.len() > MAX_MANIFEST_SIZE {
        bail!("特殊情况清单超过 8 MiB 限制");
    }
    let manifest: Manifest = serde_json::from_slice(bytes).context("无法读取特殊情况清单 JSON")?;
    manifest.validate()?;
    Ok(manifest)
}

pub(super) fn write(path: &Path, manifest: &Manifest) -> Result<()> {
    manifest.validate()?;
    let mut bytes = Vec::new();
    serde_json::to_writer_pretty(BoundedWriter(&mut bytes), manifest)
        .context("无法生成特殊情况清单（最大 8 MiB）")?;
    let mut file = File::create_new(path)
        .with_context(|| format!("无法创建特殊情况清单 {}", path.display()))?;
    file.write_all(&bytes).context("无法保存特殊情况清单")?;
    Ok(())
}

struct BoundedWriter<'a>(&'a mut Vec<u8>);

impl Write for BoundedWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_MANIFEST_SIZE.saturating_sub(self.0.len()) {
            return Err(std::io::Error::other("特殊情况清单超过 8 MiB 限制"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(super) fn restore_link(entry: &SpecialEntry, path: &Path) -> Result<()> {
    if entry.kind == SpecialKind::Skipped {
        bail!("诊断条目不包含可恢复的链接");
    }
    let target = entry.target.as_deref().context("链接目标缺失")?;
    if target.is_empty() || target.contains('\0') {
        bail!("链接目标无效");
    }
    #[cfg(windows)]
    {
        match entry.kind {
            SpecialKind::Junction => restore_junction(Path::new(target), path),
            SpecialKind::DirectorySymlink => {
                std::os::windows::fs::symlink_dir(target, path).context("无法恢复目录符号链接")
            }
            SpecialKind::FileSymlink => {
                std::os::windows::fs::symlink_file(target, path).context("无法恢复文件符号链接")
            }
            SpecialKind::Skipped => unreachable!(),
        }
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, path).context("无法恢复符号链接")
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = (target, path);
        bail!("当前系统不支持恢复链接")
    }
}

#[cfg(windows)]
fn restore_junction(target: &Path, path: &Path) -> Result<()> {
    use std::{
        ffi::c_void,
        os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle},
    };

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn DeviceIoControl(
            device: *mut c_void,
            code: u32,
            input: *const c_void,
            input_size: u32,
            output: *mut c_void,
            output_size: u32,
            returned: *mut u32,
            overlapped: *mut c_void,
        ) -> i32;
    }

    let target = if target.is_absolute() {
        target.to_owned()
    } else {
        std::path::absolute(path.parent().context("链接缺少父目录")?.join(target))?
    };
    let target = target.to_str().context("链接目标不是有效的 Unicode")?;
    let substitute = if let Some(rest) = target.strip_prefix(r"\\?\UNC\") {
        format!(r"\??\UNC\{rest}")
    } else if let Some(rest) = target.strip_prefix(r"\\?\") {
        format!(r"\??\{rest}")
    } else if target.starts_with(r"\??\") {
        target.to_owned()
    } else if let Some(rest) = target.strip_prefix(r"\\") {
        format!(r"\??\UNC\{rest}")
    } else {
        format!(r"\??\{target}")
    };
    let substitute = std::ffi::OsStr::new(&substitute)
        .encode_wide()
        .collect::<Vec<_>>();
    let print = std::ffi::OsStr::new(target)
        .encode_wide()
        .collect::<Vec<_>>();
    let substitute_len = u16::try_from(substitute.len() * 2).context("链接目标过长")?;
    let print_len = u16::try_from(print.len() * 2).context("链接目标过长")?;
    let data_len = 8usize + (substitute.len() + print.len() + 2) * 2;
    if data_len + 8 > 16 * 1024 {
        bail!("链接目标超过重解析点大小限制");
    }
    let mut buffer = Vec::with_capacity(data_len + 8);
    buffer.extend_from_slice(&0xA0000003u32.to_le_bytes()); // IO_REPARSE_TAG_MOUNT_POINT
    buffer.extend_from_slice(&(data_len as u16).to_le_bytes());
    buffer.extend_from_slice(&0u16.to_le_bytes());
    buffer.extend_from_slice(&0u16.to_le_bytes());
    buffer.extend_from_slice(&substitute_len.to_le_bytes());
    buffer.extend_from_slice(&(substitute_len + 2).to_le_bytes());
    buffer.extend_from_slice(&print_len.to_le_bytes());
    for character in substitute
        .iter()
        .chain([0u16].iter())
        .chain(print.iter())
        .chain([0u16].iter())
    {
        buffer.extend_from_slice(&character.to_le_bytes());
    }

    fs::create_dir(path).context("无法创建目录联接")?;
    let result = (|| -> Result<()> {
        let file = fs::OpenOptions::new()
            .write(true)
            .custom_flags(0x02000000 | 0x00200000) // BACKUP_SEMANTICS | OPEN_REPARSE_POINT
            .open(path)
            .context("无法打开目录联接")?;
        let mut returned = 0u32;
        // The byte buffer is a complete, bounded REPARSE_DATA_BUFFER for a mount point.
        let success = unsafe {
            DeviceIoControl(
                file.as_raw_handle(),
                0x000900A4, // FSCTL_SET_REPARSE_POINT
                buffer.as_ptr().cast(),
                buffer.len() as u32,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if success == 0 {
            return Err(std::io::Error::last_os_error()).context("无法设置目录联接目标");
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir(path);
    }
    result
}

pub(super) fn unresolved_link(path: &Path, name: &str) -> Option<SpecialEntry> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !super::common::is_link_or_reparse(&metadata) {
        return None;
    }
    let error = fs::metadata(path).err()?;
    // Permission and transient I/O errors are diagnostics, not restorable broken links.
    if error.kind() != std::io::ErrorKind::NotFound
        && error.kind() != std::io::ErrorKind::NotADirectory
    {
        return None;
    }
    let target = fs::read_link(path)
        .ok()
        .and_then(|target| target.to_str().map(str::to_owned));
    let kind = if target.is_none() {
        SpecialKind::Skipped
    } else {
        link_kind(path, &metadata)
    };
    Some(SpecialEntry {
        path: name.to_owned(),
        kind,
        target,
        reason: error.to_string(),
    })
}

pub(super) fn capture(path: &Path, name: &str, reason: &str) -> Result<SpecialEntry> {
    safe_relative_path(name)?;
    let mut entry = unresolved_link(path, name).unwrap_or_else(|| SpecialEntry {
        path: name.to_owned(),
        kind: SpecialKind::Skipped,
        target: None,
        reason: reason.to_owned(),
    });
    entry.reason = reason.to_owned();
    Ok(entry)
}

fn link_kind(path: &Path, metadata: &fs::Metadata) -> SpecialKind {
    #[cfg(windows)]
    {
        use std::os::windows::fs::{FileTypeExt, MetadataExt};
        if reparse_tag(path) == Some(0xA0000003) {
            SpecialKind::Junction
        } else if metadata.file_type().is_symlink_dir() {
            SpecialKind::DirectorySymlink
        } else if metadata.file_type().is_symlink_file() {
            SpecialKind::FileSymlink
        } else if metadata.file_attributes() & 0x10 != 0 {
            SpecialKind::Junction
        } else {
            SpecialKind::FileSymlink
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (path, metadata);
        SpecialKind::FileSymlink
    }
}

#[cfg(windows)]
fn reparse_tag(path: &Path) -> Option<u32> {
    use std::{
        ffi::c_void,
        os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
    };

    #[repr(C)]
    struct AttributeTag {
        attributes: u32,
        tag: u32,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFileInformationByHandleEx(
            file: *mut c_void,
            class: i32,
            information: *mut c_void,
            size: u32,
        ) -> i32;
    }

    let file = fs::OpenOptions::new()
        .access_mode(0)
        .custom_flags(0x02000000 | 0x00200000)
        .open(path)
        .ok()?;
    let mut information = AttributeTag {
        attributes: 0,
        tag: 0,
    };
    // FileAttributeTagInfo describes the reparse point itself, not its missing target.
    let success = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            9,
            (&mut information as *mut AttributeTag).cast(),
            std::mem::size_of::<AttributeTag>() as u32,
        )
    };
    (success != 0).then_some(information.tag)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example() -> Manifest {
        Manifest::new(vec![SpecialEntry {
            path: "folder/link".to_owned(),
            kind: SpecialKind::DirectorySymlink,
            target: Some("../missing directory".to_owned()),
            reason: "target not found".to_owned(),
        }])
    }

    #[test]
    fn manifest_round_trip_and_write_never_overwrites() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let path = directory.path().join(MANIFEST_NAME);
        let manifest = example();
        write(&path, &manifest).unwrap();
        let bytes = fs::read(&path).unwrap();
        assert_eq!(parse(&bytes).unwrap().entries, manifest.entries);
        assert!(write(&path, &Manifest::new(Vec::new())).is_err());
        assert_eq!(fs::read(path).unwrap(), bytes);
    }

    #[test]
    fn malformed_and_unknown_manifest_fields_are_rejected() {
        for bytes in [
            b"".as_slice(),
            b"not JSON".as_slice(),
            b"\xff".as_slice(),
            br#"{"entries":[]}"#.as_slice(),
        ] {
            assert!(parse(bytes).is_err());
        }
        let original = serde_json::to_value(example()).unwrap();
        for (field, value) in [
            ("magic", serde_json::json!("another format")),
            ("version", serde_json::json!(VERSION + 1)),
            ("unknown", serde_json::json!(true)),
        ] {
            let mut manifest = original.clone();
            manifest[field] = value;
            assert!(parse(&serde_json::to_vec(&manifest).unwrap()).is_err());
        }
        for (field, value) in [
            ("kind", serde_json::json!("unknown_link")),
            ("unknown", serde_json::json!(true)),
            ("target", serde_json::Value::Null),
            ("target", serde_json::json!("")),
            ("target", serde_json::json!("bad\u{0}target")),
            ("path", serde_json::json!("../escape")),
            ("path", serde_json::json!(MANIFEST_NAME)),
        ] {
            let mut manifest = original.clone();
            manifest["entries"][0][field] = value;
            assert!(parse(&serde_json::to_vec(&manifest).unwrap()).is_err());
        }
    }

    #[test]
    fn duplicate_paths_and_oversized_manifests_are_rejected() {
        let mut manifest = example();
        let mut duplicate = manifest.entries[0].clone();
        duplicate.path = duplicate.path.to_uppercase();
        manifest.entries.push(duplicate);
        assert!(parse(&serde_json::to_vec(&manifest).unwrap()).is_err());
        assert!(parse(&vec![b' '; MAX_MANIFEST_SIZE + 1]).is_err());

        let directory = tempfile::tempdir_in(".").unwrap();
        let path = directory.path().join(MANIFEST_NAME);
        let mut manifest = example();
        manifest.entries[0].reason = "x".repeat(MAX_MANIFEST_SIZE);
        assert!(write(&path, &manifest).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn diagnostics_are_not_restorable() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let missing = directory.path().join("missing");
        let entry = capture(&missing, "missing", "cannot read").unwrap();
        assert_eq!(entry.kind, SpecialKind::Skipped);
        assert_eq!(entry.target, None);
        assert!(restore_link(&entry, &missing).is_err());
        assert!(!missing.exists());
    }

    #[cfg(unix)]
    #[test]
    fn relative_file_and_directory_symlinks_capture_and_restore() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let source = directory.path().join("source");
        let output = directory.path().join("output");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&output).unwrap();
        for (name, target, is_directory) in [
            ("file-link", "missing-file", false),
            ("directory-link", "missing-directory", true),
        ] {
            let source_link = source.join(name);
            std::os::unix::fs::symlink(target, &source_link).unwrap();
            let entry = capture(&source_link, name, "missing target").unwrap();
            // Unix symlinks do not carry the target's file/directory type.
            assert_eq!(entry.kind, SpecialKind::FileSymlink);
            assert_eq!(entry.target.as_deref(), Some(target));
            let restored = output.join(name);
            restore_link(&entry, &restored).unwrap();
            assert_eq!(fs::read_link(&restored).unwrap(), Path::new(target));
            assert!(
                fs::symlink_metadata(&restored)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert!(restore_link(&entry, &restored).is_err());
            if is_directory {
                fs::create_dir(output.join(target)).unwrap();
                assert!(restored.is_dir());
            } else {
                fs::write(output.join(target), b"restored file target").unwrap();
                assert_eq!(fs::read(&restored).unwrap(), b"restored file target");
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn junction_capture_and_restore_use_native_reparse_points() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let target = std::path::absolute(directory.path().join("missing directory")).unwrap();
        let original = directory.path().join("original");
        restore_junction(&target, &original).unwrap();
        let entry = capture(&original, "original", "missing target").unwrap();
        assert_eq!(entry.kind, SpecialKind::Junction);
        let restored = directory.path().join("restored");
        restore_link(&entry, &restored).unwrap();
        assert_eq!(reparse_tag(&restored), Some(0xA0000003));
        assert_eq!(
            fs::read_link(&restored).unwrap(),
            fs::read_link(&original).unwrap()
        );
        assert!(restore_link(&entry, &restored).is_err());
    }
}
