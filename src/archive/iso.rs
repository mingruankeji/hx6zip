use std::{
    collections::{HashSet, VecDeque},
    fs::{File, Metadata},
    io::{Read, Seek, SeekFrom},
    path::Path,
    time::SystemTime,
};

use anyhow::{Context, Result, ensure};
use chrono::{FixedOffset, NaiveDate, TimeZone};
use iso9660_core::{DirRecord, rock_ridge};

use super::{ArchiveEntry, ArchiveInfo, ArchiveKind, JobControl, MAX_ARCHIVE_ENTRIES};

const SECTOR_SIZE: u64 = 2048;
const MAX_DIRECTORY_SIZE: u64 = 64 * 1024 * 1024;
const MAX_DIRECTORY_DEPTH: usize = 128;
const MAX_SUSP_BYTES: usize = 1024 * 1024;
const MAX_SUSP_CONTINUATIONS: usize = 16;

#[derive(Clone, Debug)]
pub(super) struct IsoExtent {
    pub offset: u64,
    pub length: u64,
}

pub(super) struct IsoLayout {
    pub info: ArchiveInfo,
    pub extents: Vec<Vec<IsoExtent>>,
}

struct Directory {
    lba: u32,
    size: u32,
    prefix: String,
    depth: usize,
}

struct PendingEntry {
    entry: ArchiveEntry,
    extents: Vec<IsoExtent>,
    name_bytes: Vec<u8>,
}

