use std::{
    collections::{BTreeMap, HashMap},
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom},
    path::Path,
    sync::{Arc, Mutex, mpsc},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use zip::{ZipArchive, read::ZipFile};

use super::{
    ArchiveEntry, ArchiveInfo, ArchiveKind, IO_BUFFER_SIZE, IntegrityMode, JobControl, Progress,
    archive_selection,
    common::{Tracker, copy_stream},
    extract::{SharedInput, validate_edit_entries, verify_unchanged},
    inspect::inspect_zip,
};

#[derive(Clone)]
pub struct ZipDragSource(Arc<ZipDragSourceInner>);

struct ZipDragSourceInner {
    archive: ZipArchive<SharedInput>,
    entries: BTreeMap<usize, ArchiveEntry>,
    password: Option<String>,
    control: JobControl,
    progress: Mutex<DragProgress>,
}

struct DragProgress {
    total: u64,
    completed: u64,
    positions: HashMap<usize, u64>,
    last_report: Instant,
}

impl ZipDragSource {
    pub fn new(
        info: &ArchiveInfo,
        paths: &[String],
        password: Option<&str>,
        control: &JobControl,
    ) -> Result<Self> {
        let selected = archive_selection(info, paths)?;
        control.with_archive_password(
            &info.path,
            password,
            info.entries.iter().any(|entry| {
                selected.contains(&entry.index) && entry.is_encrypted == Some(true)
            }),
            true,
            |password| Self::with_password(info, paths, password, control),
        )
    }

    fn with_password(
        info: &ArchiveInfo,
        paths: &[String],
        password: Option<&str>,
        control: &JobControl,
    ) -> Result<Self> {
        ensure!(info.format == ArchiveKind::Zip, "拖放内容不是 ZIP 压缩包");
        control.check_cancelled()?;
        let file = open_archive(&info.path)?;
        let metadata = file.metadata().context("无法读取压缩包信息")?;
        let mut archive = ZipArchive::new(SharedInput::new(file, metadata.len()))
            .context("无法读取 ZIP 压缩包")?;
        let fresh = inspect_zip(&info.path, &metadata, &mut archive, control)?;
        verify_unchanged(info, &fresh)?;
        validate_edit_entries(&fresh)?;
        let selected = archive_selection(&fresh, paths)?;
        let entries = fresh
            .entries
            .into_iter()
            .filter(|entry| selected.contains(&entry.index) && !entry.is_directory)
            .map(|entry| (entry.index, entry))
            .collect::<BTreeMap<_, _>>();
        let total = entries.values().try_fold(0u64, |total, entry| {
            total.checked_add(entry.size).context("所选文件大小溢出")
        })?;

        for entry in entries
            .values()
            .filter(|entry| entry.is_encrypted == Some(true))
        {
            open_entry(&mut archive, entry, password)?;
        }

        if control.integrity() == IntegrityMode::Safe {
            let mut tracker = Tracker::new(control, "正在校验拖放内容", total);
            for entry in entries.values() {
                tracker.start_file(&entry.name)?;
                let mut file = open_entry(&mut archive, entry, password)?;
                copy_stream(&mut file, &mut io::sink(), entry.size, control, |bytes| {
                    tracker.advance(bytes)
                })
                .with_context(|| format!("无法校验 {}", entry.name))?;
            }
            tracker.finish()?;
        }

        Ok(Self(Arc::new(ZipDragSourceInner {
            archive,
            entries,
            password: password.map(str::to_owned),
            control: control.clone(),
            progress: Mutex::new(DragProgress {
                total,
                completed: 0,
                positions: HashMap::new(),
                last_report: Instant::now(),
            }),
        })))
    }

    pub fn open(&self, index: usize) -> Result<ZipDragReader> {
        self.open_at(index, 0)
    }

    fn open_at(&self, index: usize, position: u64) -> Result<ZipDragReader> {
        self.0.control.check_cancelled()?;
        let entry = self
            .0
            .entries
            .get(&index)
            .with_context(|| format!("选中的 ZIP 条目已失效：{index}"))?
            .clone();
        let (sender, receiver) = mpsc::channel();
        let (ready_sender, ready_receiver) = mpsc::sync_channel(0);
        let source = self.clone();
        let worker = thread::Builder::new()
            .name("mzip-zip-drag".to_owned())
            .spawn(move || source.read_requests(&entry, receiver, ready_sender))
            .context("无法启动 ZIP 拖放读取线程")?;
        ready_receiver
            .recv()
            .context("ZIP 拖放读取线程异常退出")??;
        self.report(index, 0, 0, true)?;
        Ok(ZipDragReader {
            source: self.clone(),
            index,
            position,
            sender: Some(sender),
            worker: Some(worker),
        })
    }

