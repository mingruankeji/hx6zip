use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, Metadata},
    io::Read,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use tempfile::TempDir;
use walkdir::WalkDir;

use super::{
    ArchiveEntry, ArchiveFormat, ArchiveInfo, ArchiveKind, CreateOptions, ExtractOptions,
    JobControl,
    common::{
        Tracker, checked_metadata, collision_key, copy_stream, followed_source_metadata,
        is_link_or_reparse, safe_relative_path, scan_source_metadata, source_walk_path,
    },
    create::create_updated_archive,
    extract::extract_archive_for_export,
    inspect_archive,
    special::{self, SpecialEntry},
    timestamps::{set_directory_modified, set_file_modified},
};

pub enum ArchiveEdit {
    Add {
        sources: Vec<PathBuf>,
        directory: String,
        overwrite: bool,
    },
    Delete {
        paths: Vec<String>,
    },
}

fn normalized(name: &str) -> Result<String> {
    Ok(safe_relative_path(name)?
        .to_string_lossy()
        .replace('\\', "/"))
}

pub fn archive_children(info: &ArchiveInfo, directory: &str) -> Result<Vec<ArchiveEntry>> {
    Ok(ArchiveTree::new(info, &JobControl::default())?
        .children(directory)?
        .to_vec())
}

pub struct ArchiveTree {
    folders: BTreeMap<String, Vec<ArchiveEntry>>,
}

impl ArchiveTree {
    pub fn new(info: &ArchiveInfo, control: &JobControl) -> Result<Self> {
        let mut folders = BTreeMap::<String, BTreeMap<String, ArchiveEntry>>::new();
        folders.insert(String::new(), BTreeMap::new());
        for entry in &info.entries {
            control.check_cancelled()?;
            let name = normalized(&entry.name)?;
            let parts = name.split('/').collect::<Vec<_>>();
            let mut parent = String::new();
            for (index, part) in parts.iter().enumerate() {
                let full = if parent.is_empty() {
                    (*part).to_owned()
                } else {
                    format!("{parent}/{part}")
                };
                let explicit = index + 1 == parts.len();
                let directory = !explicit || entry.is_directory;
                let children = folders.entry(parent.to_lowercase()).or_default();
                let key = part.to_lowercase();
                if let Some(previous) = children.get(&key)
                    && (!previous.is_directory || !directory)
                {
                    bail!("压缩包包含重复或冲突的路径：{full}");
                }
                let child = children.entry(key).or_insert_with(|| ArchiveEntry {
                    name: full.clone(),
                    is_directory: directory,
                    size: if explicit { entry.size } else { 0 },
                    modified: if explicit { entry.modified } else { None },
                    ..entry.clone()
                });
                if explicit {
                    *child = ArchiveEntry {
                        name: full.clone(),
                        ..entry.clone()
                    };
                }
                if directory {
                    folders.entry(full.to_lowercase()).or_default();
                }
                parent = full;
            }
        }
        let folders = folders
            .into_iter()
            .map(|(key, children)| {
                let (mut directories, files): (Vec<_>, Vec<_>) =
                    children.into_values().partition(|entry| entry.is_directory);
                directories.extend(files);
                (key, directories)
            })
            .collect();
        Ok(Self { folders })
    }

    pub fn children(&self, directory: &str) -> Result<&[ArchiveEntry]> {
        let key = if directory.is_empty() {
            String::new()
        } else {
            normalized(directory)?.to_lowercase()
        };
        self.folders
            .get(&key)
            .map(Vec::as_slice)
            .context("压缩包中的文件夹已不存在，请刷新或返回上一级")
    }
}

pub fn archive_selection(info: &ArchiveInfo, paths: &[String]) -> Result<BTreeSet<usize>> {
    let paths = paths
        .iter()
        .map(|name| normalized(name).map(|name| name.to_lowercase()))
        .collect::<Result<BTreeSet<_>>>()?;
    let mut selected = BTreeSet::new();
    for entry in &info.entries {
        let relative = safe_relative_path(&entry.name)?;
        if relative
            .ancestors()
            .any(|ancestor| paths.contains(&collision_key(ancestor)))
        {
            selected.insert(entry.index);
        }
    }
    if selected.is_empty() {
        bail!("请选择压缩包中的文件或文件夹");
    }
    Ok(selected)
}

