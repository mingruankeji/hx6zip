use std::{
    collections::{BTreeSet, HashMap},
    fs::File,
    io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::Path,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use sevenz_rust2::{Archive, ArchiveWriter, Password};
use tempfile::NamedTempFile;
use zip::{ZipArchive, ZipWriter};

use super::{ArchiveInfo, ArchiveKind, IO_BUFFER_SIZE, IntegrityMode, JobControl, Progress};

pub(super) struct SevenZipUpdate {
    archive: Archive,
    blocks: Vec<usize>,
    pub repack: BTreeSet<usize>,
}

pub(super) fn verify_data_password(
    path: &Path,
    archive: &Archive,
    password: &str,
    control: &JobControl,
) -> Result<()> {
    let block = archive
        .blocks
        .iter()
        .position(|block| {
            block.coders.iter().any(|coder| {
                coder.encoder_method_id() == sevenz_rust2::EncoderMethod::ID_AES256_SHA256
            })
        })
        .context("缺少加密数据块")?;
    let mut input = BufReader::new(File::open(path)?);
    let password = Password::from(password);
    let decoder = sevenz_rust2::BlockDecoder::new(1, block, archive, &password, &mut input);
    let mut verified = false;
    control.report(Progress {
        operation: "正在校验加密数据密码",
        current_file: path.display().to_string(),
        completed_bytes: 0,
        total_bytes: 0,
        archive_size: None,
    });
    let result = decoder.for_each_entries(&mut |entry, input| {
        super::common::copy_stream(input, &mut io::sink(), entry.size, control, |_| Ok(()))
            .map_err(|error| sevenz_rust2::Error::Other(error.to_string().into()))?;
        if entry.has_stream && entry.has_crc {
            verified = true;
        }
        Ok(!verified)
    });
    control.check_cancelled()?;
    result.context("7z 密码不正确或加密数据损坏")?;
    if !verified {
        bail!("加密数据缺少条目校验码，无法确认更新密码");
    }
    Ok(())
}

pub(super) fn plan_sevenzip(
    info: &ArchiveInfo,
    retained: &BTreeSet<usize>,
    password: Option<&str>,
    control: &JobControl,
) -> Result<SevenZipUpdate> {
    let mut input = BufReader::new(File::open(&info.path)?);
    let archive = Archive::read(&mut input, &Password::from(password.unwrap_or_default()))?;
    let mut members = vec![Vec::new(); archive.blocks.len()];
    for (index, entry) in archive.files.iter().enumerate() {
        if entry.has_stream {
            let block = archive.stream_map.file_block_index[index].context("7z 条目缺少数据块")?;
            members[block].push(index);
        }
    }
    let mut blocks = Vec::new();
    let mut repack = BTreeSet::new();
    for (block, entries) in members.iter().enumerate() {
        control.check_cancelled()?;
        let kept = entries
            .iter()
            .filter(|index| retained.contains(index))
            .copied()
            .collect::<Vec<_>>();
        if kept.len() == entries.len()
            && !kept.is_empty()
            && entries.iter().all(|index| archive.files[*index].has_crc)
        {
            blocks.push(block);
        } else {
            repack.extend(kept);
        }
    }
    Ok(SevenZipUpdate {
        archive,
        blocks,
        repack,
    })
}

struct CopyInput<'a> {
    input: BufReader<File>,
    control: &'a JobControl,
    progress: Progress,
    last_report: Instant,
}

impl<'a> CopyInput<'a> {
    fn open(path: &Path, control: &'a JobControl) -> Result<Self> {
        let file = File::open(path)?;
        let total = file.metadata()?.len();
        let progress = Progress {
            operation: "正在复制未变更的压缩数据",
            current_file: path.display().to_string(),
            completed_bytes: 0,
            total_bytes: total,
            archive_size: None,
        };
        control.report(progress.clone());
        Ok(Self {
            input: BufReader::with_capacity(IO_BUFFER_SIZE, file),
            control,
            progress,
            last_report: Instant::now(),
        })
    }
}

