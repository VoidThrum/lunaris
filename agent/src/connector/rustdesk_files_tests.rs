//! Tests of the RustDesk connector's file client, against a host that
//! speaks RustDesk's protocol.
//!
//! The host here is a small one for these tests ([`start_host`]): the
//! host's side of the file transfer over a folder, following RustDesk
//! 1.5.3's `src/server/connection.rs` (the login, `FileAction`s),
//! `src/ui_cm_interface.rs` (`handle_fs` and its helpers) and
//! `libs/base/src/fs.rs` (`read_dir`, `TransferJob`'s reading and writing
//! sides, `is_write_need_confirmation`). The tests know it only by its
//! address and password (`TestHost`), and every one of them that needs a
//! host runs a second time against the real one, the agent's RustDesk
//! listener (`rustdesk/files.rs`; the tests of [`against_the_listener`]):
//! this client and that host, both sides at once. What a host cannot be
//! asked to do, a connection that breaks in the middle, is done between the
//! two ([`cutting_proxy`]).

use std::io::{Read, Seek, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};

use tokio::net::TcpListener;

use super::*;
use crate::rustdesk::login;

// ------------------------------------------------------------ the host

#[derive(Clone)]
pub(crate) struct HostOptions {
    pub(crate) password: String,
    /// `PeerInfo.platform`.
    pub(crate) platform: String,
    /// `PeerInfo.version`; also what the host can do (resume from 1.4.2).
    pub(crate) version: String,
    /// The host's "Enable file transfer" permission.
    pub(crate) file_permission: bool,
}

impl Default for HostOptions {
    /// A host of this system (its folder is one of this computer's, with
    /// this system's paths) and of the listener's version.
    fn default() -> Self {
        HostOptions { password: "mật khẩu".into(), platform: own_platform().into(), version: "1.5.3".into(), file_permission: true }
    }
}

/// `PeerInfo.platform` of a RustDesk host on this system
/// (`hbb_common::whoami::platform()`), as the listener says it.
fn own_platform() -> &'static str {
    match std::env::consts::OS {
        "linux" => "Linux",
        "windows" => "Windows",
        "macos" => "Mac OS",
        other => other,
    }
}

pub(crate) struct TestHost {
    pub(crate) address: SocketAddr,
    pub(crate) password: String,
}

thread_local! {
    /// Set by the tests of [`against_the_listener`]: the host is the
    /// agent's RustDesk listener.
    static REAL_HOST: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// A RustDesk-protocol file host over `home` (its home folder): the small
/// one here, or (for the tests of [`against_the_listener`]) the agent's
/// RustDesk listener, given `home` as the folder its clients start in. The
/// listener is this computer and this version: a host of another platform
/// or an older version is always the small one.
pub(crate) async fn start_host(home: &Path, options: HostOptions) -> TestHost {
    let own = HostOptions::default();
    if REAL_HOST.with(|real| real.get()) && options.platform == own.platform && options.version == own.version {
        // The whole disk, as a RustDesk host gives it (what these tests
        // expect of "/"); what a limited `file_access` refuses is tested
        // with the listener itself (`rustdesk/files_tests.rs`).
        let listener = crate::rustdesk::files::tests::start_listener(
            &options.password,
            "password",
            options.file_permission,
            crate::files::Access::Full,
            Some(home.to_path_buf()),
        )
        .await;
        return TestHost { address: listener.address, password: options.password };
    }
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let password = options.password.clone();
    let home = home.to_path_buf();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(fake_host::serve(stream, home.clone(), options.clone()));
        }
    });
    TestHost { address, password }
}

/// A TCP connection to `upstream` that breaks, without a word, when this
/// many bytes went to the client (`down`) or to the host (`up`).
async fn cutting_proxy(upstream: SocketAddr, down: Option<u64>, up: Option<u64>) -> SocketAddr {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let Ok((mut client, _)) = listener.accept().await else { return };
        let Ok(mut host) = TcpStream::connect(upstream).await else { return };
        let (mut to_client, mut to_host) = (0u64, 0u64);
        let (mut a, mut b) = (vec![0u8; 64 * 1024], vec![0u8; 64 * 1024]);
        loop {
            tokio::select! {
                n = client.read(&mut a) => {
                    let Ok(n) = n else { break };
                    to_host += n as u64;
                    if n == 0 || up.is_some_and(|limit| to_host >= limit) || host.write_all(&a[..n]).await.is_err() {
                        break;
                    }
                }
                n = host.read(&mut b) => {
                    let Ok(n) = n else { break };
                    to_client += n as u64;
                    if n == 0 || down.is_some_and(|limit| to_client >= limit) || client.write_all(&b[..n]).await.is_err() {
                        break;
                    }
                }
            }
        }
    });
    address
}

mod fake_host {
    use super::*;

    struct ReadJob {
        id: i32,
        base: PathBuf,
        files: Vec<proto::FileEntry>,
        file_num: i32,
        stream: Option<std::fs::File>,
        confirmed: bool,
        waiting: bool,
        skipped: bool,
    }

    struct WriteJob {
        id: i32,
        base: PathBuf,
        files: Vec<proto::FileEntry>,
        file_num: i32,
        stream: Option<std::fs::File>,
        /// fs.rs `FileDigest`: size, modified.
        digest: (u64, u64),
    }

    struct Host {
        write: tokio::net::tcp::OwnedWriteHalf,
        home: PathBuf,
        /// Whether this host seeks to a confirm's offset (RustDesk from 1.4.2).
        resume: bool,
        read_jobs: Vec<ReadJob>,
        write_jobs: Vec<WriteJob>,
    }