    fn read_requests(
        &self,
        entry: &ArchiveEntry,
        receiver: mpsc::Receiver<ReadRequest>,
        ready: mpsc::SyncSender<Result<()>>,
    ) {
        let mut archive = self.0.archive.clone();
        let mut file = match open_entry(&mut archive, entry, self.0.password.as_deref()) {
            Ok(file) => file,
            Err(error) => {
                let _ = ready.send(Err(error));
                return;
            }
        };
        let mut decoded = 0;
        let mut verified = false;
        if entry.size == 0
            && let Err(error) = read_checked(
                &mut file,
                entry,
                &self.0.control,
                &mut decoded,
                &mut verified,
                &mut [0; 1],
            )
        {
            let _ = ready.send(Err(error));
            return;
        }
        if ready.send(Ok(())).is_err() {
            return;
        }

        while let Ok(request) = receiver.recv() {
            if request.position < decoded {
                drop(file);
                file = match open_entry(&mut archive, entry, self.0.password.as_deref()) {
                    Ok(file) => file,
                    Err(error) => {
                        let _ = request.reply.send(Err(error));
                        return;
                    }
                };
                decoded = 0;
                verified = false;
            }
            let result = read_request(
                &mut file,
                entry,
                &self.0.control,
                &mut decoded,
                &mut verified,
                request.position,
                request.length,
            );
            let failed = result.is_err();
            if request.reply.send(result).is_err() || failed {
                return;
            }
        }
    }

    fn report(&self, index: usize, start: u64, end: u64, force: bool) -> Result<()> {
        let entry = self.0.entries.get(&index).context("ZIP 拖放条目已失效")?;
        let mut progress = self
            .0
            .progress
            .lock()
            .map_err(|_| anyhow::anyhow!("ZIP 拖放进度状态异常"))?;
        let previous = progress.positions.get(&index).copied().unwrap_or(0);
        if start <= previous && end > previous {
            progress.completed += end - previous;
            progress.positions.insert(index, end);
        }
        if force
            || end == entry.size
            || progress.last_report.elapsed() >= Duration::from_millis(100)
        {
            let event = Progress {
                operation: "正在解压",
                current_file: entry.name.clone(),
                completed_bytes: progress.completed,
                total_bytes: progress.total,
                archive_size: None,
            };
            progress.last_report = Instant::now();
            drop(progress);
            self.0.control.report(event);
        }
        Ok(())
    }
}

fn open_archive(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::FILE_SHARE_READ;

        options.share_mode(FILE_SHARE_READ.0);
    }
    options
        .open(path)
        .with_context(|| format!("无法打开压缩包 {}", path.display()))
}

fn open_entry<'a>(
    archive: &'a mut ZipArchive<SharedInput>,
    entry: &ArchiveEntry,
    password: Option<&str>,
) -> Result<ZipFile<'a, SharedInput>> {
    match password.filter(|password| !password.is_empty()) {
        Some(password) => archive.by_index_decrypt(entry.index, password.as_bytes()),
        None => archive.by_index(entry.index),
    }
    .with_context(|| format!("无法读取 {}，密码可能不正确", entry.name))
}

struct ReadRequest {
    position: u64,
    length: usize,
    reply: mpsc::SyncSender<Result<Vec<u8>>>,
}

fn read_request(
    file: &mut ZipFile<'_, SharedInput>,
    entry: &ArchiveEntry,
    control: &JobControl,
    decoded: &mut u64,
    verified: &mut bool,
    position: u64,
    length: usize,
) -> Result<Vec<u8>> {
    let target = position.min(entry.size);
    let mut skipped = [0; IO_BUFFER_SIZE];
    while *decoded < target {
        let length = (target - *decoded).min(IO_BUFFER_SIZE as u64) as usize;
        let count = read_checked(
            file,
            entry,
            control,
            decoded,
            verified,
            &mut skipped[..length],
        )?;
        ensure!(count != 0, "文件内容大小与预期不符：{}", entry.name);
    }
    if position >= entry.size {
        return Ok(Vec::new());
    }
    let mut bytes = vec![0; length];
    let count = read_checked(file, entry, control, decoded, verified, &mut bytes)?;
    bytes.truncate(count);
    Ok(bytes)
}