pub fn editable_format(info: &ArchiveInfo) -> Result<ArchiveFormat> {
    let format = ArchiveFormat::from_path(&info.path)
        .context("此格式只支持浏览和解压；只有普通 ZIP、7z 压缩包可以修改")?;
    if !matches!(
        (format, info.format),
        (ArchiveFormat::Zip, ArchiveKind::Zip) | (ArchiveFormat::SevenZip, ArchiveKind::SevenZip)
    ) {
        bail!("压缩包格式与扩展名不一致，不能修改");
    }
    let mut header = [0; 6];
    File::open(&info.path)?.read_exact(&mut header)?;
    let ordinary = match format {
        ArchiveFormat::Zip => {
            header.starts_with(b"PK\x03\x04") || header.starts_with(b"PK\x05\x06")
        }
        ArchiveFormat::SevenZip => header == *b"7z\xbc\xaf\x27\x1c",
        ArchiveFormat::Tar => false,
    };
    if !ordinary {
        bail!("自解压文件只支持浏览和解压，不能修改");
    }
    Ok(format)
}

struct Import {
    source: PathBuf,
    relative: PathBuf,
    directory: bool,
    metadata: Metadata,
    special: Option<SpecialEntry>,
}

impl Import {
    fn diagnostic_only(&self) -> bool {
        self.special
            .as_ref()
            .is_some_and(|entry| matches!(entry.kind, special::SpecialKind::Skipped))
    }
}

fn imports(sources: &[PathBuf], directory: &str, control: &JobControl) -> Result<Vec<Import>> {
    if sources.is_empty() {
        bail!("请选择要添加的文件或文件夹");
    }
    let base = if directory.is_empty() {
        PathBuf::new()
    } else {
        safe_relative_path(directory)?
    };
    let mut result = Vec::new();
    let mut names = BTreeSet::new();
    for source in sources {
        let root = source.file_name().context("不能添加磁盘根目录")?;
        for item in WalkDir::new(source).follow_links(true) {
            control.check_cancelled()?;
            let failed_path = item.as_ref().err().and_then(|error| error.path());
            if let Some(path) = failed_path
                && item
                    .as_ref()
                    .err()
                    .is_some_and(|error| error.loop_ancestor().is_none())
                && let Ok(metadata) = fs::symlink_metadata(path)
                && is_link_or_reparse(&metadata)
                && let Err(error) = fs::metadata(path)
            {
                let relative = base.join(root).join(path.strip_prefix(source)?);
                let relative = safe_relative_path(&relative.to_string_lossy())?;
                if !names.insert(collision_key(&relative)) {
                    bail!("拖入的项目包含重复名称：{}", relative.display());
                }
                let name = relative.to_string_lossy().replace('\\', "/");
                let special = special::capture(path, &name, &error.to_string())?;
                if special.kind == special::SpecialKind::Skipped {
                    control.warn(format!("扫描警告（已跳过）：{}：{error}", path.display()));
                }
                result.push(Import {
                    source: path.to_owned(),
                    relative,
                    directory: !matches!(special.kind, special::SpecialKind::FileSymlink),
                    metadata,
                    special: Some(special),
                });
                continue;
            }
            let Some(path) = source_walk_path(item, control)? else {
                continue;
            };
            let Some(metadata) = scan_source_metadata(&path, control)? else {
                continue;
            };
            let relative = base.join(root).join(path.strip_prefix(source)?);
            let relative = safe_relative_path(&relative.to_string_lossy())?;
            if !names.insert(collision_key(&relative)) {
                bail!("拖入的项目包含重复名称：{}", relative.display());
            }
            result.push(Import {
                source: path,
                relative,
                directory: metadata.is_dir() || is_link_or_reparse(&metadata),
                metadata,
                special: None,
            });
        }
    }
    Ok(result)
}