    fn mtime_of(meta: &std::fs::Metadata) -> u64 {
        meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0)
    }

    /// fs.rs `read_dir`, of a folder (the list of drives is `drives`).
    fn read_dir(path: &Path, include_hidden: bool) -> std::io::Result<FileDirectory> {
        let mut dir = FileDirectory { id: 0, path: path.to_string_lossy().into_owned(), entries: Vec::new() };
        for entry in path.read_dir()?.flatten() {
            let p = entry.path();
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("").to_owned();
            if name.is_empty() {
                continue;
            }
            let Ok(meta) = std::fs::symlink_metadata(&p) else { continue };
            #[cfg(windows)]
            let is_hidden = {
                use std::os::windows::fs::MetadataExt;
                meta.file_attributes() & 0x2 != 0
            };
            #[cfg(not(windows))]
            let is_hidden = name.starts_with('.');
            if is_hidden && !include_hidden {
                continue;
            }
            let link = meta.file_type().is_symlink();
            let (entry_type, size) = if p.is_dir() {
                (if link { FileType::DirLink } else { FileType::Dir }, 0)
            } else if link {
                (FileType::FileLink, 0)
            } else {
                (FileType::File, meta.len())
            };
            dir.entries.push(proto::FileEntry { entry_type: entry_type as i32, name, is_hidden, size, modified_time: mtime_of(&meta) });
        }
        Ok(dir)
    }

    /// fs.rs `read_dir` of "/" on Windows: the drives.
    #[cfg(windows)]
    fn drives() -> FileDirectory {
        let entries = (b'A'..=b'Z')
            .filter(|letter| Path::new(&format!("{}:\\", *letter as char)).exists())
            .map(|letter| proto::FileEntry { name: format!("{}:", letter as char), entry_type: FileType::DirDrive as i32, ..Default::default() })
            .collect();
        FileDirectory { id: 0, path: "/".into(), entries }
    }

    /// fs.rs `read_dir_recursive` (`get_recursive_files`).
    fn recursive_files(path: &Path, prefix: &Path, include_hidden: bool) -> Result<Vec<proto::FileEntry>, String> {
        let mut files = Vec::new();
        if path.is_dir() {
            let dir = read_dir(path, include_hidden).map_err(|e| e.to_string())?;
            for entry in &dir.entries {
                if entry.entry_type == FileType::File as i32 {
                    let mut entry = entry.clone();
                    entry.name = prefix.join(&entry.name).to_string_lossy().into_owned();
                    files.push(entry);
                } else if entry.entry_type == FileType::Dir as i32 {
                    if let Ok(mut inner) = recursive_files(&path.join(&entry.name), &prefix.join(&entry.name), include_hidden) {
                        files.append(&mut inner);
                    }
                }
            }
            Ok(files)
        } else if path.is_file() {
            let (size, modified_time) = std::fs::metadata(path).map(|m| (m.len(), mtime_of(&m))).unwrap_or((0, 0));
            files.push(proto::FileEntry { entry_type: FileType::File as i32, size, modified_time, ..Default::default() });
            Ok(files)
        } else {
            Err("Not exists".into())
        }
    }

    /// fs.rs `validate_file_name_no_traversal` (with its Windows part on
    /// Windows).
    fn validate_name(name: &str) -> Result<(), String> {
        if name.bytes().any(|b| b == 0) {
            return Err("file name contains null bytes".into());
        }
        let windows = cfg!(windows);
        if name.split(|c| c == '/' || (windows && c == '\\')).filter(|s| !s.is_empty()).any(|s| s == "..") {
            return Err("path traversal detected in file name".into());
        }
        let drive = matches!(name.as_bytes(), [letter, b':', ..] if letter.is_ascii_alphabetic());
        if name.starts_with('/') || (windows && (name.starts_with('\\') || drive)) {
            return Err("absolute path detected in file name".into());
        }
        Ok(())
    }

    /// fs.rs `TransferJob::join`.
    fn join(base: &Path, name: &str) -> PathBuf {
        if name.is_empty() {
            base.to_path_buf()
        } else {
            base.join(name)
        }
    }

    fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
        PathBuf::from(format!("{}.{}", path.to_string_lossy(), suffix))
    }

    /// fs.rs `remove_all_empty_dir`.
    fn remove_all_empty_dir(path: &Path) -> Result<(), String> {
        let dir = read_dir(path, true).map_err(|e| e.to_string())?;
        for entry in &dir.entries {
            if entry.entry_type == FileType::Dir as i32 {
                let _ = remove_all_empty_dir(&path.join(&entry.name));
            } else if entry.entry_type == FileType::DirLink as i32 || entry.entry_type == FileType::FileLink as i32 {
                // A link to a folder is removed as a folder on Windows.
                if std::fs::remove_file(path.join(&entry.name)).is_err() && cfg!(windows) {
                    let _ = std::fs::remove_dir(path.join(&entry.name));
                }
            }
        }
        let _ = std::fs::remove_dir(path);
        Ok(())
    }

    enum DigestCheck {
        NeedConfirm(FileTransferDigest),
        NoSuchFile,
    }

    /// fs.rs `is_write_need_confirmation`.
    fn is_write_need_confirmation(is_resume: bool, path: &Path, digest: &FileTransferDigest) -> Result<DigestCheck, String> {
        let (digest_file, download_file) = (with_suffix(path, "digest"), with_suffix(path, "download"));
        if is_resume && digest_file.exists() && download_file.exists() {
            if let Ok(local) = std::fs::read_to_string(&digest_file).map_err(|_| ()).and_then(|c| serde_json::from_str::<serde_json::Value>(&c).map_err(|_| ())) {
                let is_identical = local["modified"].as_u64() == Some(digest.last_modified) && local["size"].as_u64() == Some(digest.file_size);
                if is_identical {
                    let transferred_size = std::fs::metadata(&download_file).map(|m| m.len()).unwrap_or(0);
                    if transferred_size > 0 {
                        return Ok(DigestCheck::NeedConfirm(FileTransferDigest {
                            id: digest.id,
                            file_num: digest.file_num,
                            last_modified: digest.last_modified,
                            file_size: digest.file_size,
                            is_identical,
                            transferred_size,
                            ..Default::default()
                        }));
                    }
                }
            }
        }
        if path.exists() && path.is_file() {
            let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
            let local = mtime_of(&meta);
            Ok(DigestCheck::NeedConfirm(FileTransferDigest {
                id: digest.id,
                file_num: digest.file_num,
                last_modified: local,
                file_size: meta.len(),
                is_identical: local == digest.last_modified && digest.file_size == meta.len(),
                ..Default::default()
            }))
        } else {
            Ok(DigestCheck::NoSuchFile)
        }
    }

    /// fs.rs `new_error`, `new_done`, `new_dir`.
    fn new_error(id: i32, error: impl ToString, file_num: i32) -> Message {
        response(file_response::Union::Error(proto::FileTransferError { id, error: error.to_string(), file_num }))
    }

    fn new_done(id: i32, file_num: i32) -> Message {
        response(file_response::Union::Done(proto::FileTransferDone { id, file_num }))
    }

    impl ReadJob {
        /// fs.rs `TransferJob::set_stream_offset`, for a job that reads.
        fn seek(&mut self, offset: u64) {
            let Some(entry) = self.files.get(self.file_num as usize) else { return };
            if let Ok(mut file) = std::fs::File::open(join(&self.base, &entry.name)) {
                if file.seek(std::io::SeekFrom::Start(offset)).is_ok() {
                    self.stream = Some(file);
                }
            }
        }

        /// fs.rs `TransferJob::confirm`.
        fn confirm(&mut self, r: &FileTransferSendConfirmRequest, resume: bool) {
            if self.file_num != r.file_num {
                return;
            }
            match r.union {
                Some(confirm::Union::Skip(true)) => {
                    // `set_file_skipped`
                    self.stream = None;
                    self.confirmed = false;
                    self.waiting = false;
                    self.file_num += 1;
                    self.skipped = true;
                }
                Some(confirm::Union::Skip(false)) => self.confirmed = true,
                Some(confirm::Union::OffsetBlk(offset)) => {
                    self.confirmed = true;
                    if offset > 0 && resume {
                        self.seek(offset as u64);
                    }
                }
                None => {}
            }
        }
    }

    impl WriteJob {
        /// fs.rs `TransferJob::modify_time`.
        fn modify_time(&self) {
            let Some(entry) = self.files.get(self.file_num as usize) else { return };
            if validate_name(&entry.name).is_err() {
                return;
            }
            let path = join(&self.base, &entry.name);
            let _ = std::fs::remove_file(with_suffix(&path, "digest"));
            let _ = std::fs::rename(with_suffix(&path, "download"), &path);
            if let Ok(file) = std::fs::File::options().write(true).open(&path) {
                let _ = file.set_modified(UNIX_EPOCH + Duration::from_secs(entry.modified_time));
            }
        }

        /// fs.rs `TransferJob::remove_download_file`.
        fn remove_download_file(&self) {
            let Some(entry) = self.files.get(self.file_num as usize) else { return };
            let path = join(&self.base, &entry.name);
            let _ = std::fs::remove_file(with_suffix(&path, "download"));
            let _ = std::fs::remove_file(with_suffix(&path, "digest"));
        }

        /// fs.rs `TransferJob::write`.
        fn write(&mut self, block: FileTransferBlock) -> Result<(), String> {
            if block.id != self.id {
                return Err("Wrong id".into());
            }
            let file_num = block.file_num as usize;
            if file_num >= self.files.len() {
                return Err("Wrong file number".into());
            }
            if block.file_num != self.file_num || self.stream.is_none() {
                self.modify_time();
                if let Some(file) = self.stream.as_mut() {
                    file.sync_all().map_err(|e| e.to_string())?;
                }
                self.file_num = block.file_num;
                let name = &self.files[file_num].name;
                validate_name(name)?;
                let path = join(&self.base, name);
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let digest_path = with_suffix(&path, "digest");
                if digest_path.exists() {
                    std::fs::remove_file(&digest_path).map_err(|e| e.to_string())?;
                }
                self.stream = Some(std::fs::File::create(with_suffix(&path, "download")).map_err(|e| e.to_string())?);
                let _ = std::fs::write(&digest_path, serde_json::json!({"size": self.digest.0, "modified": self.digest.1}).to_string());
            }
            let data = if block.compressed {
                // hbb_common `decompress`: nothing when it cannot be read.
                cursor::decompress(&block.data, MAX_DECOMPRESSED_SIZE).unwrap_or_default()
            } else {
                block.data.to_vec()
            };
            self.stream.as_mut().ok_or("file is None")?.write_all(&data).map_err(|e| e.to_string())
        }

        /// fs.rs `TransferJob::confirm` -> `set_stream_offset`, for a job that writes.
        fn confirm(&mut self, r: &FileTransferSendConfirmRequest, resume: bool) {
            if self.file_num != r.file_num {
                return;
            }
            match r.union {
                Some(confirm::Union::Skip(true)) => {
                    self.stream = None;
                    self.file_num += 1;
                }
                Some(confirm::Union::OffsetBlk(offset)) if offset > 0 && resume => {
                    let Some(entry) = self.files.get(r.file_num as usize) else { return };
                    let path = join(&self.base, &entry.name);
                    let (download, digest) = (with_suffix(&path, "download"), with_suffix(&path, "digest"));
                    if download.exists() && digest.exists() {
                        if let Ok(mut file) = std::fs::File::options().create(true).write(true).open(&download) {
                            if file.seek(std::io::SeekFrom::Start(offset as u64)).is_ok() {
                                self.stream = Some(file);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    impl Host {
        async fn send(&mut self, msg: Message) -> bool {
            let mut out = BytesMut::new();
            encode(&mut vec![msg], &mut out).unwrap();
            self.write.write_all(&out).await.is_ok()
        }

        /// ui_cm_interface.rs `read_dir`: "" is the home folder.
        async fn read_dir(&mut self, dir: &str, include_hidden: bool) -> bool {
            #[cfg(windows)]
            if dir == "/" {
                return self.send(response(file_response::Union::Dir(drives()))).await;
            }
            let path = if dir.is_empty() { self.home.clone() } else { PathBuf::from(dir) };
            let msg = match read_dir(&path, include_hidden) {
                Ok(dir) => response(file_response::Union::Dir(dir)),
                Err(e) => new_error(0, e, -1),
            };
            self.send(msg).await
        }

        /// ui_cm_interface.rs `handle_result`.
        async fn result(&mut self, result: Result<(), String>, id: i32, file_num: i32) -> bool {
            let msg = match result {
                Ok(()) => new_done(id, file_num),
                Err(e) => new_error(id, e, file_num),
            };
            self.send(msg).await
        }

        /// connection.rs, `Message::FileAction`.
        async fn file_action(&mut self, union: file_action::Union) -> bool {
            match union {
                file_action::Union::ReadDir(rd) => self.read_dir(&rd.path, rd.include_hidden).await,
                file_action::Union::AllFiles(f) => {
                    let msg = match recursive_files(Path::new(&f.path), Path::new(""), f.include_hidden) {
                        Ok(files) => response(file_response::Union::Dir(FileDirectory { id: f.id, path: f.path, entries: files })),
                        Err(e) => new_error(f.id, e, -1),
                    };
                    self.send(msg).await
                }
                // `create_and_start_read_job` and `process_new_read_job`.
                file_action::Union::Send(s) => match recursive_files(Path::new(&s.path), Path::new(""), s.include_hidden) {
                    Err(e) => self.send(new_error(s.id, e, 0)).await,
                    Ok(files) => {
                        let listing = FileDirectory { id: s.id, path: s.path.clone(), entries: files.clone() };
                        self.read_jobs.push(ReadJob {
                            id: s.id,
                            base: PathBuf::from(&s.path),
                            files,
                            file_num: s.file_num,
                            stream: None,
                            confirmed: false,
                            waiting: false,
                            skipped: false,
                        });
                        self.send(response(file_response::Union::Dir(listing))).await
                    }
                },
                // ui_cm_interface.rs `FS::NewWrite`.
                file_action::Union::Receive(r) => {
                    let single = r.files.len() == 1 && r.files[0].name.is_empty();
                    if !single {
                        if let Some(e) = r.files.iter().find_map(|f| if f.name.is_empty() { Some("empty file name in multi-file transfer".to_string()) } else { validate_name(&f.name).err() }) {
                            return self.send(new_error(r.id, e, r.file_num)).await;
                        }
                    }
                    self.write_jobs.push(WriteJob { id: r.id, base: PathBuf::from(&r.path), files: r.files, file_num: r.file_num, stream: None, digest: (0, 0) });
                    true
                }
                // ui_cm_interface.rs `remove_dir`, `remove_file`, `create_dir`, `rename_file`.
                file_action::Union::RemoveDir(d) => {
                    let path = PathBuf::from(&d.path);
                    let result = if d.recursive { remove_all_empty_dir(&path) } else { std::fs::remove_dir(&path).map_err(|e| e.to_string()) };
                    self.result(result, d.id, 0).await
                }
                file_action::Union::RemoveFile(f) => {
                    let result = if f.path.is_empty() { Err("file path cannot be empty".to_string()) } else { std::fs::remove_file(&f.path).map_err(|e| e.to_string()) };
                    self.result(result, f.id, f.file_num).await
                }
                file_action::Union::Create(c) => {
                    let result = if c.path.is_empty() { Err("directory path cannot be empty".to_string()) } else { std::fs::create_dir_all(&c.path).map_err(|e| e.to_string()) };
                    self.result(result, c.id, 0).await
                }
                // fs.rs `rename_file`.
                file_action::Union::Rename(r) => {
                    let path = Path::new(&r.path);
                    let result = if r.new_name.is_empty() {
                        Err("new file name cannot be empty".to_string())
                    } else if let Err(e) = validate_name(&r.new_name) {
                        Err(e)
                    } else if !path.exists() {
                        Err(format!("{:?} not exists", path))
                    } else {
                        std::fs::rename(path, path.parent().unwrap_or(Path::new("/")).join(&r.new_name)).map_err(|e| e.to_string())
                    };
                    self.result(result, r.id, 0).await
                }
                // `FS::CancelWrite` and `FS::CancelRead`.
                file_action::Union::Cancel(c) => {
                    if let Some(i) = self.write_jobs.iter().position(|j| j.id == c.id) {
                        self.write_jobs.remove(i).remove_download_file();
                    }
                    self.read_jobs.retain(|j| j.id != c.id);
                    true
                }
                file_action::Union::SendConfirm(r) => {
                    let resume = self.resume;
                    if let Some(job) = self.read_jobs.iter_mut().find(|j| j.id == r.id) {
                        job.confirm(&r, resume);
                    } else if let Some(job) = self.write_jobs.iter_mut().find(|j| j.id == r.id) {
                        job.confirm(&r, resume);
                    }
                    true
                }
                file_action::Union::ReadEmptyDirs(_) => true,
            }
        }

        /// connection.rs, `Message::FileResponse` (an upload's data), with
        /// ui_cm_interface.rs `FS::WriteBlock`, `WriteDone`, `WriteError`
        /// and `CheckDigest`.
        async fn file_response(&mut self, union: file_response::Union) -> bool {
            match union {
                file_response::Union::Block(block) => {
                    let (id, file_num) = (block.id, block.file_num);
                    if let Some(job) = self.write_jobs.iter_mut().find(|j| j.id == id) {
                        if let Err(e) = job.write(block) {
                            return self.send(new_error(id, e, file_num)).await;
                        }
                    }
                    true
                }
                file_response::Union::Done(d) => {
                    if let Some(i) = self.write_jobs.iter().position(|j| j.id == d.id) {
                        let job = self.write_jobs.remove(i);
                        job.modify_time();
                        return self.send(new_done(d.id, d.file_num)).await;
                    }
                    true
                }
                file_response::Union::Error(e) => {
                    if let Some(i) = self.write_jobs.iter().position(|j| j.id == e.id) {
                        self.write_jobs.remove(i);
                        return self.send(new_error(e.id, e.error, e.file_num)).await;
                    }
                    true
                }
                file_response::Union::Digest(d) => {
                    let Some(job) = self.write_jobs.iter_mut().find(|j| j.id == d.id) else { return true };
                    let Some(file) = job.files.get(d.file_num as usize) else { return true };
                    let path = join(&job.base, &file.name);
                    let msg = match is_write_need_confirmation(d.is_resume, &path, &d) {
                        Err(e) => new_error(d.id, e, d.file_num),
                        Ok(check) => {
                            job.digest = (d.file_size, d.last_modified);
                            match check {
                                DigestCheck::NeedConfirm(mut digest) => {
                                    digest.is_upload = true;
                                    response(file_response::Union::Digest(digest))
                                }
                                DigestCheck::NoSuchFile => send_confirm(d.id, d.file_num, confirm::Union::OffsetBlk(0)),
                            }
                        }
                    };
                    self.send(msg).await
                }
                _ => true,
            }
        }

        /// fs.rs `handle_read_jobs` (`init_jobs`, then `TransferJob::read`
        /// of the first job), with overwrite detection on. `idle` is set
        /// when there was nothing to send (a job waits for its confirm).
        async fn handle_read_jobs(&mut self, idle: &mut bool) -> bool {
            *idle = false;
            let Some(job) = self.read_jobs.first_mut() else { return true };
            let id = job.id;
            // `init_data_stream`: `open_data_stream`, then the digest.
            if (job.file_num as usize) < job.files.len() {
                if job.stream.is_none() {
                    match std::fs::File::open(join(&job.base, &job.files[job.file_num as usize].name)) {
                        Ok(file) => {
                            job.stream = Some(file);
                            job.confirmed = false;
                            job.waiting = false;
                        }
                        Err(e) => {
                            job.file_num += 1;
                            job.confirmed = false;
                            job.waiting = false;
                            let file_num = job.file_num;
                            return self.send(new_error(id, e, file_num)).await;
                        }
                    }
                }
                if !job.confirmed && !job.waiting {
                    let meta = job.stream.as_ref().unwrap().metadata().unwrap();
                    let digest = FileTransferDigest { id, file_num: job.file_num, last_modified: mtime_of(&meta), file_size: meta.len(), ..Default::default() };
                    job.waiting = true;
                    return self.send(response(file_response::Union::Digest(digest))).await;
                }
            } else {
                job.stream = None;
            }
            // `read`
            if !job.confirmed {
                // `job_completed`: not waiting for a confirm.
                if !job.waiting {
                    let (file_num, skipped) = (job.file_num, job.skipped && job.files.len() == 1);
                    self.read_jobs.remove(0);
                    return self.send(if skipped { new_error(id, "skipped", file_num) } else { new_done(id, file_num) }).await;
                }
                *idle = true;
                return true;
            }
            let file_num = job.file_num;
            let name = if job.files.len() == 1 && job.files[0].name.is_empty() {
                job.base.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string()
            } else {
                job.files[file_num as usize].name.clone()
            };
            let mut buf = vec![0u8; BLOCK];
            let mut filled = 0;
            while filled < BLOCK {
                match job.stream.as_mut().unwrap().read(&mut buf[filled..]) {
                    Ok(0) => break,
                    Ok(n) => filled += n,
                    Err(e) => {
                        job.file_num += 1;
                        job.stream = None;
                        job.confirmed = false;
                        job.waiting = false;
                        let file_num = job.file_num;
                        return self.send(new_error(id, e, file_num)).await;
                    }
                }
            }
            if filled == 0 {
                job.file_num += 1;
                job.stream = None;
                job.confirmed = false;
                job.waiting = false;
            }
            let block = block_of(id, file_num, &buf[..filled], !is_compressed_file(&name));
            self.send(response(file_response::Union::Block(block))).await
        }
    }

    /// connection.rs: `on_open` (the hash), `handle_login_request`,
    /// `send_logon_response`, then the messages of a file-transfer connection.
    pub(super) async fn serve(stream: TcpStream, home: PathBuf, options: HostOptions) {
        let (mut read, write) = stream.into_split();
        let mut decoder = frame::Decoder::new(frame::MAX_FRAME_LENGTH);
        let mut buf = BytesMut::new();
        let resume = get_version_number(&options.version) >= get_version_number(MIN_VER_RESUME);
        let mut host = Host { write, home, resume, read_jobs: Vec::new(), write_jobs: Vec::new() };
        let (salt, challenge) = ("muối".to_string(), "thử thách".to_string());
        if !host.send(Message { union: Some(message::Union::Hash(proto::Hash { salt: salt.clone(), challenge: challenge.clone() })) }).await {
            return;
        }
        let login_error = |error: &str| Message {
            union: Some(message::Union::LoginResponse(proto::LoginResponse { union: Some(login_response::Union::Error(error.to_string())), ..Default::default() })),
        };
        let mut logged_in = false;
        let mut idle = false;
        loop {
            let msg = tokio::select! {
                biased;
                msg = next_message(&mut read, &mut decoder, &mut buf) => match msg {
                    Ok(Some(msg)) => msg,
                    _ => return,
                },
                // The host's file timer (1 ms while it has files to send;
                // here a block goes as soon as the one before went, and
                // the timer is only waited for when there was nothing to send).
                _ = async { if idle { tokio::time::sleep(Duration::from_millis(1)).await } else { tokio::task::yield_now().await } }, if !host.read_jobs.is_empty() => {
                    if !host.handle_read_jobs(&mut idle).await {
                        return;
                    }
                    continue;
                }
            };
            let alive = match msg.union {
                Some(message::Union::LoginRequest(lr)) => {
                    let Some(login_request::Union::FileTransfer(ft)) = lr.union else {
                        host.send(login_error("This test host has files only")).await;
                        return;
                    };
                    if !options.file_permission {
                        host.send(login_error(NO_FILE_PERMISSION)).await;
                        return;
                    }
                    if !login::verify(&options.password, &salt, &challenge, &lr.password) {
                        host.send(login_error(login::LOGIN_MSG_PASSWORD_WRONG)).await
                    } else {
                        logged_in = true;
                        let info = PeerInfo { platform: options.platform.clone(), version: options.version.clone(), username: "an".into(), hostname: "máy-thử".into(), ..Default::default() };
                        host.send(Message {
                            union: Some(message::Union::LoginResponse(proto::LoginResponse { union: Some(login_response::Union::PeerInfo(info)), ..Default::default() })),
                        })
                        .await
                            && host.read_dir(if Path::new(&ft.dir).is_dir() { &ft.dir } else { "" }, ft.show_hidden).await
                    }
                }
                Some(message::Union::FileAction(FileAction { union: Some(union) })) if logged_in => host.file_action(union).await,
                Some(message::Union::FileResponse(FileResponse { union: Some(union) })) if logged_in => host.file_response(union).await,
                Some(message::Union::Misc(Misc { union: Some(misc::Union::CloseReason(_)) })) => return,
                _ => true,
            };
            if !alive {
                return;
            }
        }
    }
}

// ------------------------------------------------------------ the client

pub(crate) fn tmp(name: &str) -> PathBuf {
    // The two runs of a test (see `against_the_listener`) do not share a folder.
    let host = if REAL_HOST.with(|real| real.get()) { "listener-" } else { "" };
    let dir = std::env::temp_dir().join(format!("lunaris-rd-files-{}{}-{}", host, name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // A path as a host names its folders: on Windows without the `\\?\`
    // of `fs::canonicalize`.
    crate::files::canonical(&dir).unwrap()
}

pub(crate) fn target(address: SocketAddr, password: &str) -> Target {
    serde_json::from_value(serde_json::json!({
        "connector": "rustdesk", "host": address.ip().to_string(), "port": address.port().to_string(), "password": password,
    }))
    .unwrap()
}

/// The path as it is written on this system, with its separator only
/// (`home.join("a/b")` has both on Windows): what the host is asked for,
/// and what the session answers with.
pub(crate) fn text(path: &Path) -> String {
    let text = path.to_string_lossy().into_owned();
    if cfg!(windows) {
        text.replace('/', "\\")
    } else {
        text
    }
}

/// The places a session with a host of this system starts with, after its
/// home: "/" or, on Windows, the drives.
pub(crate) fn places(home: &Path) -> Vec<Place> {
    let mut places = vec![Place { name: "Home".into(), path: text(home) }];
    if cfg!(windows) {
        for letter in b'A'..=b'Z' {
            let drive = format!("{}:\\", letter as char);
            if Path::new(&drive).exists() {
                places.push(Place { name: format!("{}:", letter as char), path: drive });
            }
        }
    } else {
        places.push(Place { name: "/".into(), path: "/".into() });
    }
    places
}

/// The system's words for what is not there ("No such file or directory";
/// on Windows "The system cannot find the file / the path specified").
pub(crate) fn says_not_there(error: &str) -> bool {
    error.contains("No such file") || error.contains("cannot find the")
}

/// The system's words for a folder asked for where a file is.
fn says_not_a_folder(error: &str) -> bool {
    if cfg!(windows) {
        // "The system cannot find the path specified", "The directory name
        // is invalid", "Cannot create a file when that file already exists".
        error.contains("cannot find the path") || error.contains("directory name is invalid") || error.contains("already exists")
    } else {
        error.contains("Not a directory") || error.contains("File exists")
    }
}

/// Hidden as this system has it: a name with a dot in front, and on Windows
/// the attribute (libs/base/src/fs.rs `read_dir` reads only that there).
fn hide(_path: &Path) {
    #[cfg(windows)]
    assert!(std::process::Command::new("attrib").arg("+h").arg(_path).status().unwrap().success());
}

/// A link to a folder: a symbolic link, and on Windows a junction (which
/// needs no privilege; a link to a file does there).
fn link_dir(target: &Path, link: &Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
    #[cfg(windows)]
    {
        let out = std::process::Command::new("cmd").arg("/c").arg("mklink").arg("/J").arg(link).arg(target).output().unwrap();
        assert!(out.status.success(), "mklink /J: {}", String::from_utf8_lossy(&out.stderr));
    }
}

struct Client {
    session: FileSession,
    seq: u32,
}

#[derive(Debug, PartialEq)]
enum Put {
    Done(String),
    Skipped(String),
}

impl Client {
    async fn open(host: &TestHost) -> Client {
        Client::open_at(host.address, &host.password).await.expect("the file session opens")
    }

    async fn open_at(address: SocketAddr, password: &str) -> anyhow::Result<Client> {
        Ok(Client { session: open(&target(address, password)).await?, seq: 0 })
    }

    fn id(&mut self) -> String {
        self.seq += 1;
        format!("t{}", self.seq)
    }

    async fn send(&self, command: FileCommand) {
        self.session.commands.send(command).await.expect("the session takes commands");
    }

    async fn event(&mut self) -> FileEvent {
        tokio::time::timeout(Duration::from_secs(30), self.session.events.recv()).await.expect("an event in time").expect("the session is open")
    }

    async fn list(&mut self, path: &str, hidden: bool) -> Result<(String, Option<String>, Vec<FileEntry>), String> {
        let id = self.id();
        self.send(FileCommand::List { id: id.clone(), path: path.into(), hidden }).await;
        match self.event().await {
            FileEvent::Dir { id: got, path, parent, entries } if got == id => Ok((path, parent, entries)),
            FileEvent::Error { id: got, message } if got == id => Err(message),
            other => panic!("{:?}", other),
        }
    }

    async fn names(&mut self, path: &Path) -> Vec<String> {
        let mut names: Vec<String> = self.list(&text(path), true).await.unwrap().2.into_iter().map(|e| e.name).collect();
        names.sort();
        names
    }

    /// A command that is answered with `Ok` or an error.
    async fn act(&mut self, command: impl FnOnce(String) -> FileCommand) -> Result<(), String> {
        let id = self.id();
        self.send(command(id.clone())).await;
        match self.event().await {
            FileEvent::Ok { id: got } if got == id => Ok(()),
            FileEvent::Error { id: got, message } if got == id => Err(message),
            other => panic!("{:?}", other),
        }
    }

    /// Downloads `path` from `offset`, handing the data to `sink`: the size
    /// and time the host named, and how much came.
    async fn download_to(&mut self, path: &str, offset: u64, mut sink: impl FnMut(&[u8])) -> Result<(u64, u64, u64), String> {
        let id = self.id();
        self.send(FileCommand::Read { id: id.clone(), path: path.into(), offset }).await;
        let (mut size, mut mtime, mut got) = (0, 0, 0u64);
        loop {
            match self.event().await {
                FileEvent::ReadStart { id: of, size: s, mtime: m, offset: o } if of == id => {
                    assert_eq!(o, offset.min(s));
                    (size, mtime) = (s, m);
                }
                FileEvent::ReadData { id: of, data } if of == id => {
                    got += data.len() as u64;
                    sink(&data);
                }
                FileEvent::ReadEnd { id: of, cancelled: false } if of == id => return Ok((size, mtime, got)),
                FileEvent::Error { id: of, message } if of == id => return Err(message),
                other => panic!("{:?}", other),
            }
        }
    }

    async fn download(&mut self, path: &str, offset: u64) -> Result<Vec<u8>, String> {
        let mut data = Vec::new();
        let (size, _, got) = self.download_to(path, offset, |chunk| data.extend_from_slice(chunk)).await?;
        assert_eq!(got, size - offset.min(size));
        Ok(data)
    }

    /// Asks to write `path`; `Ok(Ok((offset, path)))` when the host takes data.
    async fn put(&mut self, path: &str, size: u64, mtime: u64, on_exists: OnExists) -> (String, Result<Result<(u64, String), Put>, String>) {
        let id = self.id();
        self.send(FileCommand::Write { id: id.clone(), path: path.into(), size, mtime, on_exists }).await;
        let answer = match self.event().await {
            FileEvent::WriteReady { id: of, offset, path } if of == id => Ok(Ok((offset, path))),
            FileEvent::WriteSkip { id: of, reason, .. } if of == id => Ok(Err(Put::Skipped(reason))),
            FileEvent::Error { id: of, message } if of == id => Err(message),
            other => panic!("{:?}", other),
        };
        (id, answer)
    }

    async fn put_end(&mut self, id: &str) -> Result<Put, String> {
        self.send(FileCommand::WriteEnd { id: id.into() }).await;
        match self.event().await {
            FileEvent::WriteDone { id: of, path } if of == id => Ok(Put::Done(path)),
            FileEvent::Error { id: of, message } if of == id => Err(message),
            other => panic!("{:?}", other),
        }
    }

    async fn upload(&mut self, path: &str, data: &[u8], mtime: u64, on_exists: OnExists) -> Result<Put, String> {
        let (id, answer) = self.put(path, data.len() as u64, mtime, on_exists).await;
        let offset = match answer? {
            Ok((offset, _)) => offset as usize,
            Err(skipped) => return Ok(skipped),
        };
        // As the file manager sends it: chunks of 48 KiB.
        for chunk in data[offset..].chunks(48 * 1024) {
            self.send(FileCommand::WriteData { id: id.clone(), data: Bytes::copy_from_slice(chunk) }).await;
        }
        self.put_end(&id).await
    }
}

/// Bytes that do not compress (xorshift), `n` of them.
fn noise(n: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    let mut out = Vec::with_capacity(n + 8);
    while out.len() < n {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.extend_from_slice(&x.to_le_bytes());
    }
    out.truncate(n);
    out
}

fn mtime(path: &Path) -> u64 {
    std::fs::metadata(path).unwrap().modified().unwrap().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

fn set_mtime(path: &Path, secs: u64) {
    std::fs::File::options().write(true).open(path).unwrap().set_modified(UNIX_EPOCH + Duration::from_secs(secs)).unwrap();
}

const T0: u64 = 1_700_000_000;

#[tokio::test]
async fn opens_with_the_home_folder_and_lists() {
    let home = tmp("list");
    std::fs::create_dir_all(home.join("Tài liệu/Ảnh cưới")).unwrap();
    std::fs::write(home.join("Tài liệu/Báo cáo quý 3.docx"), b"12345").unwrap();
    std::fs::write(home.join("ghi chú.txt"), b"xin chao").unwrap();
    std::fs::write(home.join(".ẩn"), b"x").unwrap();
    hide(&home.join(".ẩn"));
    set_mtime(&home.join("ghi chú.txt"), T0);
    let host = start_host(&home, HostOptions::default()).await;
    let mut c = Client::open(&host).await;

    // What the session knows of the computer: the folder the host listed
    // after the login is its home.
    let info = c.session.info.clone();
    assert_eq!(info.os, std::env::consts::OS);
    assert_eq!(info.sep, std::path::MAIN_SEPARATOR_STR);
    assert_eq!(info.home.as_deref(), Some(text(&home).as_str()));
    // Its places: "/", or the drives of a Windows computer.
    assert_eq!(info.roots, places(&home));
    assert!(info.roots.len() >= 2);

    // "" is the home folder; hidden entries only when asked for.
    let (path, parent, entries) = c.list("", false).await.unwrap();
    assert_eq!(path, text(&home));
    assert_eq!(parent, Some(text(home.parent().unwrap())));
    let mut names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    names.sort();
    assert_eq!(names, ["Tài liệu", "ghi chú.txt"]);
    let note = entries.iter().find(|e| e.name == "ghi chú.txt").unwrap();
    assert_eq!((note.dir, note.size, note.mtime, note.hidden), (false, 8, T0, false));
    assert!(entries.iter().find(|e| e.name == "Tài liệu").unwrap().dir);
    let (_, _, entries) = c.list(&text(&home), true).await.unwrap();
    assert!(entries.iter().any(|e| e.name == ".ẩn" && e.hidden));

    // A folder with a name that is not ASCII, with a slash at its end.
    let (path, parent, entries) = c.list(&format!("{}/", text(&home.join("Tài liệu"))), false).await.unwrap();
    assert_eq!(path, text(&home.join("Tài liệu")));
    assert_eq!(parent, Some(text(&home)));
    assert_eq!(entries.len(), 2);
    let (path, parent, entries) = c.list("/", false).await.unwrap();
    assert_eq!((path.as_str(), parent), ("/", None));
    // On Windows that is the list of drives, each a folder to open.
    if cfg!(windows) {
        let drives: Vec<String> = places(&home)[1..].iter().map(|place| place.name.clone()).collect();
        assert_eq!(entries.iter().map(|e| e.name.clone()).collect::<Vec<_>>(), drives);
        assert!(entries.iter().all(|e| e.dir));
        let drive = &text(&home)[..2];
        let (path, parent, _) = c.list(drive, false).await.unwrap();
        assert_eq!((path, parent), (format!("{}\\", drive), None));
    }

    // One entry; nothing by that name; a root.
    let stat = |c: &mut Client, path: String| {
        let id = c.id();
        (id.clone(), FileCommand::Stat { id, path })
    };
    let (id, command) = stat(&mut c, text(&home.join("Tài liệu/Báo cáo quý 3.docx")));
    c.send(command).await;
    assert_eq!(c.event().await, FileEvent::Stat { id, entry: Some(FileEntry { name: "Báo cáo quý 3.docx".into(), dir: false, size: 5, mtime: mtime(&home.join("Tài liệu/Báo cáo quý 3.docx")), hidden: false }) });
    let (id, command) = stat(&mut c, text(&home.join("không có")));
    c.send(command).await;
    assert_eq!(c.event().await, FileEvent::Stat { id, entry: None });
    let (id, command) = stat(&mut c, "/".into());
    c.send(command).await;
    assert!(matches!(c.event().await, FileEvent::Stat { id: of, entry: Some(FileEntry { dir: true, .. }) } if of == id));

    // The host's own words for what it cannot list.
    let error = c.list(&text(&home.join("không có")), false).await.unwrap_err();
    assert!(says_not_there(&error), "{}", error);
    // Everything under a folder.
    std::fs::write(home.join("Tài liệu/Ảnh cưới/ảnh 1.jpg"), b"jpg").unwrap();
    let id = c.id();
    c.send(FileCommand::Walk { id: id.clone(), path: text(&home.join("Tài liệu")) }).await;
    match c.event().await {
        FileEvent::Walk { id: of, path, mut files, dirs } => {
            assert_eq!((of, path), (id, text(&home.join("Tài liệu"))));
            files.sort_by(|a, b| a.rel.cmp(&b.rel));
            assert_eq!(files.iter().map(|f| (f.rel.as_str(), f.size)).collect::<Vec<_>>(), [("Báo cáo quý 3.docx", 5), ("Ảnh cưới/ảnh 1.jpg", 3)]);
            assert_eq!(dirs, ["Ảnh cưới"]);
        }
        other => panic!("{:?}", other),
    }
    let id = c.id();
    c.send(FileCommand::Walk { id: id.clone(), path: text(&home.join("ghi chú.txt")) }).await;
    assert!(matches!(c.event().await, FileEvent::Error { id: of, message } if of == id && message.contains("is not a folder")));
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test]
async fn downloads_files_whole_and_from_an_offset() {
    let home = tmp("down");
    // Text (sent compressed), noise (as it is), a name the host does not
    // compress, an empty file, and one larger than a block.
    let words = "Con đường dài nhất là con đường chưa đi. ".repeat(20_000).into_bytes();
    let random = noise(300_000, 7);
    std::fs::write(home.join("văn bản.txt"), &words).unwrap();
    std::fs::write(home.join("nhiễu.bin"), &random).unwrap();
    std::fs::write(home.join("ảnh.jpg"), &words[..200_000]).unwrap();
    std::fs::write(home.join("rỗng"), b"").unwrap();
    std::fs::create_dir(home.join("thư mục")).unwrap();
    set_mtime(&home.join("văn bản.txt"), T0);
    let host = start_host(&home, HostOptions::default()).await;
    let mut c = Client::open(&host).await;

    let mut got = Vec::new();
    let (size, time, n) = c.download_to(&text(&home.join("văn bản.txt")), 0, |chunk| got.extend_from_slice(chunk)).await.unwrap();
    assert_eq!((size, time, n), (words.len() as u64, T0, words.len() as u64));
    assert!(got == words);
    assert!(c.download(&text(&home.join("nhiễu.bin")), 0).await.unwrap() == random);
    assert!(c.download(&text(&home.join("ảnh.jpg")), 0).await.unwrap() == words[..200_000]);
    assert!(c.download(&text(&home.join("rỗng")), 0).await.unwrap().is_empty());

    // From where the viewer has it: the host goes on from there.
    assert!(c.download(&text(&home.join("nhiễu.bin")), 123_457).await.unwrap() == random[123_457..]);
    assert!(c.download(&text(&home.join("văn bản.txt")), 500_001).await.unwrap() == words[500_001..]);
    // All of it is there already; more than there is.
    assert!(c.download(&text(&home.join("nhiễu.bin")), 300_000).await.unwrap().is_empty());
    assert!(c.download(&text(&home.join("nhiễu.bin")), 999_999).await.unwrap().is_empty());

    // What is not a file, and what is not there: the words reach the caller.
    assert_eq!(c.download(&text(&home.join("thư mục")), 0).await.unwrap_err(), "That is not a file");
    assert_eq!(c.download(&text(&home.join("không có")), 0).await.unwrap_err(), "Not exists");
    // And the session goes on.
    assert!(c.download(&text(&home.join("nhiễu.bin")), 0).await.unwrap() == random);

    // A host from before resuming (1.4.2) sends the file from its start:
    // the part the viewer has is dropped here.
    let old = start_host(&home, HostOptions { version: "1.3.9".into(), ..Default::default() }).await;
    let mut c = Client::open(&old).await;
    assert!(c.download(&text(&home.join("nhiễu.bin")), 200_001).await.unwrap() == random[200_001..]);
    assert!(c.download(&text(&home.join("văn bản.txt")), 131_072).await.unwrap() == words[131_072..]);
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test]
async fn uploads_files_and_answers_for_a_file_that_is_there() {
    let home = tmp("up");
    let host = start_host(&home, HostOptions::default()).await;
    let mut c = Client::open(&host).await;
    let words = "Trăm năm trong cõi người ta. ".repeat(30_000).into_bytes();
    let random = noise(400_000, 11);

    // New files: the data, the time, no leftovers.
    let target = home.join("Tải lên/thơ Kiều.txt");
    std::fs::create_dir(home.join("Tải lên")).unwrap();
    assert_eq!(c.upload(&text(&target), &words, T0, OnExists::Rename).await.unwrap(), Put::Done(text(&target)));
    assert!(std::fs::read(&target).unwrap() == words);
    assert_eq!(mtime(&target), T0);
    assert_eq!(c.upload(&text(&home.join("nhiễu.bin")), &random, T0, OnExists::Rename).await.unwrap(), Put::Done(text(&home.join("nhiễu.bin"))));
    assert!(std::fs::read(home.join("nhiễu.bin")).unwrap() == random);
    assert_eq!(c.upload(&text(&home.join("rỗng.txt")), b"", T0, OnExists::Rename).await.unwrap(), Put::Done(text(&home.join("rỗng.txt"))));
    assert_eq!(std::fs::metadata(home.join("rỗng.txt")).unwrap().len(), 0);
    assert_eq!(c.names(&home).await, ["Tải lên", "nhiễu.bin", "rỗng.txt"]);

    // The same file again (size and time): not sent, whatever was asked.
    for on_exists in [OnExists::Rename, OnExists::Replace, OnExists::Skip] {
        assert_eq!(c.upload(&text(&target), &words, T0, on_exists).await.unwrap(), Put::Skipped("identical".into()));
    }
    // Another file of that name: left alone, ...
    let other = b"mot noi dung khac".to_vec();
    assert_eq!(c.upload(&text(&target), &other, T0 + 60, OnExists::Skip).await.unwrap(), Put::Skipped("exists".into()));
    assert!(std::fs::read(&target).unwrap() == words);
    // ... kept beside it under another name (twice: the next name), ...
    assert_eq!(c.upload(&text(&target), &other, T0 + 60, OnExists::Rename).await.unwrap(), Put::Done(text(&home.join("Tải lên/thơ Kiều (2).txt"))));
    assert_eq!(c.upload(&text(&target), b"lan ba", T0 + 90, OnExists::Rename).await.unwrap(), Put::Done(text(&home.join("Tải lên/thơ Kiều (3).txt"))));
    assert!(std::fs::read(&target).unwrap() == words);
    assert!(std::fs::read(home.join("Tải lên/thơ Kiều (2).txt")).unwrap() == other);
    assert_eq!(c.names(&home.join("Tải lên")).await, ["thơ Kiều (2).txt", "thơ Kiều (3).txt", "thơ Kiều.txt"]);
    // ... or replaced.
    assert_eq!(c.upload(&text(&target), &other, T0 + 60, OnExists::Replace).await.unwrap(), Put::Done(text(&target)));
    assert!(std::fs::read(&target).unwrap() == other);
    assert_eq!(mtime(&target), T0 + 60);

    // More data than announced, and less: refused, in words.
    let (id, answer) = c.put(&text(&home.join("dài.bin")), 10, T0, OnExists::Replace).await;
    assert_eq!(answer.unwrap().unwrap().0, 0);
    c.send(FileCommand::WriteData { id: id.clone(), data: Bytes::from_static(b"12345678901") }).await;
    assert!(matches!(c.event().await, FileEvent::Error { id: of, message } if of == id && message.contains("more data than the announced 10 bytes")));
    let (id, _) = c.put(&text(&home.join("ngắn.bin")), 10, T0, OnExists::Replace).await;
    c.send(FileCommand::WriteData { id: id.clone(), data: Bytes::from_static(b"123") }).await;
    assert!(c.put_end(&id).await.unwrap_err().contains("incomplete: 3 of 10 bytes"));
    assert!(!home.join("dài.bin").exists() && !home.join("ngắn.bin").exists());
    // Where the host cannot write: its words.
    let error = c.upload(&text(&home.join("nhiễu.bin/x.txt")), b"x", T0, OnExists::Replace).await.unwrap_err();
    assert!(says_not_a_folder(&error), "{}", error);
    assert!(c.upload("/", b"x", T0, OnExists::Replace).await.unwrap_err().contains("cannot be changed from here"));
    // The session goes on after all of that.
    assert_eq!(c.upload(&text(&home.join("cuối.txt")), b"xong", T0, OnExists::Rename).await.unwrap(), Put::Done(text(&home.join("cuối.txt"))));
    let _ = std::fs::remove_dir_all(&home);
}

/// xxh3 of a file, read in pieces.
fn hash_of(path: &Path) -> u64 {
    let mut hasher = xxhash_rust::xxh3::Xxh3::new();
    let mut file = std::fs::File::open(path).unwrap();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf).unwrap();
        if n == 0 {
            return hasher.digest();
        }
        hasher.update(&buf[..n]);
    }
}

/// Removes the folder with the large files however the test ends.
struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_large_file_both_ways() {
    const SIZE: usize = 200 * 1024 * 1024;
    let home = tmp("large");
    let _cleanup = Cleanup(home.clone());
    // Half of it compresses and half does not, megabyte by megabyte.
    let source = home.join("lớn.bin");
    {
        let mut file = std::fs::File::create(&source).unwrap();
        let words = "Lunaris chuyển tệp qua RustDesk. ".repeat(40_000).into_bytes();
        for i in 0..SIZE / (1 << 20) {
            if i % 2 == 0 {
                file.write_all(&noise(1 << 20, i as u64 + 1)).unwrap();
            } else {
                file.write_all(&words[..1 << 20]).unwrap();
            }
        }
    }
    set_mtime(&source, T0);
    let wanted = hash_of(&source);
    let host = start_host(&home, HostOptions::default()).await;
    let mut c = Client::open(&host).await;

    // Down: to a file here, as the viewer would write it.
    let copy = home.join("bản sao.bin");
    let mut out = std::fs::File::create(&copy).unwrap();
    let started = Instant::now();
    let (size, _, got) = c.download_to(&text(&source), 0, |chunk| out.write_all(chunk).unwrap()).await.unwrap();
    let down = started.elapsed();
    drop(out);
    assert_eq!((size, got), (SIZE as u64, SIZE as u64));
    assert_eq!(hash_of(&copy), wanted);
    let host_kind = if REAL_HOST.with(|real| real.get()) { "the agent's listener" } else { "the test host" };
    println!("{}: download: {} MB in {:.2} s = {:.0} MB/s", host_kind, SIZE >> 20, down.as_secs_f64(), (SIZE >> 20) as f64 / down.as_secs_f64());

    // Up: from that file, in the file manager's chunks.
    let uploaded = home.join("tải lên.bin");
    let started = Instant::now();
    let (id, answer) = c.put(&text(&uploaded), SIZE as u64, T0, OnExists::Replace).await;
    assert_eq!(answer.unwrap().unwrap().0, 0);
    let mut file = std::fs::File::open(&copy).unwrap();
    let mut buf = vec![0u8; 48 * 1024];
    loop {
        let n = file.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        c.send(FileCommand::WriteData { id: id.clone(), data: Bytes::copy_from_slice(&buf[..n]) }).await;
    }
    assert_eq!(c.put_end(&id).await.unwrap(), Put::Done(text(&uploaded)));
    let up = started.elapsed();
    assert_eq!(std::fs::metadata(&uploaded).unwrap().len(), SIZE as u64);
    assert_eq!(hash_of(&uploaded), wanted);
    assert_eq!(mtime(&uploaded), T0);
    println!("{}: upload: {} MB in {:.2} s = {:.0} MB/s", host_kind, SIZE >> 20, up.as_secs_f64(), (SIZE >> 20) as f64 / up.as_secs_f64());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfers_resume_after_the_connection_was_cut() {
    let home = tmp("resume");
    let host = start_host(&home, HostOptions::default()).await;
    let data = noise(8 * 1024 * 1024, 3);

    // An upload cut after about 3 MB went to the host.
    let target = home.join("dở dang.bin");
    let through = cutting_proxy(host.address, None, Some(3 * 1024 * 1024)).await;
    let mut c = Client::open_at(through, &host.password).await.unwrap();
    let (id, answer) = c.put(&text(&target), data.len() as u64, T0, OnExists::Rename).await;
    assert_eq!(answer.unwrap().unwrap(), (0, text(&target)));
    for chunk in data.chunks(48 * 1024) {
        if c.session.commands.send(FileCommand::WriteData { id: id.clone(), data: Bytes::copy_from_slice(chunk) }).await.is_err() {
            break;
        }
    }
    // The session says that it ended, in words.
    loop {
        match tokio::time::timeout(Duration::from_secs(20), c.session.events.recv()).await.unwrap() {
            Some(FileEvent::Closed { reason }) => {
                assert!(reason.contains("connection") || reason.contains("closed"), "{}", reason);
                break;
            }
            Some(_) => {}
            None => break,
        }
    }
    // The host has a part of it, not the file.
    let part = PathBuf::from(format!("{}.download", text(&target)));
    assert!(!target.exists());
    let mut have = 0;
    for _ in 0..200 {
        have = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
        if have > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(have > 0 && have < data.len() as u64, "{}", have);
    // A new session: the host says how much it has, and the rest completes it.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let have = std::fs::metadata(&part).unwrap().len();
    let mut c = Client::open(&host).await;
    let (id, answer) = c.put(&text(&target), data.len() as u64, T0, OnExists::Rename).await;
    assert_eq!(answer.unwrap().unwrap(), (have, text(&target)));
    for chunk in data[have as usize..].chunks(48 * 1024) {
        c.send(FileCommand::WriteData { id: id.clone(), data: Bytes::copy_from_slice(chunk) }).await;
    }
    assert_eq!(c.put_end(&id).await.unwrap(), Put::Done(text(&target)));
    assert!(std::fs::read(&target).unwrap() == data);
    assert_eq!(mtime(&target), T0);
    assert!(!part.exists() && !PathBuf::from(format!("{}.digest", text(&target))).exists());
    // Another file under that name does not go on from a part of this one.
    std::fs::remove_file(&target).unwrap();
    std::fs::write(&part, &data[..1000]).unwrap();
    std::fs::write(format!("{}.digest", text(&target)), serde_json::json!({"size": data.len(), "modified": T0}).to_string()).unwrap();
    let (_, answer) = c.put(&text(&target), 5, T0 + 1, OnExists::Rename).await;
    assert_eq!(answer.unwrap().unwrap(), (0, text(&target)));

    // A download cut after about 3 MB came.
    let source = home.join("nguồn.bin");
    std::fs::write(&source, &data).unwrap();
    let through = cutting_proxy(host.address, Some(3 * 1024 * 1024), None).await;
    let mut c = Client::open_at(through, &host.password).await.unwrap();
    let id = c.id();
    c.send(FileCommand::Read { id: id.clone(), path: text(&source), offset: 0 }).await;
    let mut got = Vec::new();
    loop {
        match c.event().await {
            FileEvent::ReadStart { .. } => {}
            FileEvent::ReadData { data, .. } => got.extend_from_slice(&data),
            FileEvent::Closed { .. } => break,
            other => panic!("{:?}", other),
        }
    }
    assert!(!got.is_empty() && got.len() < data.len(), "{}", got.len());
    assert!(got == data[..got.len()]);
    // A new session, from where it stopped.
    let mut c = Client::open(&host).await;
    let rest = c.download(&text(&source), got.len() as u64).await.unwrap();
    got.extend_from_slice(&rest);
    assert!(got == data);
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_transfer_is_cancelled_in_the_middle() {
    let home = tmp("cancel");
    let data = noise(48 * 1024 * 1024, 5);
    std::fs::write(home.join("lớn.bin"), &data).unwrap();
    std::fs::write(home.join("nhỏ.txt"), b"sau khi huy").unwrap();
    let host = start_host(&home, HostOptions::default()).await;
    let mut c = Client::open(&host).await;

    // A download: after a few chunks the viewer has had enough.
    let id = c.id();
    c.send(FileCommand::Read { id: id.clone(), path: text(&home.join("lớn.bin")), offset: 0 }).await;
    let (mut got, mut asked) = (0usize, false);
    loop {
        match c.event().await {
            FileEvent::ReadStart { size, .. } => assert_eq!(size, data.len() as u64),
            FileEvent::ReadData { data: chunk, .. } => {
                assert!(chunk[..] == data[got..got + chunk.len()]);
                got += chunk.len();
                if got >= 1024 * 1024 && !asked {
                    asked = true;
                    c.send(FileCommand::Cancel { id: id.clone() }).await;
                }
            }
            FileEvent::ReadEnd { id: of, cancelled } => {
                assert_eq!((of, cancelled), (id.clone(), true));
                break;
            }
            other => panic!("{:?}", other),
        }
    }
    assert!(got < data.len(), "the download was stopped before its end ({} of {})", got, data.len());
    // Nothing of it follows: the next answers are the next requests' own.
    assert!(c.download(&text(&home.join("nhỏ.txt")), 0).await.unwrap() == b"sau khi huy");
    assert_eq!(c.names(&home).await, ["lớn.bin", "nhỏ.txt"]);

    // An upload: the host drops what it had of it.
    let target = home.join("bỏ dở.bin");
    let (id, answer) = c.put(&text(&target), data.len() as u64, T0, OnExists::Rename).await;
    assert_eq!(answer.unwrap().unwrap().0, 0);
    for chunk in data[..4 * 1024 * 1024].chunks(48 * 1024) {
        c.send(FileCommand::WriteData { id: id.clone(), data: Bytes::copy_from_slice(chunk) }).await;
    }
    c.send(FileCommand::Cancel { id: id.clone() }).await;
    // Data that was on its way when it was cancelled is not an error.
    c.send(FileCommand::WriteData { id: id.clone(), data: Bytes::from_static(b"muon") }).await;
    assert_eq!(c.names(&home).await, ["lớn.bin", "nhỏ.txt"]);
    assert_eq!(c.upload(&text(&target), b"lai tu dau", T0, OnExists::Rename).await.unwrap(), Put::Done(text(&target)));
    assert!(std::fs::read(&target).unwrap() == b"lai tu dau");
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test]
async fn makes_renames_and_removes() {
    let home = tmp("ops");
    let outside = tmp("ops-outside");
    std::fs::write(outside.join("giữ lại.txt"), b"khong xoa").unwrap();
    let host = start_host(&home, HostOptions::default()).await;
    let mut c = Client::open(&host).await;
    let at = |name: &str| text(&home.join(name));

    // New folders, the ones above them too.
    c.act(|id| FileCommand::Mkdir { id, path: at("Dự án/2026/Bản vẽ") }).await.unwrap();
    assert!(home.join("Dự án/2026/Bản vẽ").is_dir());
    std::fs::write(home.join("Dự án/2026/Bản vẽ/mặt bằng.dwg"), b"1").unwrap();
    std::fs::write(home.join("Dự án/2026/.ẩn"), b"2").unwrap();
    std::fs::write(home.join("Dự án/ghi chú.txt"), b"3").unwrap();
    std::fs::create_dir(home.join("Dự án/trống")).unwrap();

    // Rename: a file and a folder; not onto a name that is taken, not a
    // name that is a path, not what is not there.
    c.act(|id| FileCommand::Rename { id, path: at("Dự án/ghi chú.txt"), name: "Ghi chú mới.txt".into() }).await.unwrap();
    assert!(home.join("Dự án/Ghi chú mới.txt").exists() && !home.join("Dự án/ghi chú.txt").exists());
    c.act(|id| FileCommand::Rename { id, path: at("Dự án/2026"), name: "Năm 2026".into() }).await.unwrap();
    assert!(home.join("Dự án/Năm 2026/Bản vẽ/mặt bằng.dwg").exists());
    std::fs::write(home.join("Dự án/khác.txt"), b"4").unwrap();
    let error = c.act(|id| FileCommand::Rename { id, path: at("Dự án/khác.txt"), name: "Ghi chú mới.txt".into() }).await.unwrap_err();
    assert_eq!(error, "Ghi chú mới.txt already exists");
    assert_eq!(std::fs::read(home.join("Dự án/Ghi chú mới.txt")).unwrap(), b"3");
    for name in ["../ra ngoài.txt", "a/b", "..", ""] {
        let error = c.act(|id| FileCommand::Rename { id, path: at("Dự án/khác.txt"), name: name.into() }).await.unwrap_err();
        assert_eq!(error, "That is not a name for a file", "{:?}", name);
    }
    let error = c.act(|id| FileCommand::Rename { id, path: at("Dự án/không có"), name: "x".into() }).await.unwrap_err();
    assert_eq!(error, "không có is not there");
    assert!(home.join("Dự án/khác.txt").exists());

    // Remove: a file; a link to a folder (the link goes, not what it leads
    // to); a folder with all that is in it; what is not there; a root.
    c.act(|id| FileCommand::Remove { id, path: at("Dự án/khác.txt") }).await.unwrap();
    assert!(!home.join("Dự án/khác.txt").exists());
    link_dir(&outside, &home.join("Dự án/lối tắt"));
    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.join("giữ lại.txt"), home.join("Dự án/tệp tắt")).unwrap();
    c.act(|id| FileCommand::Remove { id, path: at("Dự án/lối tắt") }).await.unwrap();
    assert!(std::fs::symlink_metadata(home.join("Dự án/lối tắt")).is_err());
    assert!(outside.join("giữ lại.txt").exists(), "what the link led to is still there");
    // Links inside a folder that is removed are removed, not followed.
    link_dir(&outside, &home.join("Dự án/Năm 2026/lối tắt"));
    c.act(|id| FileCommand::Remove { id, path: at("Dự án") }).await.unwrap();
    assert!(!home.join("Dự án").exists());
    assert!(outside.join("giữ lại.txt").exists());
    assert!(c.names(&home).await.is_empty());
    let error = c.act(|id| FileCommand::Remove { id, path: at("không có") }).await.unwrap_err();
    assert_eq!(error, "không có is not there");
    let error = c.act(|id| FileCommand::Remove { id, path: "/".into() }).await.unwrap_err();
    assert!(error.contains("cannot be changed from here"), "{}", error);
    // An empty folder.
    c.act(|id| FileCommand::Mkdir { id, path: at("trống") }).await.unwrap();
    c.act(|id| FileCommand::Remove { id, path: at("trống") }).await.unwrap();
    assert!(!home.join("trống").exists());
    // The host's words when it cannot: a folder under a file.
    std::fs::write(home.join("tệp"), b"x").unwrap();
    let error = c.act(|id| FileCommand::Mkdir { id, path: at("tệp/bên trong") }).await.unwrap_err();
    assert!(says_not_a_folder(&error), "{}", error);

    // A host too old to rename says so in words, without being asked.
    let old = start_host(&home, HostOptions { version: "1.2.7".into(), ..Default::default() }).await;
    let mut c = Client::open(&old).await;
    let error = c.act(|id| FileCommand::Rename { id, path: at("tệp"), name: "x".into() }).await.unwrap_err();
    assert!(error.contains("too old to rename"), "{}", error);
    assert!(home.join("tệp").exists());
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&outside);
}

#[tokio::test]
async fn a_host_that_refuses_says_why() {
    let home = tmp("refuse");
    let host = start_host(&home, HostOptions::default()).await;
    let error = |result: anyhow::Result<Client>| result.err().expect("refused").to_string();
    assert_eq!(error(Client::open_at(host.address, "sai").await), "Wrong password for the RustDesk computer");
    let closed = start_host(&home, HostOptions { file_permission: false, ..Default::default() }).await;
    let said = error(Client::open_at(closed.address, &closed.password).await);
    assert!(said.contains("File transfer is switched off on the RustDesk computer"), "{}", said);
    // Nothing listens there.
    let gone = {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        listener.local_addr().unwrap()
    };
    assert!(error(Client::open_at(gone, "x").await).contains("cannot be reached"));
    // A session that works ends with a word when the host goes.
    let mut c = Client::open(&host).await;
    c.send(FileCommand::Close).await;
    assert_eq!(c.event().await, FileEvent::Closed { reason: "Closed".into() });
    let _ = std::fs::remove_dir_all(&home);
}

// ------------------------------------------------- Windows paths, as text

/// The client's state for a Windows host, without a connection: what it
/// would send, and what it tells the agent.
fn windows_files() -> (Files, mpsc::Receiver<FileEvent>) {
    let (tx, rx) = mpsc::channel(64);
    (Files::new(PathForm { windows: true }, "1.5.3", tx), rx)
}

fn sent(files: &mut Files) -> Vec<file_action::Union> {
    files
        .out
        .drain(..)
        .filter_map(|m| match m.union {
            Some(message::Union::FileAction(FileAction { union })) => union,
            _ => None,
        })
        .collect()
}

fn entry(name: &str, kind: FileType) -> proto::FileEntry {
    proto::FileEntry { entry_type: kind as i32, name: name.into(), ..Default::default() }
}

async fn host_says(files: &mut Files, union: file_response::Union) {
    assert!(matches!(files.on_message(response(union)).await, Flow::Continue));
}

#[tokio::test]
async fn windows_paths_go_to_the_host_in_its_form() {
    let (mut f, mut events) = windows_files();
    // Listing: slashes either way, a slash at the end, a drive from the list of drives.
    for (asked, wanted) in [("C:/Users/An/", r"C:\Users\An"), ("/C:", "C:\\"), ("D:", "D:\\"), ("/", "/"), ("", "")] {
        f.on_command(FileCommand::List { id: "l".into(), path: asked.into(), hidden: false }).await;
        assert!(matches!(&sent(&mut f)[..], [file_action::Union::ReadDir(rd)] if rd.path == wanted), "{}", asked);
    }
    // Their answers, in the order asked: the folder above, and where there is none.
    host_says(&mut f, file_response::Union::Dir(FileDirectory { id: 0, path: r"C:\Users\An".into(), entries: vec![entry("Tài liệu", FileType::Dir), entry("ảnh.png", FileType::File)] })).await;
    assert_eq!(
        events.recv().await.unwrap(),
        FileEvent::Dir {
            id: "l".into(),
            path: r"C:\Users\An".into(),
            parent: Some(r"C:\Users".into()),
            entries: vec![
                FileEntry { name: "Tài liệu".into(), dir: true, size: 0, mtime: 0, hidden: false },
                FileEntry { name: "ảnh.png".into(), dir: false, size: 0, mtime: 0, hidden: false }
            ],
        }
    );
    host_says(&mut f, file_response::Union::Dir(FileDirectory { id: 0, path: "C:\\".into(), entries: vec![] })).await;
    assert!(matches!(events.recv().await.unwrap(), FileEvent::Dir { path, parent: None, .. } if path == "C:\\"));
    host_says(&mut f, file_response::Union::Dir(FileDirectory { id: 0, path: "D:\\".into(), entries: vec![] })).await;
    events.recv().await.unwrap();
    // The list of drives: each is a folder.
    host_says(&mut f, file_response::Union::Dir(FileDirectory { id: 0, path: "/".into(), entries: vec![entry("C:", FileType::DirDrive), entry("D:", FileType::DirDrive)] })).await;
    assert!(matches!(events.recv().await.unwrap(), FileEvent::Dir { path, parent: None, entries, .. } if path == "/" && entries.len() == 2 && entries[0].dir && entries[0].name == "C:"));
    // The home folder, as the host names it.
    host_says(&mut f, file_response::Union::Dir(FileDirectory { id: 0, path: r"C:\Users\An".into(), entries: vec![] })).await;
    assert!(matches!(events.recv().await.unwrap(), FileEvent::Dir { path, .. } if path == r"C:\Users\An"));
    // A listing whose names are paths is not shown.
    f.on_command(FileCommand::List { id: "x".into(), path: "C:/".into(), hidden: false }).await;
    sent(&mut f);
    host_says(&mut f, file_response::Union::Dir(FileDirectory { id: 0, path: "C:\\".into(), entries: vec![entry(r"..\Windows", FileType::Dir)] })).await;
    assert!(matches!(events.recv().await.unwrap(), FileEvent::Error { id, .. } if id == "x"));

    // Transfers and folders: the path with backslashes.
    f.on_command(FileCommand::Read { id: "r".into(), path: "C:/Users/An/ảnh.png".into(), offset: 0 }).await;
    assert!(matches!(&sent(&mut f)[..], [file_action::Union::Send(s)] if s.path == r"C:\Users\An\ảnh.png" && s.id == 1));
    f.on_command(FileCommand::Cancel { id: "r".into() }).await;
    assert_eq!(events.recv().await.unwrap(), FileEvent::ReadEnd { id: "r".into(), cancelled: true });
    assert!(matches!(&sent(&mut f)[..], [file_action::Union::Cancel(c)] if c.id == 1));
    f.on_command(FileCommand::Mkdir { id: "m".into(), path: r"C:\Users\An/Mới".into() }).await;
    assert!(matches!(&sent(&mut f)[..], [file_action::Union::Create(c)] if c.path == r"C:\Users\An\Mới"));
    f.on_command(FileCommand::Write { id: "w".into(), path: r"C:\Users\An\báo cáo.docx".into(), size: 3, mtime: T0, on_exists: OnExists::Rename }).await;
    let job = match &sent(&mut f)[..] {
        [file_action::Union::Receive(r)] => {
            assert_eq!((r.path.as_str(), r.files.len(), r.files[0].name.as_str(), r.files[0].size, r.total_size), (r"C:\Users\An\báo cáo.docx", 1, "", 3, 3));
            r.id
        }
        other => panic!("{:?}", other),
    };
    // It is there, as another file: offered again under the next name, in the same folder.
    host_says(&mut f, file_response::Union::Digest(FileTransferDigest { id: job, is_upload: true, file_size: 9, ..Default::default() })).await;
    match &sent(&mut f)[..] {
        [file_action::Union::SendConfirm(skip), file_action::Union::Receive(r)] => {
            assert_eq!((skip.id, skip.union), (job, Some(confirm::Union::Skip(true))));
            assert_eq!(r.path, r"C:\Users\An\báo cáo (2).docx");
        }
        other => panic!("{:?}", other),
    }

    // Removing a folder: its folder is listed, its files asked for, each
    // removed by its path as the host wrote it, then the folder.
    f.on_command(FileCommand::Remove { id: "d".into(), path: "C:/Users/An/Cũ".into() }).await;
    assert!(matches!(&sent(&mut f)[..], [file_action::Union::ReadDir(rd)] if rd.path == r"C:\Users\An" && rd.include_hidden));
    host_says(&mut f, file_response::Union::Dir(FileDirectory { id: 0, path: r"C:\Users\An".into(), entries: vec![entry("cũ", FileType::Dir)] })).await;
    let job = match &sent(&mut f)[..] {
        [file_action::Union::AllFiles(all)] => {
            assert_eq!(all.path, r"C:\Users\An\Cũ");
            all.id
        }
        other => panic!("{:?}", other),
    };
    host_says(&mut f, file_response::Union::Dir(FileDirectory { id: job, path: r"C:\Users\An\Cũ".into(), entries: vec![entry("a.txt", FileType::File), entry(r"con\b.txt", FileType::File)] })).await;
    assert!(matches!(&sent(&mut f)[..], [file_action::Union::RemoveFile(rf)] if rf.path == r"C:\Users\An\Cũ\a.txt" && rf.id == job));
    host_says(&mut f, file_response::Union::Done(proto::FileTransferDone { id: job, file_num: 0 })).await;
    assert!(matches!(&sent(&mut f)[..], [file_action::Union::RemoveFile(rf)] if rf.path == r"C:\Users\An\Cũ\con\b.txt" && rf.file_num == 1));
    host_says(&mut f, file_response::Union::Done(proto::FileTransferDone { id: job, file_num: 1 })).await;
    assert!(matches!(&sent(&mut f)[..], [file_action::Union::RemoveDir(rd)] if rd.path == r"C:\Users\An\Cũ" && rd.recursive));
    host_says(&mut f, file_response::Union::Done(proto::FileTransferDone { id: job, file_num: 0 })).await;
    assert_eq!(events.recv().await.unwrap(), FileEvent::Ok { id: "d".into() });
    // A link to a folder is removed as a folder there, itself only.
    f.on_command(FileCommand::Remove { id: "j".into(), path: r"C:\Users\An\Lối tắt".into() }).await;
    sent(&mut f);
    host_says(&mut f, file_response::Union::Dir(FileDirectory { id: 0, path: r"C:\Users\An".into(), entries: vec![entry("Lối tắt", FileType::DirLink)] })).await;
    assert!(matches!(&sent(&mut f)[..], [file_action::Union::RemoveDir(rd)] if rd.path == r"C:\Users\An\Lối tắt" && !rd.recursive));
    // A drive is not an entry to remove or rename.
    f.on_command(FileCommand::Remove { id: "c".into(), path: "C:\\".into() }).await;
    assert!(matches!(events.recv().await.unwrap(), FileEvent::Error { id, .. } if id == "c"));
    assert!(sent(&mut f).is_empty());

    // Everything under a folder: the host's backslashes become slashes;
    // names that lead out of the folder are refused.
    f.on_command(FileCommand::Walk { id: "k".into(), path: r"C:\Users\An\Tài liệu".into() }).await;
    let job = match &sent(&mut f)[..] {
        [file_action::Union::AllFiles(all)] => all.id,
        other => panic!("{:?}", other),
    };
    host_says(&mut f, file_response::Union::Dir(FileDirectory { id: job, path: String::new(), entries: vec![entry(r"Ảnh\2026\a.jpg", FileType::File), entry("b.txt", FileType::File)] })).await;
    match events.recv().await.unwrap() {
        FileEvent::Walk { files, dirs, .. } => {
            assert_eq!(files.iter().map(|f| f.rel.as_str()).collect::<Vec<_>>(), ["Ảnh/2026/a.jpg", "b.txt"]);
            assert_eq!(dirs, ["Ảnh", "Ảnh/2026"]);
        }
        other => panic!("{:?}", other),
    }
    for bad in [r"..\..\Windows\x.dll", r"C:\x", r"\x"] {
        f.on_command(FileCommand::Walk { id: "k".into(), path: r"C:\Users".into() }).await;
        let job = match &sent(&mut f)[..] {
            [file_action::Union::AllFiles(all)] => all.id,
            other => panic!("{:?}", other),
        };
        host_says(&mut f, file_response::Union::Dir(FileDirectory { id: job, path: String::new(), entries: vec![entry(bad, FileType::File)] })).await;
        assert!(matches!(events.recv().await.unwrap(), FileEvent::Error { .. }), "{}", bad);
    }
}

#[test]
fn blocks_are_compressed_as_hbb_common_does() {
    let words = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".repeat(1000).into_bytes();
    let block = block_of(3, 0, &words, true);
    assert!(block.compressed && block.data.len() < words.len());
    assert_eq!(data_of(block).unwrap(), words);
    // What does not get smaller goes as it is; so do the files RustDesk does not try.
    let random = noise(4096, 9);
    let block = block_of(3, 0, &random, true);
    assert!(!block.compressed && block.data == random);
    assert!(!block_of(3, 0, &words, false).compressed);
    assert!(is_compressed_file("ảnh.jpg") && is_compressed_file("a.tar.gz") && !is_compressed_file("a.JPG") && !is_compressed_file("gz"));
    // A block that says it is compressed and is not: no data, not garbage.
    assert!(data_of(FileTransferBlock { compressed: true, data: Bytes::from_static(b"khong phai zstd"), ..Default::default() }).is_none());
    // The end of a file.
    let end = block_of(3, 0, &[], true);
    assert!(!end.compressed && end.data.is_empty());
}

#[test]
fn the_login_is_a_file_transfer_login() {
    let hash = proto::Hash { salt: "s".into(), challenge: "c".into() };
    let Some(message::Union::LoginRequest(request)) = file_login_message("192.0.2.7:21118", "bí mật", &hash).union else { panic!("no login") };
    assert_eq!(request.union, Some(login_request::Union::FileTransfer(proto::FileTransfer { dir: String::new(), show_hidden: true })));
    assert!(request.option.is_none());
    assert!(login::verify("bí mật", "s", "c", &request.password));
    assert_eq!(file_login_error(login::LOGIN_MSG_PASSWORD_WRONG), "Wrong password for the RustDesk computer");
    assert!(file_login_error(NO_FILE_PERMISSION).contains("switched off"));
    assert_eq!(file_login_error("Something else"), "Something else");
}

/// Every test above that needs a host, with the agent's RustDesk listener
/// as the host ([`start_host`]): the connector's client and the listener's
/// file host, both sides at once.
mod against_the_listener {
    use super::REAL_HOST;

    macro_rules! with_the_listener {
        ($($name:ident),* $(,)?) => {
            $(
                #[test]
                fn $name() {
                    REAL_HOST.with(|real| real.set(true));
                    super::$name();
                }
            )*
        };
    }

    with_the_listener!(
        opens_with_the_home_folder_and_lists,
        downloads_files_whole_and_from_an_offset,
        uploads_files_and_answers_for_a_file_that_is_there,
        a_large_file_both_ways,
        transfers_resume_after_the_connection_was_cut,
        a_transfer_is_cancelled_in_the_middle,
        makes_renames_and_removes,
        a_host_that_refuses_says_why,
    );
}