fn read_checked(
    file: &mut ZipFile<'_, SharedInput>,
    entry: &ArchiveEntry,
    control: &JobControl,
    decoded: &mut u64,
    verified: &mut bool,
    buffer: &mut [u8],
) -> Result<usize> {
    control.check_cancelled()?;
    if *verified {
        return Ok(0);
    }
    let count = file
        .read(buffer)
        .with_context(|| format!("无法解压 {}", entry.name))?;
    *decoded = decoded.checked_add(count as u64).context("文件大小溢出")?;
    ensure!(
        *decoded <= entry.size && (count != 0 || *decoded == entry.size),
        "文件内容大小与预期不符：{}",
        entry.name
    );
    if *decoded == entry.size {
        let extra = file
            .read(&mut [0; 1])
            .with_context(|| format!("无法校验 {}", entry.name))?;
        ensure!(extra == 0, "文件内容超过声明大小：{}", entry.name);
        *verified = true;
    }
    Ok(count)
}

pub struct ZipDragReader {
    source: ZipDragSource,
    index: usize,
    position: u64,
    sender: Option<mpsc::Sender<ReadRequest>>,
    worker: Option<JoinHandle<()>>,
}

impl ZipDragReader {
    pub fn size(&self) -> u64 {
        self.source.0.entries[&self.index].size
    }

    pub fn name(&self) -> &str {
        &self.source.0.entries[&self.index].name
    }

    pub fn try_clone(&self) -> Result<Self> {
        self.source.open_at(self.index, self.position)
    }
}

impl Read for ZipDragReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        self.source
            .0
            .control
            .check_cancelled()
            .map_err(io::Error::other)?;
        let (reply, response) = mpsc::sync_channel(0);
        let request = ReadRequest {
            position: self.position,
            length: buffer.len().min(IO_BUFFER_SIZE),
            reply,
        };
        self.sender
            .as_ref()
            .ok_or_else(|| io::Error::other("ZIP 拖放读取线程已关闭"))?
            .send(request)
            .map_err(|_| io::Error::other("ZIP 拖放读取线程异常退出"))?;
        let bytes = response
            .recv()
            .map_err(|_| io::Error::other("ZIP 拖放读取线程异常退出"))?
            .map_err(|error| io::Error::other(format!("{error:#}")))?;
        let start = self.position;
        self.position += bytes.len() as u64;
        self.source
            .report(self.index, start, self.position, false)
            .map_err(io::Error::other)?;
        buffer[..bytes.len()].copy_from_slice(&bytes);
        Ok(bytes.len())
    }
}

impl Seek for ZipDragReader {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let next = match position {
            SeekFrom::Start(position) => Some(position),
            SeekFrom::Current(offset) => self.position.checked_add_signed(offset),
            SeekFrom::End(offset) => self.size().checked_add_signed(offset),
        }
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "无效的 ZIP 拖放位置"))?;
        self.position = next;
        Ok(next)
    }
}

impl Drop for ZipDragReader {
    fn drop(&mut self) {
        drop(self.sender.take());
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            eprintln!("ZIP 拖放读取线程异常退出");
        }
    }
}

#[cfg(test)]
mod tests {
        use super::*;
        use std::{fs, io::Write};
        use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

        use crate::archive::{CancellationToken, inspect_archive};

        fn archive(path: &Path, files: &[(&str, &[u8])]) {
            let mut writer = ZipWriter::new(File::create(path).unwrap());
            for (name, bytes) in files {
                writer
                    .start_file(
                        *name,
                        SimpleFileOptions::default().compression_method(CompressionMethod::Stored),
                    )
                    .unwrap();
                writer.write_all(bytes).unwrap();
            }
            writer.finish().unwrap();
        }

        fn control(mode: IntegrityMode) -> JobControl {
            JobControl::default().with_integrity(mode)
        }