pub(super) fn read_iso(
    path: &Path,
    metadata: &Metadata,
    file: &mut File,
    control: &JobControl,
) -> Result<IsoLayout> {
    let image_size = metadata.len();
    let mut primary = None;
    let mut joliet = None;
    let mut terminated = false;
    for sector in 16..144 {
        control.check_cancelled()?;
        let data = read_sector(file, sector, image_size)?;
        ensure!(&data[1..6] == b"CD001" && data[6] == 1, "ISO 卷描述符无效");
        match data[0] {
            1 | 2 if data[0] == 1 || matches!(&data[88..91], b"%/@" | b"%/B" | b"%/C" | b"%/E") => {
                let block_size = both_u16(&data[128..132])?;
                ensure!(
                    u64::from(block_size) == SECTOR_SIZE,
                    "不支持此 ISO 逻辑块大小"
                );
                let volume_sectors = both_u32(&data[80..88])?;
                let volume_size = u64::from(volume_sectors)
                    .checked_mul(SECTOR_SIZE)
                    .context("ISO 卷大小溢出")?;
                ensure!(volume_size <= image_size, "ISO 声明的卷大小超出镜像");
                let root = parse_record(&data, 156)?;
                ensure!(root.is_dir() && root.size > 0, "ISO 根目录无效");
                if data[0] == 1 {
                    primary = Some((root, volume_size));
                } else {
                    joliet = Some((root, volume_size));
                }
            }
            255 => {
                terminated = true;
                break;
            }
            _ => {}
        }
    }
    ensure!(terminated, "ISO 卷描述符未结束");
    let (primary, primary_size) = primary.context("ISO 缺少主卷描述符")?;
    let root_sector = read_sector(file, u64::from(primary.lba), primary_size)?;
    let root_dot = parse_record(&root_sector, 0)?;
    ensure!(
        root_dot.name_bytes == [0]
            && root_dot.is_dir()
            && root_dot.lba == primary.lba
            && root_dot.size == primary.size,
        "ISO 根目录记录无效"
    );
    let has_rock_ridge = rock_ridge::has_sp_entry(&root_dot.system_use);
    let susp_skip = if has_rock_ridge {
        rock_ridge::sp_skip(&root_dot.system_use)
    } else {
        0
    };
    let (root, image_size, use_joliet) = if has_rock_ridge {
        (primary, primary_size, false)
    } else if let Some((joliet, joliet_size)) = joliet {
        (joliet, joliet_size, true)
    } else {
        (primary, primary_size, false)
    };

    let mut entries = Vec::new();
    let mut extents = Vec::new();
    let mut total_size = 0u64;
    let mut records_seen = 0usize;
    let mut visited = HashSet::new();
    let mut directories = VecDeque::from([Directory {
        lba: root.lba,
        size: root.size,
        prefix: String::new(),
        depth: 0,
    }]);
    while let Some(directory) = directories.pop_front() {
        control.check_cancelled()?;
        ensure!(directory.depth <= MAX_DIRECTORY_DEPTH, "ISO 文件夹嵌套过深");
        ensure!(
            visited.insert(directory.lba),
            "ISO 包含循环或重复引用的文件夹"
        );
        ensure!(
            u64::from(directory.size) <= MAX_DIRECTORY_SIZE,
            "ISO 文件夹大小超过安全限制"
        );
        checked_extent(directory.lba, directory.size, image_size)?;
        let mut pending: Option<PendingEntry> = None;
        let sectors = u64::from(directory.size).div_ceil(SECTOR_SIZE);
        for index in 0..sectors {
            control.check_cancelled()?;
            let data = read_sector(file, u64::from(directory.lba) + index, image_size)?;
            let remaining = u64::from(directory.size) - index * SECTOR_SIZE;
            let limit = remaining.min(SECTOR_SIZE) as usize;
            let mut offset = 0;
            while offset < limit && data[offset] != 0 {
                control.check_cancelled()?;
                let length = usize::from(data[offset]);
                ensure!(
                    length >= 34 && offset + length <= limit,
                    "ISO 文件夹记录超出扇区"
                );
                let record = parse_record(&data, offset)?;
                offset += length;
                if record.is_dot() {
                    continue;
                }
                records_seen += 1;
                ensure!(records_seen <= MAX_ARCHIVE_ENTRIES, "ISO 条目超过安全限制");
                let system_use = if has_rock_ridge {
                    read_susp(file, &record.system_use, susp_skip, image_size, control)?
                } else {
                    Vec::new()
                };
                let (mode, has_link) = rock_ridge_kind(&system_use)?;
                if has_rock_ridge && rock_ridge::is_relocated(&system_use) {
                    ensure!(pending.is_none(), "ISO 多区段文件不完整");
                    continue;
                }
                let mut is_directory = record.is_dir();
                let mut directory_lba = record.lba;
                let mut directory_size = record.size;
                if let Some(lba) = rock_ridge::child_link(&system_use) {
                    let sector = read_sector(file, u64::from(lba), image_size)?;
                    let dot = parse_record(&sector, 0)?;
                    ensure!(
                        dot.name_bytes == [0] && dot.is_dir() && dot.lba == lba && dot.size > 0,
                        "ISO Rock Ridge 文件夹链接无效"
                    );
                    is_directory = true;
                    directory_lba = lba;
                    directory_size = dot.size;
                }
                let mut name = if use_joliet {
                    joliet_name(&record.name_bytes)?
                } else {
                    primary_name(&record.name_bytes)?
                };
                if has_rock_ridge {
                    if let Some(alternate) = rock_ridge_name(&system_use)? {
                        name = alternate;
                    }
                }
                ensure!(
                    !name.is_empty() && !name.contains(['/', '\\', '\0']),
                    "ISO 文件名无效：{name}"
                );
                let full_name = if directory.prefix.is_empty() {
                    name
                } else {
                    format!("{}/{}", directory.prefix, name)
                };
                let unsupported_kind = has_link
                    || mode.is_some_and(|kind| {
                        kind != 0x8000 && kind != 0x4000 || (kind == 0x4000) != is_directory
                    });
                let more = record.is_multi_extent();
                ensure!(
                    !is_directory || !more,
                    "不支持多区段 ISO 文件夹：{full_name}"
                );
                let extent = if is_directory {
                    checked_extent(directory_lba, directory_size, image_size)?;
                    Vec::new()
                } else {
                    vec![checked_extent(record.lba, record.size, image_size)?]
                };
                if let Some(mut item) = pending.take() {
                    ensure!(
                        !is_directory
                            && item.name_bytes == record.name_bytes
                            && item.entry.name == full_name
                            && item.entry.unsupported_kind == unsupported_kind,
                        "ISO 多区段文件不完整或名称不一致"
                    );
                    item.entry.size = item
                        .entry
                        .size
                        .checked_add(u64::from(record.size))
                        .context("ISO 文件大小溢出")?;
                    item.extents.extend(extent);
                    if more {
                        pending = Some(item);
                        continue;
                    }
                    push_entry(item, &mut entries, &mut extents, &mut total_size)?;
                    continue;
                }
                let item = PendingEntry {
                    entry: ArchiveEntry {
                        index: entries.len(),
                        name: full_name.clone(),
                        size: if is_directory {
                            0
                        } else {
                            u64::from(record.size)
                        },
                        compressed_size: None,
                        is_directory,
                        is_encrypted: Some(false),
                        unsupported_kind,
                        is_symbolic_link: !is_directory
                            && (has_link || mode == Some(0xa000))
                            && mode.is_none_or(|kind| kind == 0xa000),
                        modified: recorded_time(record.recorded.as_ref())?,
                    },
                    extents: extent,
                    name_bytes: record.name_bytes,
                };
                if more {
                    pending = Some(item);
                } else {
                    if is_directory && !unsupported_kind {
                        directories.push_back(Directory {
                            lba: directory_lba,
                            size: directory_size,
                            prefix: full_name,
                            depth: directory.depth + 1,
                        });
                    }
                    push_entry(item, &mut entries, &mut extents, &mut total_size)?;
                }
            }
        }
        ensure!(pending.is_none(), "ISO 多区段文件不完整");
    }

    Ok(IsoLayout {
        info: ArchiveInfo {
            path: path.to_path_buf(),
            format: ArchiveKind::Iso,
            entries,
            total_size,
            archive_size: metadata.len(),
            modified: metadata.modified().context("无法读取压缩包修改时间")?,
        },
        extents,
    })
}