impl Read for CopyInput<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.control.check_cancelled().map_err(io::Error::other)?;
        let count = self.input.read(buffer)?;
        self.progress.completed_bytes = self
            .progress
            .completed_bytes
            .saturating_add(count as u64)
            .min(self.progress.total_bytes);
        if self.last_report.elapsed() >= Duration::from_millis(100) {
            self.control.report(self.progress.clone());
            self.last_report = Instant::now();
        }
        Ok(count)
    }
}

impl Seek for CopyInput<'_> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.control.check_cancelled().map_err(io::Error::other)?;
        self.input.seek(position)
    }
}

pub(super) fn write_updated(
    info: &ArchiveInfo,
    delta: &Path,
    retained: &BTreeSet<usize>,
    sevenzip: Option<&SevenZipUpdate>,
    password: Option<&str>,
    output: &mut NamedTempFile,
    control: &JobControl,
) -> Result<()> {
    let result = match info.format {
        ArchiveKind::Zip => write_zip(info, delta, retained, output, control),
        ArchiveKind::SevenZip => write_sevenzip(
            info,
            delta,
            retained,
            sevenzip.context("缺少 7z 更新计划")?,
            password,
            output,
            control,
        ),
        _ => bail!("此格式不支持更新"),
    };
    control.check_cancelled()?;
    result?;
    output
        .as_file_mut()
        .sync_all()
        .context("无法保存更新的压缩包")?;
    if control.integrity() == IntegrityMode::Safe {
        control.report(Progress {
            operation: "正在校验压缩数据副本",
            current_file: info.path.display().to_string(),
            completed_bytes: 0,
            total_bytes: output.as_file().metadata()?.len(),
            archive_size: None,
        });
        match info.format {
            ArchiveKind::Zip => verify_zip(info, delta, retained, output.path(), control)?,
            ArchiveKind::SevenZip => verify_sevenzip(
                info,
                delta,
                sevenzip.context("缺少 7z 更新计划")?,
                password,
                output.path(),
                control,
            )?,
            _ => unreachable!(),
        }
    }
    Ok(())
}

fn write_zip(
    info: &ArchiveInfo,
    delta: &Path,
    retained: &BTreeSet<usize>,
    output: &mut NamedTempFile,
    control: &JobControl,
) -> Result<()> {
    let old = ZipArchive::new(CopyInput::open(&info.path, control)?)?;
    let changed = ZipArchive::new(CopyInput::open(delta, control)?)?;
    let mut writer = ZipWriter::new(BufWriter::new(output.as_file_mut()));
    writer.set_raw_comment(old.comment().into())?;
    writer.merge_selected_archive(old, &retained.iter().copied().collect::<Vec<_>>())?;
    writer.merge_archive(changed)?;
    writer.finish()?.flush()?;
    Ok(())
}

fn write_sevenzip(
    info: &ArchiveInfo,
    delta: &Path,
    retained: &BTreeSet<usize>,
    plan: &SevenZipUpdate,
    password: Option<&str>,
    output: &mut NamedTempFile,
    control: &JobControl,
) -> Result<()> {
    let mut old = CopyInput::open(&info.path, control)?;
    let mut changed = CopyInput::open(delta, control)?;
    let changed_archive =
        Archive::read(&mut changed, &Password::from(password.unwrap_or_default()))?;
    let mut writer = ArchiveWriter::new(BufWriter::new(output.as_file_mut()))?;
    for &index in retained {
        let entry = &plan.archive.files[index];
        if !entry.has_stream {
            writer.push_archive_entry(entry.clone(), None::<io::Empty>)?;
        }
    }
    for &block in &plan.blocks {
        control.check_cancelled()?;
        writer.push_raw_block(&plan.archive, block, &mut old, |entry| entry.name.clone())?;
    }
    for entry in &changed_archive.files {
        if !entry.has_stream {
            writer.push_archive_entry(entry.clone(), None::<io::Empty>)?;
        }
    }
    for block in 0..changed_archive.blocks.len() {
        control.check_cancelled()?;
        writer.push_raw_block(&changed_archive, block, &mut changed, |entry| {
            entry.name.clone()
        })?;
    }
    super::create::configure_sevenzip_header(&mut writer, password);
    writer.finish()?.flush()?;
    Ok(())
}