fn conflicts(info: &ArchiveInfo, incoming: &[Import]) -> Result<Vec<String>> {
    let mut existing = BTreeMap::new();
    for entry in &info.entries {
        let path = safe_relative_path(&entry.name)?;
        existing.insert(collision_key(&path), entry.is_directory);
        for parent in path
            .ancestors()
            .skip(1)
            .filter(|p| !p.as_os_str().is_empty())
        {
            existing.entry(collision_key(parent)).or_insert(true);
        }
    }
    let mut result = BTreeSet::new();
    for item in incoming.iter().filter(|item| !item.diagnostic_only()) {
        let key = collision_key(&item.relative);
        if let Some(directory) = existing.get(&key)
            && (!item.directory || !directory || item.special.is_some())
        {
            result.insert(item.relative.to_string_lossy().into_owned());
        }
        for parent in item.relative.ancestors().skip(1) {
            if existing.get(&collision_key(parent)) == Some(&false) {
                result.insert(parent.to_string_lossy().into_owned());
            }
        }
    }
    Ok(result.into_iter().collect())
}

pub fn import_conflicts(
    info: &ArchiveInfo,
    sources: &[PathBuf],
    directory: &str,
    control: &JobControl,
) -> Result<Vec<String>> {
    editable_format(info)?;
    conflicts(info, &imports(sources, directory, control)?)
}

fn unchanged(info: &ArchiveInfo, password: Option<&str>, control: &JobControl) -> Result<()> {
    let fresh = inspect_archive(&info.path, password, control)?;
    if fresh.archive_size != info.archive_size
        || fresh.modified != info.modified
        || fresh.entries != info.entries
    {
        bail!("压缩包已被其他程序修改，请刷新后重试");
    }
    Ok(())
}