fn push_entry(
    mut item: PendingEntry,
    entries: &mut Vec<ArchiveEntry>,
    extents: &mut Vec<Vec<IsoExtent>>,
    total_size: &mut u64,
) -> Result<()> {
    ensure!(entries.len() < MAX_ARCHIVE_ENTRIES, "ISO 条目超过安全限制");
    *total_size = total_size
        .checked_add(item.entry.size)
        .context("ISO 文件大小溢出")?;
    item.entry.index = entries.len();
    entries.push(item.entry);
    extents.push(item.extents);
    Ok(())
}

fn read_sector(file: &mut File, sector: u64, image_size: u64) -> Result<[u8; 2048]> {
    let offset = sector
        .checked_mul(SECTOR_SIZE)
        .context("ISO 扇区位置溢出")?;
    ensure!(
        offset
            .checked_add(SECTOR_SIZE)
            .is_some_and(|end| end <= image_size),
        "ISO 扇区超出镜像"
    );
    file.seek(SeekFrom::Start(offset))
        .context("无法定位 ISO 扇区")?;
    let mut data = [0; 2048];
    file.read_exact(&mut data).context("无法读取 ISO 扇区")?;
    Ok(data)
}

fn checked_extent(lba: u32, length: u32, image_size: u64) -> Result<IsoExtent> {
    let offset = u64::from(lba) * SECTOR_SIZE;
    ensure!(
        offset
            .checked_add(u64::from(length))
            .is_some_and(|end| end <= image_size),
        "ISO 文件数据超出镜像"
    );
    Ok(IsoExtent {
        offset,
        length: u64::from(length),
    })
}

fn both_u16(data: &[u8]) -> Result<u16> {
    let little = u16::from_le_bytes([data[0], data[1]]);
    ensure!(
        little == u16::from_be_bytes([data[2], data[3]]),
        "ISO 字段不一致"
    );
    Ok(little)
}