fn equal_bytes(left: &mut dyn Read, right: &mut dyn Read, control: &JobControl) -> Result<()> {
    let mut a = [0; IO_BUFFER_SIZE];
    let mut b = [0; IO_BUFFER_SIZE];
    loop {
        control.check_cancelled()?;
        let count = left.read(&mut a)?;
        if count == 0 {
            if right.read(&mut b[..1])? != 0 {
                bail!("复制后的压缩数据长度不一致");
            }
            return Ok(());
        }
        right
            .read_exact(&mut b[..count])
            .context("复制后的压缩数据被截断")?;
        if a[..count] != b[..count] {
            bail!("复制后的压缩数据与原始数据不一致");
        }
    }
}

fn verify_zip(
    info: &ArchiveInfo,
    delta: &Path,
    retained: &BTreeSet<usize>,
    output: &Path,
    control: &JobControl,
) -> Result<()> {
    let mut result = ZipArchive::new(BufReader::new(File::open(output)?))?;
    let indices = result
        .file_names()
        .enumerate()
        .map(|(i, name)| (name.to_owned(), i))
        .collect::<HashMap<_, _>>();
    for (path, selected) in [(&info.path as &Path, Some(retained)), (delta, None)] {
        let mut source = ZipArchive::new(BufReader::new(File::open(path)?))?;
        for index in 0..source.len() {
            if selected.is_some_and(|selected| !selected.contains(&index)) {
                continue;
            }
            let mut original = source.by_index_raw(index)?;
            let target = *indices
                .get(original.name())
                .context("更新后的 ZIP 缺少条目")?;
            let mut copied = result.by_index_raw(target)?;
            equal_bytes(&mut original, &mut copied, control)?;
        }
    }
    Ok(())
}

fn block_streams(archive: &Archive, block: usize) -> Result<std::ops::Range<usize>> {
    let starts = archive.stream_map.block_first_pack_stream_index();
    let start = *starts.get(block).context("7z 数据块索引越界")?;
    let end = starts
        .get(block + 1)
        .copied()
        .unwrap_or(archive.pack_sizes().len());
    if start > end || end > archive.pack_sizes().len() {
        bail!("7z 压缩流索引无效");
    }
    Ok(start..end)
}

fn pack_offset(archive: &Archive, index: usize) -> Result<u64> {
    let stream_offset = *archive
        .stream_map
        .pack_stream_offsets()
        .get(index)
        .context("7z 压缩流偏移索引越界")?;
    32u64
        .checked_add(archive.pack_pos())
        .and_then(|offset| offset.checked_add(stream_offset))
        .context("7z 压缩流偏移溢出")
}

fn verify_sevenzip(
    info: &ArchiveInfo,
    delta: &Path,
    plan: &SevenZipUpdate,
    password: Option<&str>,
    output: &Path,
    control: &JobControl,
) -> Result<()> {
    let password = Password::from(password.unwrap_or_default());
    let mut result_file = File::open(output)?;
    let result = Archive::read(&mut result_file, &password)?;
    let mut output_block = 0;
    for (path, old) in [(&info.path as &Path, true), (delta, false)] {
        let mut source_file = File::open(path)?;
        let changed;
        let (archive, blocks) = if old {
            (&plan.archive, plan.blocks.clone())
        } else {
            changed = Archive::read(&mut source_file, &password)?;
            let blocks = (0..changed.blocks.len()).collect();
            (&changed, blocks)
        };
        for block in blocks {
            let streams = block_streams(archive, block)?;
            let targets = block_streams(&result, output_block)?;
            if streams.len() != targets.len() {
                bail!("更新后的 7z 数据块结构不一致");
            }
            for (index, target) in streams.zip(targets) {
                let size = archive.pack_sizes()[index];
                if size != result.pack_sizes()[target] {
                    bail!("7z 压缩数据大小不一致");
                }
                source_file.seek(SeekFrom::Start(pack_offset(archive, index)?))?;
                result_file.seek(SeekFrom::Start(pack_offset(&result, target)?))?;
                equal_bytes(
                    &mut (&mut source_file).take(size),
                    &mut (&mut result_file).take(size),
                    control,
                )?;
            }
            output_block += 1;
        }
    }
    if output_block != result.blocks.len() {
        bail!("更新后的 7z 数据块数量不一致");
    }
    Ok(())
}