        #[test]
        fn streams_selected_files_with_independent_seeks_and_bounded_progress() {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("test.zip");
            let expected = (0..256 * 1024).map(|index| (index % 251) as u8).collect::<Vec<_>>();
            archive(
                &path,
                &[("folder/file.bin", &expected), ("other.bin", b"unselected")],
            );
            let events = Arc::new(Mutex::new(Vec::<Progress>::new()));
            let reported = events.clone();
            let control = JobControl::new(CancellationToken::default(), move |event| {
                reported.lock().unwrap().push(event);
            })
            .with_integrity(IntegrityMode::Normal);
            let info = inspect_archive(&path, None, &control).unwrap();
            let source = ZipDragSource::new(&info, &["folder".into()], None, &control).unwrap();
            assert!(events.lock().unwrap().is_empty());
            assert!(source.open(info.entries[1].index).is_err());
            let mut reader = source.open(info.entries[0].index).unwrap();
            assert_eq!(reader.size(), expected.len() as u64);

            let mut first = [0; 31];
            reader.read_exact(&mut first).unwrap();
            assert_eq!(&first, &expected[..31]);
            assert_eq!(reader.seek(SeekFrom::Start(70_000)).unwrap(), 70_000);
            let mut clone = reader.try_clone().unwrap();
            let mut middle = [0; 64];
            reader.read_exact(&mut middle).unwrap();
            assert_eq!(&middle, &expected[70_000..70_064]);
            clone.read_exact(&mut middle).unwrap();
            assert_eq!(&middle, &expected[70_000..70_064]);
            reader.seek(SeekFrom::Start(0)).unwrap();
            let mut actual = Vec::new();
            reader.read_to_end(&mut actual).unwrap();
            assert_eq!(actual, expected);
            assert_eq!(reader.seek(SeekFrom::End(7)).unwrap(), expected.len() as u64 + 7);
            assert_eq!(reader.read(&mut middle).unwrap(), 0);
            assert!(reader.seek(SeekFrom::Start(0)).is_ok());
            assert!(reader.seek(SeekFrom::Current(-1)).is_err());
            let events = events.lock().unwrap();
            let extraction = events
                .iter()
                .filter(|event| event.operation == "正在解压")
                .collect::<Vec<_>>();
            assert_eq!(extraction.last().unwrap().completed_bytes, expected.len() as u64);
            assert!(
                extraction
                    .iter()
                    .all(|event| event.completed_bytes <= event.total_bytes)
            );
        }

        #[test]
        fn cloned_streams_can_read_at_the_same_time() {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("test.zip");
            let expected = (0..512 * 1024).map(|index| (index % 239) as u8).collect::<Vec<_>>();
            archive(&path, &[("file.bin", &expected)]);
            let control = control(IntegrityMode::Normal);
            let info = inspect_archive(&path, None, &control).unwrap();
            let source = ZipDragSource::new(&info, &["file.bin".into()], None, &control).unwrap();
            let reader = source.open(info.entries[0].index).unwrap();
            let clone = reader.try_clone().unwrap();
            let handles = [reader, clone].map(|mut reader| {
                thread::spawn(move || {
                    let mut bytes = Vec::new();
                    reader.read_to_end(&mut bytes).unwrap();
                    bytes
                })
            });
            for handle in handles {
                assert_eq!(handle.join().unwrap(), expected);
            }
        }

        #[test]
        fn selected_entries_are_checked_before_safe_streaming_and_at_eof() {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("corrupt.zip");
            archive(&path, &[("good.txt", b"good"), ("bad.txt", b"bad contents")]);
            let mut zip = ZipArchive::new(File::open(&path).unwrap()).unwrap();
            let offset = zip.by_index(1).unwrap().data_start().unwrap();
            drop(zip);
            let mut file = OpenOptions::new().write(true).open(&path).unwrap();
            file.seek(SeekFrom::Start(offset)).unwrap();
            file.write_all(b"X").unwrap();
            drop(file);
            let safe = control(IntegrityMode::Safe);
            let info = inspect_archive(&path, None, &safe).unwrap();

            let source = ZipDragSource::new(&info, &["good.txt".into()], None, &safe).unwrap();
            let mut good = source.open(info.entries[0].index).unwrap();
            let mut bytes = Vec::new();
            good.read_to_end(&mut bytes).unwrap();
            assert_eq!(bytes, b"good");
            drop(good);
            drop(source);
            let error = ZipDragSource::new(
                &info,
                &["good.txt".into(), "bad.txt".into()],
                None,
                &safe,
            )
            .err()
            .unwrap();
            assert!(format!("{error:#}").contains("bad.txt"));

            let normal = control(IntegrityMode::Normal);
            let source = ZipDragSource::new(&info, &["bad.txt".into()], None, &normal).unwrap();
            let mut bad = source.open(info.entries[1].index).unwrap();
            let mut exact_size = [0; 12];
            let error = bad.read_exact(&mut exact_size).unwrap_err();
            assert!(error.to_string().contains("校验"), "{error}");
        }