fn both_u32(data: &[u8]) -> Result<u32> {
    let little = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    ensure!(
        little == u32::from_be_bytes([data[4], data[5], data[6], data[7]]),
        "ISO 字段不一致"
    );
    Ok(little)
}

fn parse_record(data: &[u8], offset: usize) -> Result<DirRecord> {
    let (record, length) = DirRecord::parse(data, offset)
        .context("无法读取 ISO 文件夹记录")?
        .context("ISO 文件夹记录为空")?;
    ensure!(length >= 34, "ISO 文件夹记录过短");
    let raw = &data[offset..offset + length];
    both_u32(&raw[2..10])?;
    both_u32(&raw[10..18])?;
    ensure!(raw[1] == 0, "暂不支持 ISO 扩展属性记录");
    ensure!(raw[26] == 0 && raw[27] == 0, "暂不支持交错存储的 ISO 文件");
    ensure!(both_u16(&raw[28..32])? == 1, "暂不支持多卷 ISO 镜像");
    Ok(record)
}

fn primary_name(bytes: &[u8]) -> Result<String> {
    let name = std::str::from_utf8(bytes).context("ISO 文件名编码无效")?;
    Ok(strip_version(name).trim_end_matches('.').to_owned())
}

fn joliet_name(bytes: &[u8]) -> Result<String> {
    ensure!(bytes.len() % 2 == 0, "ISO Joliet 文件名长度无效");
    let units = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
        .collect::<Vec<_>>();
    let name = String::from_utf16(&units).context("ISO Joliet 文件名编码无效")?;
    Ok(strip_version(&name).trim_end_matches('.').to_owned())
}

fn strip_version(name: &str) -> &str {
    match name.rsplit_once(';') {
        Some((base, version))
            if !version.is_empty() && version.bytes().all(|b| b.is_ascii_digit()) =>
        {
            base
        }
        _ => name,
    }
}

fn rock_ridge_name(system_use: &[u8]) -> Result<Option<String>> {
    let mut offset = 0;
    let mut name = Vec::new();
    let mut continuation = false;
    let mut found = false;
    while offset < system_use.len() {
        let length = usize::from(system_use[offset + 2]);
        let entry = &system_use[offset..offset + length];
        if &entry[..2] == b"NM" {
            ensure!(length >= 5, "ISO Rock Ridge 文件名记录无效");
            let flags = entry[4];
            ensure!(flags & !1 == 0, "不支持此 ISO Rock Ridge 文件名");
            ensure!(!found || continuation, "ISO Rock Ridge 文件名记录重复");
            name.extend_from_slice(&entry[5..]);
            ensure!(name.len() <= 4096, "ISO 文件名过长");
            continuation = flags & 1 != 0;
            found = true;
        }
        offset += length;
    }
    ensure!(!continuation, "ISO Rock Ridge 文件名不完整");
    if found {
        Ok(Some(
            String::from_utf8(name).context("ISO Rock Ridge 文件名编码无效")?,
        ))
    } else {
        Ok(None)
    }
}

fn rock_ridge_kind(system_use: &[u8]) -> Result<(Option<u32>, bool)> {
    let mut offset = 0;
    let mut mode = None;
    let mut has_link = false;
    while offset < system_use.len() {
        let length = usize::from(system_use[offset + 2]);
        let entry = &system_use[offset..offset + length];
        match &entry[..2] {
            b"SL" => has_link = true,
            b"PX" => {
                ensure!(
                    length >= 36 && mode.is_none(),
                    "ISO Rock Ridge 文件属性无效"
                );
                mode = Some(both_u32(&entry[4..12])? & 0xf000);
                both_u32(&entry[12..20])?;
                both_u32(&entry[20..28])?;
                both_u32(&entry[28..36])?;
            }
            b"CL" | b"PL" => {
                ensure!(length >= 12, "ISO Rock Ridge 文件夹链接无效");
                both_u32(&entry[4..12])?;
            }
            _ => {}
        }
        offset += length;
    }
    Ok((mode, has_link))
}

