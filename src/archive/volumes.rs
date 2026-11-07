use std::{
    fs::{self, File, Metadata},
    io::Read,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use tempfile::NamedTempFile;

use super::{
    JobControl,
    common::{Tracker, checked_metadata, copy_stream, ensure_root, persist_temp},
};

fn source_metadata(path: &Path) -> Result<Metadata> {
    let metadata =
        checked_metadata(path)?.with_context(|| format!("文件不存在：{}", path.display()))?;
    ensure!(metadata.is_file(), "不是普通文件：{}", path.display());
    Ok(metadata)
}

fn verify_source(path: &Path, before: &Metadata) -> Result<()> {
    let after = source_metadata(path)?;
    ensure!(
        before.len() == after.len() && before.modified()? == after.modified()?,
        "源文件在处理时发生变化：{}",
        path.display()
    );
    Ok(())
}

pub fn split_file(
    source: &Path,
    destination: &Path,
    volume_bytes: u64,
    control: &JobControl,
) -> Result<Vec<PathBuf>> {
    ensure!(volume_bytes > 0, "分卷大小必须大于零");
    control.check_cancelled()?;
    let metadata = source_metadata(source)?;
    let count = metadata.len().div_ceil(volume_bytes).max(1);
    ensure!(count <= 100_000, "分片数量超过 100,000，请增大分卷大小");
    let name = source.file_name().context("源文件没有文件名")?;
    let root = ensure_root(destination)?;
    let targets: Vec<_> = (1..=count)
        .map(|index| {
            let mut name = name.to_os_string();
            name.push(format!(".{index:03}"));
            root.join(name)
        })
        .collect();
    for target in &targets {
        ensure!(
            checked_metadata(target)?.is_none(),
            "目标文件已存在：{}",
            target.display()
        );
    }
    // An older longer group would otherwise leave trailing parts that look mergeable.
    for entry in fs::read_dir(&root).context("无法检查目标文件夹")? {
        let path = entry.context("无法读取目标文件夹")?.path();
        if path.file_stem() == Some(name)
            && path
                .extension()
                .and_then(|value| value.to_str())
                .is_some_and(|suffix| {
                    suffix.len() >= 3 && suffix.bytes().all(|byte| byte.is_ascii_digit())
                })
        {
            anyhow::bail!("目标文件夹已有同名分片：{}", path.display());
        }
    }
    let mut input = File::open(source).context("无法打开源文件")?;
    let mut tracker = Tracker::new(control, "分割文件", metadata.len());
    let mut remaining = metadata.len();
    for target in &targets {
        tracker.start_file(&target.file_name().unwrap_or_default().to_string_lossy())?;
        let bytes = remaining.min(volume_bytes);
        let mut temp = NamedTempFile::new_in(&root).context("无法创建分片临时文件")?;
        copy_stream(
            &mut (&mut input).take(bytes),
            &mut temp,
            bytes,
            control,
            |bytes| tracker.advance(bytes),
        )?;
        verify_source(source, &metadata)?;
        control.check_cancelled()?;
        persist_temp(temp, target, false)?;
        remaining -= bytes;
    }
    verify_source(source, &metadata)?;
    tracker.finish()?;
    Ok(targets)
}

pub fn merge_files(paths: &[PathBuf], control: &JobControl) -> Result<PathBuf> {
    let (destination, parts) = hx6zip_shell_protocol::merge_parts(paths)
        .context("请选择同一文件夹内、同名且从 .001 开始的连续分片（至少两个）")?;
    control.check_cancelled()?;
    let metadata: Vec<_> = parts
        .iter()
        .map(|path| source_metadata(path))
        .collect::<Result<_>>()?;
    let total = metadata.iter().try_fold(0u64, |total, item| {
        total.checked_add(item.len()).context("合并文件过大")
    })?;
    let parent = destination.parent().context("无法确定保存位置")?;
    let root = ensure_root(if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    })?;
    let destination = root.join(destination.file_name().context("无法确定合并文件名")?);
    ensure!(
        checked_metadata(&destination)?.is_none(),
        "目标文件已存在，不会覆盖：{}",
        destination.display()
    );
    let mut tracker = Tracker::new(control, "合并文件", total);
    let mut temp = NamedTempFile::new_in(root).context("无法创建合并临时文件")?;
    for (path, metadata) in parts.iter().zip(&metadata) {
        tracker.start_file(&path.file_name().unwrap_or_default().to_string_lossy())?;
        let mut input =
            File::open(path).with_context(|| format!("无法打开分片：{}", path.display()))?;
        copy_stream(&mut input, &mut temp, metadata.len(), control, |bytes| {
            tracker.advance(bytes)
        })?;
        verify_source(path, metadata)?;
    }
    for (path, metadata) in parts.iter().zip(&metadata) {
        verify_source(path, metadata)?;
    }
    tracker.finish()?;
    persist_temp(temp, &destination, false)?;
    Ok(destination)
}