        #[test]
        fn encrypted_entries_accept_passwords_and_reject_incorrect_ones() {
            use zip::unstable::write::FileOptionsExt;

            for legacy in [false, true] {
                let temp = tempfile::tempdir().unwrap();
                let path = temp.path().join("encrypted.zip");
                let mut writer = ZipWriter::new(File::create(&path).unwrap());
                let options = if legacy {
                    SimpleFileOptions::default()
                        .with_deprecated_encryption(b"secret")
                        .unwrap()
                } else {
                    SimpleFileOptions::default().with_aes_encryption(zip::AesMode::Aes256, "secret")
                };
                writer.start_file("file.txt", options.clone()).unwrap();
                writer.write_all(b"encrypted content").unwrap();
                writer.start_file("empty.txt", options).unwrap();
                writer.finish().unwrap();
                let safe = control(IntegrityMode::Safe);
                let info = inspect_archive(&path, None, &safe).unwrap();
                let paths = ["file.txt".into(), "empty.txt".into()];
                assert!(ZipDragSource::new(&info, &paths, Some("wrong"), &safe).is_err());
                let source = ZipDragSource::new(&info, &paths, Some("secret"), &safe).unwrap();
                let mut reader = source.open(info.entries[0].index).unwrap();
                let mut bytes = Vec::new();
                reader.read_to_end(&mut bytes).unwrap();
                assert_eq!(bytes, b"encrypted content");
                let mut empty = source.open(info.entries[1].index).unwrap();
                assert_eq!(empty.read(&mut [0; 1]).unwrap(), 0);
            }
        }

        #[test]
        fn rejects_changed_archives_and_unsupported_entries() {
            let temp = tempfile::tempdir().unwrap();
            let changed = temp.path().join("changed.zip");
            archive(&changed, &[("file.txt", b"original")]);
            let control = control(IntegrityMode::Normal);
            let info = inspect_archive(&changed, None, &control).unwrap();
            OpenOptions::new()
                .append(true)
                .open(&changed)
                .unwrap()
                .write_all(b"changed")
                .unwrap();
            assert!(ZipDragSource::new(&info, &["file.txt".into()], None, &control).is_err());

            let unsafe_path = temp.path().join("unsafe.zip");
            archive(&unsafe_path, &[("../escape.txt", b"unsafe")]);
            let info = inspect_archive(&unsafe_path, None, &control).unwrap();
            assert!(ZipDragSource::new(&info, &["../escape.txt".into()], None, &control).is_err());

            let link = temp.path().join("link.zip");
            let mut writer = ZipWriter::new(File::create(&link).unwrap());
            writer
                .add_symlink("link", "target", SimpleFileOptions::default())
                .unwrap();
            writer.finish().unwrap();
            let info = inspect_archive(&link, None, &control).unwrap();
            assert!(ZipDragSource::new(&info, &["link".into()], None, &control).is_err());
        }

        #[test]
        fn cancellation_interrupts_a_stream_without_changing_the_archive() {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("test.zip");
            archive(&path, &[("file.bin", &vec![42; 256 * 1024])]);
            let original = fs::read(&path).unwrap();
            let token = CancellationToken::default();
            let control =
                JobControl::new(token.clone(), |_| {}).with_integrity(IntegrityMode::Normal);
            let info = inspect_archive(&path, None, &control).unwrap();
            let source = ZipDragSource::new(&info, &["file.bin".into()], None, &control).unwrap();
            let mut reader = source.open(info.entries[0].index).unwrap();
            reader.read_exact(&mut [0; 1024]).unwrap();
            token.cancel();
            assert!(reader.read(&mut [0; 1024]).unwrap_err().to_string().contains("取消"));
            assert_eq!(fs::read(path).unwrap(), original);
        }

        #[cfg(windows)]
        #[test]
        fn archive_cannot_be_overwritten_while_streaming() {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("test.zip");
            archive(&path, &[("file.txt", b"contents")]);
            let control = control(IntegrityMode::Normal);
            let info = inspect_archive(&path, None, &control).unwrap();
            let source = ZipDragSource::new(&info, &["file.txt".into()], None, &control).unwrap();
            let reader = source.open(info.entries[0].index).unwrap();
            assert!(OpenOptions::new().write(true).open(&path).is_err());
            assert!(fs::remove_file(&path).is_err());
            drop(reader);
            drop(source);
            assert!(OpenOptions::new().write(true).open(&path).is_ok());
        }
}