fn read_susp(
    file: &mut File,
    system_use: &[u8],
    skip: usize,
    image_size: u64,
    control: &JobControl,
) -> Result<Vec<u8>> {
    if system_use.is_empty() {
        return Ok(Vec::new());
    }
    ensure!(skip <= system_use.len(), "ISO Rock Ridge 元数据无效");
    let mut result = Vec::new();
    let mut seen = HashSet::new();
    expand_susp(
        file,
        &system_use[skip..],
        image_size,
        control,
        &mut seen,
        &mut result,
    )?;
    Ok(result)
}

fn expand_susp(
    file: &mut File,
    data: &[u8],
    image_size: u64,
    control: &JobControl,
    seen: &mut HashSet<(u32, u32, u32)>,
    output: &mut Vec<u8>,
) -> Result<()> {
    let mut offset = 0;
    while offset < data.len() {
        control.check_cancelled()?;
        if data[offset..].iter().all(|byte| *byte == 0) {
            break;
        }
        ensure!(data.len() - offset >= 4, "ISO Rock Ridge 元数据不完整");
        let length = usize::from(data[offset + 2]);
        ensure!(
            length >= 4 && offset + length <= data.len(),
            "ISO Rock Ridge 元数据长度无效"
        );
        let entry = &data[offset..offset + length];
        ensure!(entry[3] == 1, "不支持此 ISO Rock Ridge 元数据版本");
        if &entry[..2] == b"ST" {
            break;
        }
        if &entry[..2] == b"CE" {
            ensure!(length >= 28, "ISO Rock Ridge 延续记录无效");
            let lba = both_u32(&entry[4..12])?;
            let relative = both_u32(&entry[12..20])?;
            let size = both_u32(&entry[20..28])?;
            ensure!(
                seen.len() < MAX_SUSP_CONTINUATIONS && seen.insert((lba, relative, size)),
                "ISO Rock Ridge 延续记录过多或循环"
            );
            let start = u64::from(lba)
                .checked_mul(SECTOR_SIZE)
                .and_then(|value| value.checked_add(u64::from(relative)))
                .context("ISO Rock Ridge 元数据位置溢出")?;
            ensure!(
                usize::try_from(size).is_ok_and(|size| size <= MAX_SUSP_BYTES)
                    && start
                        .checked_add(u64::from(size))
                        .is_some_and(|end| end <= image_size),
                "ISO Rock Ridge 元数据超出安全限制"
            );
            let mut continuation = vec![0; size as usize];
            file.seek(SeekFrom::Start(start))
                .context("无法定位 ISO 元数据")?;
            file.read_exact(&mut continuation)
                .context("无法读取 ISO 元数据")?;
            expand_susp(file, &continuation, image_size, control, seen, output)?;
        } else {
            ensure!(
                output.len() + length <= MAX_SUSP_BYTES,
                "ISO Rock Ridge 元数据过大"
            );
            output.extend_from_slice(entry);
        }
        offset += length;
    }
    Ok(())
}

fn recorded_time(value: Option<&iso9660_core::IsoDateTime>) -> Result<Option<SystemTime>> {
    value
        .map(|value| {
            let offset = FixedOffset::east_opt(i32::from(value.tz_offset_15min) * 15 * 60)
                .context("ISO 文件时间的时区无效")?;
            let date = NaiveDate::from_ymd_opt(
                i32::from(value.year),
                u32::from(value.month),
                u32::from(value.day),
            )
            .and_then(|date| {
                date.and_hms_opt(
                    u32::from(value.hour),
                    u32::from(value.minute),
                    u32::from(value.second),
                )
            })
            .context("ISO 文件时间无效")?;
            let time = offset
                .from_local_datetime(&date)
                .single()
                .context("ISO 文件时间无效")?;
            Ok(time.into())
        })
        .transpose()
}