fn remove(path: &Path) -> Result<()> {
    if let Some(metadata) = checked_metadata(path)? {
        if metadata.is_dir() {
            fs::remove_dir_all(path)?;
        } else {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

fn retained_entries(
    info: &ArchiveInfo,
    edit: &ArchiveEdit,
    incoming: &[Import],
) -> Result<BTreeSet<usize>> {
    let removed = match edit {
        ArchiveEdit::Delete { paths } => archive_selection(info, paths)?,
        ArchiveEdit::Add { .. } => BTreeSet::new(),
    };
    let replacements = incoming
        .iter()
        .filter(|item| !item.diagnostic_only())
        .map(|item| collision_key(&item.relative))
        .collect::<BTreeSet<_>>();
    let files = incoming
        .iter()
        .filter(|item| !item.diagnostic_only() && (!item.directory || item.special.is_some()))
        .map(|item| collision_key(&item.relative))
        .collect::<BTreeSet<_>>();
    let parents = incoming
        .iter()
        .filter(|item| !item.diagnostic_only())
        .flat_map(|item| item.relative.ancestors().skip(1))
        .map(collision_key)
        .collect::<BTreeSet<_>>();
    let mut retained = BTreeSet::new();
    for entry in &info.entries {
        let relative = safe_relative_path(&entry.name)?;
        let key = collision_key(&relative);
        if !removed.contains(&entry.index)
            && !replacements.contains(&key)
            && (entry.is_directory || !parents.contains(&key))
            && !relative
                .ancestors()
                .skip(1)
                .any(|parent| files.contains(&collision_key(parent)))
        {
            retained.insert(entry.index);
        }
    }
    Ok(retained)
}

pub fn edit_archive(
    info: &ArchiveInfo,
    edit: &ArchiveEdit,
    password: Option<&str>,
    control: &JobControl,
) -> Result<ArchiveInfo> {
    editable_format(info)?;
    control.with_archive_password(
        &info.path,
        password,
        info.entries
            .iter()
            .any(|entry| entry.is_encrypted == Some(true)),
        true,
        |password| edit_archive_with_password(info, edit, password, control),
    )
}

fn edit_archive_with_password(
    info: &ArchiveInfo,
    edit: &ArchiveEdit,
    password: Option<&str>,
    control: &JobControl,
) -> Result<ArchiveInfo> {
    let format = editable_format(info)?;
    control.check_cancelled()?;
    super::extract::validate_edit_entries(info)?;
    unchanged(info, password, control)?;
    let encrypted = if format == ArchiveFormat::SevenZip {
        match sevenz_rust2::ArchiveReader::new(
            File::open(&info.path)?,
            sevenz_rust2::Password::empty(),
        ) {
            Ok(reader) => {
                let encrypted = reader.archive().blocks.iter().any(|block| {
                    block.coders.iter().any(|coder| {
                        coder.encoder_method_id() == sevenz_rust2::EncoderMethod::ID_AES256_SHA256
                    })
                });
                if encrypted {
                    super::update::verify_data_password(
                        &info.path,
                        reader.archive(),
                        password
                            .filter(|value| !value.is_empty())
                            .context("修改加密压缩包前请输入密码")?,
                        control,
                    )?;
                }
                encrypted
            }
            Err(sevenz_rust2::Error::PasswordRequired) => true,
            Err(error) => return Err(error).context("无法检查 7z 加密方式"),
        }
    } else {
        info.entries
            .iter()
            .any(|entry| entry.is_encrypted == Some(true))
    };
    if encrypted && password.is_none_or(str::is_empty) {
        bail!("修改加密压缩包前请输入密码");
    }
    if format == ArchiveFormat::Zip && encrypted {
        let mut reader = zip::ZipArchive::new(std::io::BufReader::new(File::open(&info.path)?))?;
        if let Some(entry) = info
            .entries
            .iter()
            .find(|entry| entry.is_encrypted == Some(true))
        {
            reader
                .by_index_decrypt(
                    entry.index,
                    password.context("请输入压缩包密码")?.as_bytes(),
                )
                .context("ZIP 密码不正确")?;
        }
    }
    let parent = info
        .path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let incoming = match edit {
        ArchiveEdit::Add {
            sources,
            directory,
            overwrite,
        } => {
            let incoming = imports(sources, directory, control)?;
            let archive_path = fs::canonicalize(&info.path)?;
            for item in incoming
                .iter()
                .filter(|item| !item.directory && item.special.is_none())
            {
                if fs::canonicalize(&item.source)? == archive_path {
                    bail!("不能把正在修改的压缩包添加到自身；请选择其他文件或文件夹");
                }
            }
            if !overwrite && !conflicts(info, &incoming)?.is_empty() {
                bail!("存在同名项目，请确认替换后重试");
            }
            incoming
        }
        ArchiveEdit::Delete { paths } => {
            archive_selection(info, paths)?;
            Vec::new()
        }
    };
    if matches!(edit, ArchiveEdit::Add { .. }) && incoming.is_empty() {
        return Ok(info.clone());
    }
    let mut retained = retained_entries(info, edit, &incoming)?;
    let previous_manifest = match super::extract::read_special_manifest(info, password, control) {
        Ok(manifest) => manifest,
        Err(error) => {
            control.check_cancelled()?;
            control.warn(format!("原有特殊情况清单未修改：{error:#}"));
            None
        }
    };
    let had_manifest = previous_manifest.is_some();
    let mut special_entries = previous_manifest
        .map(|manifest| manifest.entries)
        .unwrap_or_default();
    let deleted = match edit {
        ArchiveEdit::Delete { paths } => paths
            .iter()
            .map(|path| safe_relative_path(path).map(|path| collision_key(&path)))
            .collect::<Result<BTreeSet<_>>>()?,
        ArchiveEdit::Add { .. } => BTreeSet::new(),
    };
    special_entries.retain(|entry| {
        let path = PathBuf::from(entry.path.replace('\\', "/"));
        !deleted.contains(&special::MANIFEST_NAME.to_lowercase())
            && !path
                .ancestors()
                .any(|parent| deleted.contains(&collision_key(parent)))
            && !incoming
                .iter()
                .filter(|item| !item.diagnostic_only())
                .any(|item| {
                    let key = collision_key(&item.relative);
                    collision_key(&path) == key
                        || ((!item.directory || item.special.is_some())
                            && path.ancestors().any(|parent| collision_key(parent) == key))
                })
    });
    for item in &incoming {
        if let Some(entry) = &item.special {
            let key = collision_key(&item.relative);
            special_entries.retain(|previous| collision_key(Path::new(&previous.path)) != key);
            special_entries.push(entry.clone());
        }
    }
    let manifest_entry = info
        .entries
        .iter()
        .find(|entry| entry.name.eq_ignore_ascii_case(special::MANIFEST_NAME));
    if !special_entries.is_empty() {
        if manifest_entry.is_some_and(|entry| retained.contains(&entry.index)) && !had_manifest {
            bail!(
                "特殊情况清单名称与已有用户文件冲突：{}",
                special::MANIFEST_NAME
            );
        }
        if incoming
            .iter()
            .any(|item| collision_key(&item.relative) == special::MANIFEST_NAME.to_lowercase())
        {
            bail!(
                "特殊情况清单名称与添加的用户文件冲突：{}",
                special::MANIFEST_NAME
            );
        }
    }
    if had_manifest && let Some(entry) = manifest_entry {
        retained.remove(&entry.index);
    }
    let sevenzip = if format == ArchiveFormat::SevenZip {
        Some(super::update::plan_sevenzip(
            info, &retained, password, control,
        )?)
    } else {
        None
    };
    let work = tempfile::Builder::new()
        .prefix(".hx6zip-edit-")
        .tempdir_in(parent)?;
    let data = work.path().join("data");
    fs::create_dir(&data)?;
    if let Some(plan) = &sevenzip
        && !plan.repack.is_empty()
    {
        super::extract::extract_archive_without_special(
            info,
            &ExtractOptions {
                destination: data.clone(),
                password: password.map(str::to_owned),
                selected: Some(plan.repack.clone()),
                overwrite: false,
            },
            control,
        )?;
    }
    if matches!(edit, ArchiveEdit::Add { .. }) {
        for item in incoming.iter().filter(|item| item.special.is_none()) {
            control.check_cancelled()?;
            let target = data.join(&item.relative);
            for parent in item
                .relative
                .ancestors()
                .skip(1)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
            {
                let path = data.join(parent);
                if checked_metadata(&path)?.is_some_and(|m| !m.is_dir()) {
                    remove(&path)?;
                }
                fs::create_dir_all(&path)?;
            }
            if checked_metadata(&target)?.is_some_and(|m| m.is_dir() != item.directory) {
                remove(&target)?;
            }
            if item.directory {
                fs::create_dir_all(&target)?;
            } else {
                let metadata = followed_source_metadata(&item.source)?;
                let mut source = File::open(&item.source)?;
                let mut output = File::create(&target)?;
                let mut tracker = Tracker::new(control, "正在添加文件", metadata.len());
                tracker.start_file(&item.relative.to_string_lossy())?;
                copy_stream(&mut source, &mut output, metadata.len(), control, |bytes| {
                    tracker.advance(bytes)
                })?;
                tracker.finish()?;
                let fresh = followed_source_metadata(&item.source)?;
                if fresh.len() != metadata.len() || fresh.modified()? != metadata.modified()? {
                    bail!("源文件在添加过程中发生变化：{}", item.source.display());
                }
                set_file_modified(&output, metadata.modified().ok())?;
            }
        }
        for item in incoming
            .iter()
            .rev()
            .filter(|item| item.directory && item.special.is_none())
        {
            set_directory_modified(&data.join(&item.relative), item.metadata.modified().ok())?;
        }
    }
    let incoming_names = incoming
        .iter()
        .map(|item| collision_key(&item.relative))
        .collect::<BTreeSet<_>>();
    for entry in info.entries.iter().rev().filter(|entry| entry.is_directory) {
        let relative = safe_relative_path(&entry.name)?;
        let path = data.join(&relative);
        if path.is_dir() && !incoming_names.contains(&collision_key(&relative)) {
            set_directory_modified(&path, entry.modified)?;
        }
    }
    if !special_entries.is_empty() {
        special::write(
            &data.join(special::MANIFEST_NAME),
            &special::Manifest::new(special_entries),
        )?;
    }
    let destination = work.path().join(format!("updated.{}", format.extension()));
    let sources = fs::read_dir(&data)?
        .map(|e| e.map(|e| e.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    create_updated_archive(
        &CreateOptions {
            sources,
            destination: destination.clone(),
            format,
            password: if encrypted {
                password.map(str::to_owned)
            } else {
                None
            },
            overwrite: false,
        },
        control,
    )?;
    let changed = inspect_archive(&destination, password, control)?;
    let changed_names = changed
        .entries
        .iter()
        .map(|entry| safe_relative_path(&entry.name).map(|path| collision_key(&path)))
        .collect::<Result<BTreeSet<_>>>()?;
    retained.retain(|index| {
        !changed_names.contains(&collision_key(Path::new(&info.entries[*index].name)))
    });
    let mut published = tempfile::Builder::new()
        .suffix(&format!(".{}", format.extension()))
        .tempfile_in(work.path())?;
    super::update::write_updated(
        info,
        &destination,
        &retained,
        sevenzip.as_ref(),
        if encrypted { password } else { None },
        &mut published,
        control,
    )?;
    let mut updated = inspect_archive(published.path(), password, control)?;
    let expected = retained
        .iter()
        .map(|index| &info.entries[*index])
        .chain(changed.entries.iter())
        .map(|entry| {
            (
                entry.name.as_str(),
                (entry.size, entry.is_directory, entry.modified),
            )
        })
        .collect::<BTreeMap<_, _>>();
    if updated.entries.len() != expected.len()
        || updated.entries.iter().any(|entry| {
            expected.get(entry.name.as_str())
                != Some(&(entry.size, entry.is_directory, entry.modified))
        })
    {
        bail!("更新后的压缩包条目不完整，原压缩包未替换");
    }
    updated.modified = published.as_file().metadata()?.modified()?;
    unchanged(info, password, control)?;
    let mut tracker = Tracker::new(control, "正在保存更新的压缩包", 0);
    tracker.start_file(&info.path.display().to_string())?;
    tracker.finish()?;
    control.check_cancelled()?;
    super::common::persist_temp(published, &info.path, true)?;
    updated.path = info.path.clone();
    Ok(updated)
}

pub struct ExportedEntries {
    pub paths: Vec<PathBuf>,
    pub directory: TempDir,
}

pub fn export_entries(
    info: &ArchiveInfo,
    paths: &[String],
    password: Option<&str>,
    control: &JobControl,
) -> Result<ExportedEntries> {
    let selected = archive_selection(info, paths)?;
    let directory = tempfile::Builder::new()
        .prefix("hx6zip-export-")
        .tempdir()?;
    extract_archive_for_export(
        info,
        &ExtractOptions {
            destination: directory.path().to_path_buf(),
            password: password.map(str::to_owned),
            selected: Some(selected),
            overwrite: false,
        },
        control,
    )?;
    let skipped = info
        .entries
        .iter()
        .filter(|entry| info.skips_iso_symlink(entry))
        .map(|entry| collision_key(Path::new(&entry.name)))
        .collect::<BTreeSet<_>>();
    let paths = paths
        .iter()
        .map(|name| safe_relative_path(name))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .filter(|path| !skipped.contains(&collision_key(path)))
        .map(|path| directory.path().join(path))
        .collect();
    Ok(ExportedEntries { paths, directory })
}
