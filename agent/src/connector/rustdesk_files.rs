//! The files of a RustDesk computer: the client's side of RustDesk's file
//! transfer, for the Lunaris file manager (`connector/files.rs`).
//!
//! Ported from RustDesk 1.5.3 (AGPL-3.0, https://github.com/rustdesk/rustdesk):
//! `src/client.rs` (`create_login_msg`), `src/client/io_loop.rs` (the file
//! parts of `handle_msg_from_ui` and `handle_msg_from_peer`),
//! `src/client/file_trait.rs`, `libs/base/src/fs.rs` and `fs/download.rs`
//! (`TransferJob`), and what the Flutter file manager does with them
//! (`flutter/lib/models/file_model.dart`). Each step names what it follows.
//!
//! A file-transfer connection is a connection of its own, logged in to with
//! `LoginRequest.file_transfer` set; on it the client sends `FileAction`s
//! and the host answers with `FileResponse`s (and, for an upload, the other
//! way round for the data).
//!
//! One thing differs from RustDesk's client by what this is: the files are
//! not on this computer. A download's blocks go on to the viewer and an
//! upload's data comes from it, so where RustDesk's client looks at its own
//! disk (is the file there already, how much of it), the viewer says so:
//! the offset it asks a download from, and what to do when the file to
//! write is there (`OnExists`).

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use prost::Message as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::files::{self, FileCommand, FileEntry, FileEvent, FileSession, FilesInfo, OnExists, PathForm, Place, WalkFile};
use super::rustdesk::{login_error, login_message, platform_of, Conn, CONNECT_TIMEOUT, DEFAULT_PORT, READ_TIMEOUT};
use super::{Target, Viewer};
use crate::rustdesk::clipboard::get_version_number;
use crate::rustdesk::proto::{
    self, file_action, file_response, file_transfer_send_confirm_request as confirm, login_request, login_response, message, misc,
    FileAction, FileDirectory, FileResponse, FileTransferBlock, FileTransferDigest, FileTransferSendConfirmRequest, FileType,
    Message, Misc, PeerInfo,
};
use crate::rustdesk::{cursor, frame};

/// libs/base/src/fs.rs `TransferJob::read`: `BUF_SIZE`, the size of a block.
const BLOCK: usize = 128 * 1024;
/// hbb_common/src/compress.rs `MAX_DECOMPRESSED_SIZE`.
const MAX_DECOMPRESSED_SIZE: usize = 256 * 1024 * 1024;
/// libs/base/src/fs.rs `can_enable_overwrite_detection`.
const MIN_VER_OVERWRITE_DETECTION: &str = "1.1.10";
/// src/common.rs `is_support_file_transfer_resume_num`.
const MIN_VER_RESUME: &str = "1.4.2";
/// flutter/lib/desktop/pages/file_manager_page.dart: "Rename" is offered
/// for a peer from this version on.
const MIN_VER_RENAME: &str = "1.3.0";
/// The host's login error when its file-transfer permission is off
/// (src/server/connection.rs `handle_login_request`).
const NO_FILE_PERMISSION: &str = "No permission of file transfer";
/// How long the host has for the listing it sends by itself after the login.
const FIRST_LISTING_TIMEOUT: Duration = Duration::from_secs(5);
/// Upload data is not taken from the viewer while this much waits to be
/// written to the host.
const OUT_LIMIT: usize = 1024 * 1024;
/// Other names tried for an upload that is to be kept beside the file there.
const MAX_OTHER_NAMES: u32 = 100;

/// `Connector::open_files` of the RustDesk connector.
pub(super) async fn open(target: &Target) -> anyhow::Result<FileSession> {
    let host = target.get("host").trim();
    if host.is_empty() {
        anyhow::bail!("The computer's IP address is missing");
    }
    let port = match target.get("port").trim() {
        "" => DEFAULT_PORT,
        port => port.parse::<u16>().map_err(|_| anyhow::anyhow!("\"{}\" is not a port", port))?,
    };
    let address = format!("{}:{}", host, port);

    // client.rs Client::_start, the `is_ip_str` branch, as the picture
    // session does it (rustdesk.rs): a connection of its own.
    let stream = match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&address)).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => {
            warn!("RustDesk connector: {} cannot be reached for files: {}", address, e);
            anyhow::bail!("The RustDesk computer cannot be reached (is it on, and is direct IP access switched on there?)");
        }
        Err(_) => {
            warn!("RustDesk connector: {} does not answer", address);
            anyhow::bail!("The RustDesk computer does not answer");
        }
    };
    let _ = stream.set_nodelay(true);
    let mut conn = Conn { stream, decoder: frame::Decoder::new(frame::MAX_FRAME_LENGTH), buf: BytesMut::with_capacity(64 * 1024) };

    // io_loop.rs handle_msg_from_peer: the host's Hash, our LoginRequest,
    // its LoginResponse.
    let peer_info = loop {
        let msg = next_in_time(&mut conn, READ_TIMEOUT, "The RustDesk computer did not answer the login").await?;
        match msg.union {
            Some(message::Union::Hash(hash)) => {
                conn.send(&file_login_message(&address, target.get("password"), &hash)).await?;
            }
            Some(message::Union::LoginResponse(response)) => match response.union {
                Some(login_response::Union::Error(error)) => anyhow::bail!("{}", file_login_error(&error)),
                Some(login_response::Union::PeerInfo(info)) => break info,
                None => {}
            },
            // client.rs handle_test_delay.
            Some(message::Union::TestDelay(delay)) if !delay.from_client => {
                conn.send(&Message { union: Some(message::Union::TestDelay(delay)) }).await?;
            }
            Some(message::Union::Misc(Misc { union: Some(misc::Union::CloseReason(reason)) })) => {
                anyhow::bail!("{}", if reason.is_empty() { "The computer closed the connection".to_string() } else { reason })
            }
            _ => {}
        }
    };
    info!("RustDesk connector: file session with {} (version {}, platform {})", address, peer_info.version, peer_info.platform);

    let form = PathForm { windows: peer_is_windows(&peer_info) };
    // src/server/connection.rs `send_logon_response`: after the login the
    // host lists the folder of `FileTransfer.dir`, its home when that is
    // empty (ui_cm_interface.rs `read_dir`). That listing's path is the
    // home folder: the client is told it no other way.
    let mut first_listing_missed = false;
    let home = match first_listing(&mut conn, FIRST_LISTING_TIMEOUT).await? {
        Some(Ok(dir)) if !dir.path.is_empty() => Some(form.normalize(&dir.path)),
        Some(_) => None,
        None => {
            first_listing_missed = true;
            None
        }
    };
    // libs/base/src/fs.rs `read_dir`: on Windows "/" lists the drives
    // (file_model.dart `_goToParentDirectory`: "specially for C:\, D:\,
    // goto '/'"). They are this session's places.
    let mut roots: Vec<Place> = home.iter().map(|path| Place { name: "Home".into(), path: path.clone() }).collect();
    if form.windows && !first_listing_missed {
        conn.send(&action(file_action::Union::ReadDir(proto::ReadDir { path: "/".into(), include_hidden: true }))).await?;
        match first_listing(&mut conn, FIRST_LISTING_TIMEOUT).await? {
            Some(Ok(dir)) => roots.extend(
                dir.entries
                    .iter()
                    .filter(|e| e.entry_type == FileType::DirDrive as i32)
                    .map(|e| Place { name: e.name.clone(), path: form.normalize(&e.name) }),
            ),
            Some(Err(_)) => {}
            None => first_listing_missed = true,
        }
    } else if !form.windows {
        roots.push(Place { name: "/".into(), path: "/".into() });
    }

    let info = FilesInfo { os: platform_of(&peer_info), sep: form.sep().to_string(), home, roots };
    let (event_tx, events) = mpsc::channel::<FileEvent>(files::QUEUE);
    let (commands, command_rx) = mpsc::channel::<FileCommand>(files::QUEUE);
    let mut state = Files::new(form, &peer_info.version, event_tx.clone());
    state.skip_listings = first_listing_missed as usize;
    tokio::spawn(async move {
        let reason = run(conn, state, command_rx).await;
        debug!("RustDesk connector: file session with {} ended: {}", address, reason);
        let _ = event_tx.send(FileEvent::Closed { reason }).await;
    });
    Ok(FileSession { info, events, commands })
}

async fn next_in_time(conn: &mut Conn, time: Duration, late: &str) -> anyhow::Result<Message> {
    tokio::time::timeout(time, conn.next())
        .await
        .map_err(|_| anyhow::anyhow!("{}", late))?
        .map_err(|e| anyhow::anyhow!("The connection to the RustDesk computer broke: {}", e.kind()))?
        .ok_or_else(|| anyhow::anyhow!("The RustDesk computer closed the connection"))
}

/// The next listing the host sends that belongs to no job (`FileDirectory.id`
/// 0, or the error in its place): `None` when none came in `time`.
async fn first_listing(conn: &mut Conn, time: Duration) -> anyhow::Result<Option<Result<FileDirectory, String>>> {
    let until = tokio::time::Instant::now() + time;
    loop {
        let msg = match tokio::time::timeout_at(until, conn.next()).await {
            Err(_) => return Ok(None),
            Ok(msg) => msg
                .map_err(|e| anyhow::anyhow!("The connection to the RustDesk computer broke: {}", e.kind()))?
                .ok_or_else(|| anyhow::anyhow!("The RustDesk computer closed the connection"))?,
        };
        match msg.union {
            Some(message::Union::FileResponse(FileResponse { union: Some(file_response::Union::Dir(dir)) })) if dir.id <= 0 => {
                return Ok(Some(Ok(dir)))
            }
            Some(message::Union::FileResponse(FileResponse { union: Some(file_response::Union::Error(e)) })) if e.id <= 0 => {
                return Ok(Some(Err(e.error)))
            }
            Some(message::Union::TestDelay(delay)) if !delay.from_client => {
                conn.send(&Message { union: Some(message::Union::TestDelay(delay)) }).await?;
            }
            Some(message::Union::Misc(Misc { union: Some(misc::Union::CloseReason(reason)) })) => {
                anyhow::bail!("{}", if reason.is_empty() { "The computer closed the connection".to_string() } else { reason })
            }
            _ => {}
        }
    }
}

/// client.rs `LoginConfigHandler::create_login_msg` for
/// `ConnType::FILE_TRANSFER`: the login of the picture session with
/// `file_transfer` set (`dir` empty: the host's home; hidden entries are
/// asked for with each listing) and without the option message
/// (`get_option_message` gives none for a file transfer).
fn file_login_message(address: &str, password: &str, hash: &proto::Hash) -> Message {
    let mut msg = login_message(address, password, hash, &Viewer { codecs: Vec::new(), fps: 0 });
    if let Some(message::Union::LoginRequest(request)) = msg.union.as_mut() {
        request.option = None;
        request.union = Some(login_request::Union::FileTransfer(proto::FileTransfer { dir: String::new(), show_hidden: true }));
    }
    msg
}

/// client.rs `handle_login_error`, with what the host says when it does not
/// give its files (src/server/connection.rs: "No permission of file transfer").
fn file_login_error(error: &str) -> String {
    if error == NO_FILE_PERMISSION {
        return "File transfer is switched off on the RustDesk computer (its \"Enable file transfer\" permission)".to_string();
    }
    login_error(error)
}

/// io_loop.rs: `self.handler.peer_platform() == "Windows"`.
fn peer_is_windows(info: &PeerInfo) -> bool {
    info.platform == "Windows"
}

fn action(union: file_action::Union) -> Message {
    Message { union: Some(message::Union::FileAction(FileAction { union: Some(union) })) }
}

fn response(union: file_response::Union) -> Message {
    Message { union: Some(message::Union::FileResponse(FileResponse { union: Some(union) })) }
}

/// libs/base/src/fs.rs `new_send_confirm`.
fn send_confirm(id: i32, file_num: i32, union: confirm::Union) -> Message {
    action(file_action::Union::SendConfirm(FileTransferSendConfirmRequest { id, file_num, union: Some(union) }))
}

/// libs/base/src/fs.rs `is_compressed_file`: blocks of these are sent as
/// they are.
fn is_compressed_file(name: &str) -> bool {
    let ext = name.rfind('.').map(|i| &name[i + 1..]).unwrap_or("");
    ["xz", "gz", "zip", "7z", "rar", "bz2", "tgz", "png", "jpg"].contains(&ext)
}

/// libs/base/src/fs.rs `TransferJob::read`: a block of a file, compressed
/// (hbb_common/src/compress.rs, zstd) when that makes it smaller.
fn block_of(id: i32, file_num: i32, data: &[u8], may_compress: bool) -> FileTransferBlock {
    let mut block = FileTransferBlock { id, file_num, data: Bytes::new(), compressed: false, blk_id: 0 };
    if may_compress && !data.is_empty() {
        let compressed = cursor::compress(data);
        if !compressed.is_empty() && compressed.len() < data.len() {
            block.data = Bytes::from(compressed);
            block.compressed = true;
            return block;
        }
    }
    block.data = Bytes::copy_from_slice(data);
    block
}

/// libs/base/src/fs.rs `TransferJob::write`: the data of a block
/// (`decompress` when it says so). `None` for what is not zstd or would be
/// larger than hbb_common allows: RustDesk writes nothing then and goes
/// on; here the transfer fails, as a file with a hole is not the file.
fn data_of(block: FileTransferBlock) -> Option<Bytes> {
    if !block.compressed {
        return Some(block.data);
    }
    cursor::decompress(&block.data, MAX_DECOMPRESSED_SIZE).map(Bytes::from)
}

/// libs/base/src/fs/download.rs `validate_listing`: the names of a folder's
/// entries are single names (a drive in the list of drives is `C:`).
fn validate_listing(entries: &[proto::FileEntry], peer_windows: bool) -> Result<(), String> {
    for entry in entries {
        let name = &entry.name;
        if entry.entry_type == FileType::DirDrive as i32 && matches!(name.as_bytes(), [drive, b':'] if drive.is_ascii_alphabetic()) {
            continue;
        }
        if name.is_empty()
            || name == "."
            || name == ".."
            || name.contains('\0')
            || name.contains('/')
            || (peer_windows && (name.contains('\\') || matches!(name.as_bytes(), [drive, b':', ..] if drive.is_ascii_alphabetic())))
        {
            return Err("The computer sent a folder listing with a name that is not a file's name".to_string());
        }
    }
    Ok(())
}

/// libs/base/src/fs.rs `validate_file_name_no_traversal`, for the names of
/// a recursive listing (written with `/` by then): relative, without "..".
fn relative_name_is_safe(name: &str) -> bool {
    !name.contains('\0')
        && !name.starts_with('/')
        && !name.split('/').any(|part| part == "..")
        && !matches!(name.as_bytes(), [drive, b':', ..] if drive.is_ascii_alphabetic())
}

fn is_dir(entry: &proto::FileEntry) -> bool {
    entry.entry_type != FileType::File as i32 && entry.entry_type != FileType::FileLink as i32
}

fn entry_of(entry: &proto::FileEntry) -> FileEntry {
    FileEntry { name: entry.name.clone(), dir: is_dir(entry), size: entry.size, mtime: entry.modified_time, hidden: entry.is_hidden }
}

/// What a listing without a job id is waited for (the host answers
/// `ReadDir` in the order asked: ui_cm_interface.rs `read_dir`).
enum Listing {
    List { req: String, path: String },
    Stat { req: String, name: String },
    /// `Remove`: what kind of entry is to go.
    Remove { req: String, path: String, name: String },
    /// `Rename`: the entry is there and the new name is not.
    Rename { req: String, path: String, name: String, new_name: String },
}

impl Listing {
    fn req(&self) -> &str {
        match self {
            Listing::List { req, .. } | Listing::Stat { req, .. } | Listing::Remove { req, .. } | Listing::Rename { req, .. } => req,
        }
    }
}

/// What a job id that is not a transfer waits for.
enum Op {
    /// `Done` is the end of it.
    Simple { req: String },
    /// `ReadAllFiles` for `Walk`.
    Walk { req: String, path: String },
    /// `ReadAllFiles` for removing a folder.
    RemoveList { req: String, path: String },
    /// The folder's files go one by one, then the folder.
    RemoveFiles { req: String, path: String, files: Vec<String>, next: usize },
}

impl Op {
    fn req(&self) -> &str {
        match self {
            Op::Simple { req } | Op::Walk { req, .. } | Op::RemoveList { req, .. } | Op::RemoveFiles { req, .. } => req,
        }
    }
}

struct Download {
    req: String,
    job: i32,
    /// Where the viewer wants it from.
    offset: u64,
    size: u64,
    mtime: u64,
    /// `ReadStart` was sent.
    started: bool,
    /// Bytes from the host that the viewer has already (a host that cannot
    /// start in the middle sends the file from its start).
    to_skip: u64,
}

#[derive(PartialEq)]
enum UploadState {
    /// The host is asked whether the file is there.
    Asked,
    Sending,
    /// `Done` was sent; the host's own `Done` is the end.
    Finishing,
}

struct Upload {
    req: String,
    job: i32,
    /// The folder and the name asked for, and the path written now (another
    /// name when the file was to be kept beside the one there).
    dir: String,
    name: String,
    path: String,
    size: u64,
    mtime: u64,
    on_exists: OnExists,
    other_names: u32,
    state: UploadState,
    received: u64,
    block: BytesMut,
}

enum Flow {
    Continue,
    End(String),
}

/// The client's state of a file session, without its connection: commands
/// and the host's messages in, the host's messages (`out`) and events out.
struct Files {
    form: PathForm,
    /// hbb_common `get_version_number` of the host's version.
    version: i64,
    events: mpsc::Sender<FileEvent>,
    out: Vec<Message>,
    /// libs/base/src/fs.rs `get_next_job_id`.
    next_job: i32,
    listings: VecDeque<Listing>,
    /// Listings without a job id that belong to nothing asked here (one
    /// the host sent late after the login).
    skip_listings: usize,
    ops: HashMap<i32, Op>,
    download: Option<Download>,
    upload: Option<Upload>,
    /// Jobs that are over here; what the host still says of them is not news.
    over: HashSet<i32>,
}

impl Files {
    fn new(form: PathForm, version: &str, events: mpsc::Sender<FileEvent>) -> Self {
        Files {
            form,
            version: get_version_number(version),
            events,
            out: Vec::new(),
            next_job: 1,
            listings: VecDeque::new(),
            skip_listings: 0,
            ops: HashMap::new(),
            download: None,
            upload: None,
            over: HashSet::new(),
        }
    }

    fn job_id(&mut self) -> i32 {
        let id = self.next_job;
        self.next_job += 1;
        id
    }

    fn at_least(&self, version: &str) -> bool {
        self.version >= get_version_number(version)
    }

    /// An event for the agent; `false` when nobody listens any more.
    async fn emit(&self, event: FileEvent) -> bool {
        self.events.send(event).await.is_ok()
    }

    async fn fail(&self, req: &str, message: impl Into<String>) -> bool {
        self.emit(FileEvent::Error { id: req.to_string(), message: message.into() }).await
    }

    /// file_trait.rs `read_remote_dir`.
    fn read_dir(&mut self, path: &str, include_hidden: bool, waiting: Listing) {
        self.out.push(action(file_action::Union::ReadDir(proto::ReadDir { path: path.to_string(), include_hidden })));
        self.listings.push_back(waiting);
    }

    /// io_loop.rs `cancel_transfer_job`.
    fn cancel_job(&mut self, job: i32) {
        self.out.push(action(file_action::Union::Cancel(proto::FileTransferCancel { id: job })));
        self.over.insert(job);
    }

    /// A path a command names, in the host's form; `None` (after the error
    /// went out) when it names nothing.
    async fn path_of(&self, req: &str, path: &str) -> Option<String> {
        let path = self.form.normalize(path);
        if path.is_empty() || path.contains('\0') {
            self.fail(req, "No file or folder was named").await;
            return None;
        }
        Some(path)
    }

    /// The folder and the name of an entry a command names.
    async fn entry_of_path(&self, req: &str, path: &str) -> Option<(String, String, String)> {
        let path = self.path_of(req, path).await?;
        match self.form.parent(&path) {
            Some(parent) => {
                let name = self.form.name(&path);
                Some((path, parent, name))
            }
            None => {
                self.fail(req, format!("{} cannot be changed from here", path)).await;
                None
            }
        }
    }

    async fn on_command(&mut self, command: FileCommand) -> Flow {
        match command {
            FileCommand::Close => return Flow::End("Closed".into()),
            FileCommand::List { id, path, hidden } => {
                let path = self.form.normalize(&path);
                self.read_dir(&path.clone(), hidden, Listing::List { req: id, path });
            }
            // The protocol has no question about one entry: its folder is
            // listed and the entry looked for in it.
            FileCommand::Stat { id, path } => {
                let Some(path) = self.path_of(&id, &path).await else { return Flow::Continue };
                match self.form.parent(&path) {
                    Some(parent) => {
                        let name = self.form.name(&path);
                        self.read_dir(&parent, true, Listing::Stat { req: id, name });
                    }
                    // A root: a folder.
                    None => {
                        let entry = FileEntry { name: path, dir: true, size: 0, mtime: 0, hidden: false };
                        self.emit(FileEvent::Stat { id, entry: Some(entry) }).await;
                    }
                }
            }
            // io_loop.rs `Data::RemoveDirAll` (is_remote): every file under
            // the folder, as `ReadAllFiles`.
            FileCommand::Walk { id, path } => {
                let Some(path) = self.path_of(&id, &path).await else { return Flow::Continue };
                let job = self.job_id();
                self.out.push(action(file_action::Union::AllFiles(proto::ReadAllFiles { id: job, path: path.clone(), include_hidden: true })));
                self.ops.insert(job, Op::Walk { req: id, path });
            }
            // io_loop.rs `Data::CreateDir` (is_remote).
            FileCommand::Mkdir { id, path } => {
                let Some(path) = self.path_of(&id, &path).await else { return Flow::Continue };
                let job = self.job_id();
                self.out.push(action(file_action::Union::Create(proto::FileDirCreate { id: job, path })));
                self.ops.insert(job, Op::Simple { req: id });
            }
            // file_model.dart `removeAction` knows what it removes (a file,
            // a folder) from the listing the person chose it in: the
            // entry's folder is listed first.
            FileCommand::Remove { id, path } => {
                let Some((path, parent, name)) = self.entry_of_path(&id, &path).await else { return Flow::Continue };
                self.read_dir(&parent, true, Listing::Remove { req: id, path, name });
            }
            FileCommand::Rename { id, path, name } => {
                if !self.at_least(MIN_VER_RENAME) {
                    self.fail(&id, "The RustDesk on that computer is too old to rename files (it needs 1.3.0 or newer)").await;
                    return Flow::Continue;
                }
                // libs/base/src/fs.rs `rename_file`: the new name is one name.
                if !PathForm::is_plain_name(&name) {
                    self.fail(&id, "That is not a name for a file").await;
                    return Flow::Continue;
                }
                let Some((path, parent, old)) = self.entry_of_path(&id, &path).await else { return Flow::Continue };
                // The host renames over a file of the new name without a
                // word (`std::fs::rename`): looked for first.
                self.read_dir(&parent, true, Listing::Rename { req: id, path, name: old, new_name: name });
            }
            FileCommand::Read { id, path, offset } => {
                let Some(path) = self.path_of(&id, &path).await else { return Flow::Continue };
                if self.download.is_some() {
                    self.fail(&id, "Another download is running").await;
                    return Flow::Continue;
                }
                // io_loop.rs `Data::SendFiles` (is_remote): `fs::new_send`;
                // the host answers with the file list, then the digest.
                let job = self.job_id();
                self.out.push(action(file_action::Union::Send(proto::FileTransferSendRequest {
                    id: job,
                    path,
                    include_hidden: true,
                    file_num: 0,
                    file_type: proto::file_transfer_send_request::FileType::Generic as i32,
                })));
                self.download = Some(Download { req: id, job, offset, size: 0, mtime: 0, started: false, to_skip: 0 });
            }
            FileCommand::Write { id, path, size, mtime, on_exists } => {
                let Some((path, dir, name)) = self.entry_of_path(&id, &path).await else { return Flow::Continue };
                if !PathForm::is_plain_name(&name) {
                    self.fail(&id, "That is not a name for a file").await;
                    return Flow::Continue;
                }
                // A new upload takes the place of an unfinished one; what
                // the host has of that one stays there, to go on from.
                if let Some(old) = self.upload.take() {
                    self.over.insert(old.job);
                }
                let mut upload = Upload {
                    req: id,
                    job: 0,
                    dir,
                    name,
                    path,
                    size,
                    mtime,
                    on_exists,
                    other_names: 0,
                    state: UploadState::Asked,
                    received: 0,
                    block: BytesMut::new(),
                };
                let ready = self.ask_to_write(&mut upload);
                self.upload = Some(upload);
                if let Some(offset) = ready {
                    return self.upload_ready(offset).await;
                }
            }
            FileCommand::WriteData { id, data } => return self.upload_data(&id, data).await,
            FileCommand::WriteEnd { id } => return self.upload_end(&id).await,
            // io_loop.rs `Data::CancelJob` -> `cancel_transfer_job`.
            FileCommand::Cancel { id } => {
                if self.download.as_ref().is_some_and(|d| d.req == id) {
                    let download = self.download.take().expect("checked");
                    self.cancel_job(download.job);
                    // Nothing of it follows: its later blocks are dropped here.
                    if !self.emit(FileEvent::ReadEnd { id, cancelled: true }).await {
                        return Flow::End("The viewer left".into());
                    }
                } else if self.upload.as_ref().is_some_and(|u| u.req == id) {
                    let upload = self.upload.take().expect("checked");
                    self.cancel_job(upload.job);
                }
            }
        }
        Flow::Continue
    }

    /// io_loop.rs `Data::SendFiles` (not remote): `fs::new_receive` with the
    /// one file (its name empty: the path is the file's, libs/base/src/fs.rs
    /// `read_dir_recursive` for a file), then, to a host that knows it
    /// (`can_enable_overwrite_detection`), the file's digest
    /// (`TransferJob::send_current_digest`), on which the host says whether
    /// the file is there. An older host is sent the data at once: `Some(0)`.
    ///
    /// The digest always says `is_resume`: the host then looks for what it
    /// has of this same file from an upload that broke off
    /// (`is_write_need_confirmation`), which RustDesk's client asks only
    /// for a job the person resumes, and the Lunaris file manager always.
    fn ask_to_write(&mut self, upload: &mut Upload) -> Option<u64> {
        upload.job = self.job_id();
        upload.state = UploadState::Asked;
        upload.received = 0;
        upload.block.clear();
        let file = proto::FileEntry {
            entry_type: FileType::File as i32,
            name: String::new(),
            is_hidden: false,
            size: upload.size,
            modified_time: upload.mtime,
        };
        self.out.push(action(file_action::Union::Receive(proto::FileTransferReceiveRequest {
            id: upload.job,
            path: upload.path.clone(),
            files: vec![file],
            file_num: 0,
            total_size: upload.size,
        })));
        if !self.at_least(MIN_VER_OVERWRITE_DETECTION) {
            return Some(0);
        }
        self.out.push(response(file_response::Union::Digest(FileTransferDigest {
            id: upload.job,
            file_num: 0,
            last_modified: upload.mtime,
            file_size: upload.size,
            is_resume: true,
            ..Default::default()
        })));
        None
    }

    /// The host takes the data from `offset` on.
    async fn upload_ready(&mut self, offset: u64) -> Flow {
        let Some(upload) = self.upload.as_mut() else { return Flow::Continue };
        upload.state = UploadState::Sending;
        upload.received = offset;
        let event = FileEvent::WriteReady { id: upload.req.clone(), offset, path: upload.path.clone() };
        if self.emit(event).await {
            Flow::Continue
        } else {
            Flow::End("The viewer left".into())
        }
    }

    /// The file is not written. io_loop.rs, the digest of an upload with
    /// `Skip`: `job.confirm` (`set_file_skipped`), the confirm to the host,
    /// and then what `fs::handle_read_jobs` sends for a job whose only file
    /// was skipped (`job_error`: "skipped"), which ends the host's job.
    fn skip_upload(&mut self, job: i32) {
        self.out.push(send_confirm(job, 0, confirm::Union::Skip(true)));
        self.end_skipped_job(job);
    }

    fn end_skipped_job(&mut self, job: i32) {
        self.out.push(response(file_response::Union::Error(proto::FileTransferError { id: job, error: "skipped".into(), file_num: 1 })));
        // The host says the same back (ui_cm_interface.rs `FS::WriteError`).
        self.over.insert(job);
    }

    /// io_loop.rs `handle_msg_from_peer`, `FileResponse::Digest` with
    /// `is_upload`: the host has a file of that name, or a part of this one.
    async fn upload_digest(&mut self, digest: FileTransferDigest) -> Flow {
        let Some(upload) = self.upload.as_mut() else { return Flow::Continue };
        let job = upload.job;
        // "if digest.is_identical && job.is_resume { if digest.transferred_size > 0 {
        // overwrite_strategy = Some(true); offset = digest.transferred_size } }".
        // The offset is 32 bits in the protocol (`offset_blk`): what does
        // not fit, or is more than the file, is sent from the start.
        if digest.is_identical && digest.transferred_size > 0 {
            let offset = match u32::try_from(digest.transferred_size) {
                Ok(offset) if digest.transferred_size <= upload.size => offset,
                _ => 0,
            };
            self.out.push(send_confirm(job, 0, confirm::Union::OffsetBlk(offset)));
            return self.upload_ready(offset as u64).await;
        }
        // RustDesk asks the person (`override_file_confirm`, telling them
        // when the file is the same one); the Lunaris file manager said
        // beforehand what it wants.
        let (req, path) = (upload.req.clone(), upload.path.clone());
        let skip = |reason: &str| FileEvent::WriteSkip { id: req.clone(), reason: reason.into(), path: path.clone() };
        let event = if digest.is_identical {
            Some(skip("identical"))
        } else {
            match upload.on_exists {
                OnExists::Replace => {
                    self.out.push(send_confirm(job, 0, confirm::Union::OffsetBlk(0)));
                    return self.upload_ready(0).await;
                }
                OnExists::Skip => Some(skip("exists")),
                // Kept beside the one there: this job is skipped and the
                // file offered again under another name.
                OnExists::Rename => None,
            }
        };
        self.skip_upload(job);
        match event {
            Some(event) => {
                self.upload = None;
                if !self.emit(event).await {
                    return Flow::End("The viewer left".into());
                }
            }
            None => {
                let mut upload = self.upload.take().expect("checked above");
                upload.other_names += 1;
                if upload.other_names > MAX_OTHER_NAMES {
                    self.fail(&upload.req, format!("{} is there already, and so are its other names", upload.name)).await;
                    return Flow::Continue;
                }
                upload.path = self.form.join(&upload.dir, &files::numbered_name(&upload.name, upload.other_names + 1));
                let ready = self.ask_to_write(&mut upload);
                self.upload = Some(upload);
                if let Some(offset) = ready {
                    return self.upload_ready(offset).await;
                }
            }
        }
        Flow::Continue
    }

    /// The host's answer to the digest of an upload when no file is in the
    /// way (ui_cm_interface.rs `FS::CheckDigest`: `NoSuchFile` ->
    /// `new_send_confirm` with offset 0; `IsSame` -> skip). io_loop.rs
    /// `FileAction::SendConfirm`: `job.confirm`.
    async fn upload_confirmed(&mut self, request: FileTransferSendConfirmRequest) -> Flow {
        let Some(upload) = self.upload.as_ref() else { return Flow::Continue };
        if upload.state != UploadState::Asked {
            return Flow::Continue;
        }
        match request.union {
            Some(confirm::Union::Skip(true)) => {
                let upload = self.upload.take().expect("checked");
                self.end_skipped_job(upload.job);
                let event = FileEvent::WriteSkip { id: upload.req, reason: "identical".into(), path: upload.path };
                if !self.emit(event).await {
                    return Flow::End("The viewer left".into());
                }
                Flow::Continue
            }
            Some(confirm::Union::OffsetBlk(offset)) if offset as u64 <= upload.size => self.upload_ready(offset as u64).await,
            _ => self.upload_ready(0).await,
        }
    }

    /// libs/base/src/fs.rs `TransferJob::read`, with the viewer in the
    /// place of the file: its data goes on in blocks of `BUF_SIZE`.
    async fn upload_data(&mut self, req: &str, data: Bytes) -> Flow {
        let Some(upload) = self.upload.as_mut().filter(|u| u.req == req && u.state == UploadState::Sending) else {
            return Flow::Continue;
        };
        if upload.received.saturating_add(data.len() as u64) > upload.size {
            let upload = self.upload.take().expect("checked");
            self.cancel_job(upload.job);
            self.fail(req, format!("more data than the announced {} bytes: transfer refused", upload.size)).await;
            return Flow::Continue;
        }
        upload.received += data.len() as u64;
        upload.block.extend_from_slice(&data);
        let may_compress = !is_compressed_file(&upload.name);
        while upload.block.len() >= BLOCK {
            let block = upload.block.split_to(BLOCK);
            self.out.push(response(file_response::Union::Block(block_of(upload.job, 0, &block, may_compress))));
        }
        Flow::Continue
    }

    /// The end of the file: the rest of the data, then what
    /// `TransferJob::read` gives at a file's end (an empty block of that
    /// file, on which the host makes a file that has no data) and what
    /// `fs::handle_read_jobs` sends when the job is through (`new_done`
    /// with the job's `file_num`, now 1). The host's own `Done` follows.
    async fn upload_end(&mut self, req: &str) -> Flow {
        let Some(upload) = self.upload.as_mut().filter(|u| u.req == req && u.state == UploadState::Sending) else {
            if self.upload.as_ref().is_none_or(|u| u.req != req) {
                self.fail(req, "no upload in progress").await;
            }
            return Flow::Continue;
        };
        if upload.received != upload.size {
            // What the host has stays there: the upload can go on from it.
            let upload = self.upload.take().expect("checked");
            self.over.insert(upload.job);
            self.fail(req, format!("incomplete: {} of {} bytes (it can be resumed)", upload.received, upload.size)).await;
            return Flow::Continue;
        }
        let may_compress = !is_compressed_file(&upload.name);
        if !upload.block.is_empty() {
            let block = upload.block.split();
            self.out.push(response(file_response::Union::Block(block_of(upload.job, 0, &block, may_compress))));
        }
        self.out.push(response(file_response::Union::Block(block_of(upload.job, 0, &[], false))));
        self.out.push(response(file_response::Union::Done(proto::FileTransferDone { id: upload.job, file_num: 1 })));
        upload.state = UploadState::Finishing;
        Flow::Continue
    }

    async fn on_message(&mut self, msg: Message) -> Flow {
        let alive = match msg.union {
            // client.rs handle_test_delay: the host's probe goes back as it
            // came (a host ends a connection it hears nothing from).
            Some(message::Union::TestDelay(delay)) => {
                if !delay.from_client {
                    self.out.push(Message { union: Some(message::Union::TestDelay(delay)) });
                }
                true
            }
            Some(message::Union::Misc(Misc { union: Some(misc::Union::CloseReason(reason)) })) => {
                return Flow::End(if reason.is_empty() { "The computer closed the connection".into() } else { reason });
            }
            Some(message::Union::FileResponse(FileResponse { union: Some(union) })) => match union {
                file_response::Union::Dir(dir) => self.on_dir(dir).await,
                file_response::Union::Digest(digest) => return self.on_digest(digest).await,
                file_response::Union::Block(block) => self.on_block(block).await,
                file_response::Union::Done(done) => self.on_done(done.id).await,
                file_response::Union::Error(error) => self.on_error(error.id, error.error).await,
                file_response::Union::EmptyDirs(_) => true,
            },
            // io_loop.rs `FileAction::SendConfirm`: the host's word on an upload.
            Some(message::Union::FileAction(FileAction { union: Some(file_action::Union::SendConfirm(request)) })) => {
                if self.upload.as_ref().is_some_and(|u| u.job == request.id) {
                    return self.upload_confirmed(request).await;
                }
                true
            }
            _ => true,
        };
        if alive {
            Flow::Continue
        } else {
            Flow::End("The viewer left".into())
        }
    }

    /// io_loop.rs `handle_msg_from_peer`, `FileResponse::Dir`: a folder's
    /// listing (no job id), or the files of a job.
    async fn on_dir(&mut self, mut dir: FileDirectory) -> bool {
        // "if self.handler.peer_platform() == "Windows" { fs::transform_windows_path(&mut entries) }"
        // for a client that is not Windows; here the names are used with `/` on every system.
        let names_with_slash = |dir: &mut FileDirectory, windows: bool| {
            if windows {
                for entry in dir.entries.iter_mut() {
                    entry.name = entry.name.replace('\\', "/");
                }
            }
        };
        if dir.id <= 0 {
            if self.skip_listings > 0 {
                self.skip_listings -= 1;
                return true;
            }
            let Some(waiting) = self.listings.pop_front() else { return true };
            if let Err(e) = validate_listing(&dir.entries, self.form.windows) {
                return self.fail(waiting.req(), e).await;
            }
            return self.on_listing(waiting, dir).await;
        }
        if self.download.as_ref().is_some_and(|d| d.job == dir.id) {
            // The job's file list (libs/base/src/fs.rs `TransferJob::new_read`
            // on the host): one file without a name when the path is a
            // file's; a folder's files otherwise.
            let one_file = dir.entries.len() == 1 && dir.entries[0].name.is_empty();
            let download = self.download.as_mut().expect("checked");
            if !one_file {
                let (req, job) = (download.req.clone(), download.job);
                self.download = None;
                self.cancel_job(job);
                return self.fail(&req, "That is not a file").await;
            }
            download.size = dir.entries[0].size;
            download.mtime = dir.entries[0].modified_time;
            return true;
        }
        match self.ops.remove(&dir.id) {
            Some(Op::Walk { req, path }) => {
                names_with_slash(&mut dir, self.form.windows);
                if dir.entries.iter().any(|e| !relative_name_is_safe(&e.name)) {
                    return self.fail(&req, "The computer sent a file list with a name that leads out of the folder").await;
                }
                // A file's path: the one file, without a name.
                if dir.entries.len() == 1 && dir.entries[0].name.is_empty() {
                    return self.fail(&req, format!("{} is not a folder", path)).await;
                }
                let files: Vec<WalkFile> =
                    dir.entries.iter().map(|e| WalkFile { rel: e.name.clone(), size: e.size, mtime: e.modified_time }).collect();
                // The list has files only: the folders are the ones they are in.
                let mut dirs: Vec<String> = Vec::new();
                let mut seen: HashSet<&str> = HashSet::new();
                for file in &files {
                    let mut rel = file.rel.as_str();
                    let mut above = Vec::new();
                    while let Some(cut) = rel.rfind('/') {
                        rel = &rel[..cut];
                        if !seen.insert(rel) {
                            break;
                        }
                        above.push(rel.to_string());
                    }
                    dirs.extend(above.into_iter().rev());
                }
                self.emit(FileEvent::Walk { id: req, path, files, dirs }).await
            }
            Some(Op::RemoveList { req, path }) => {
                // The names stay as the host wrote them: they go back to it
                // (io_loop.rs `handle_job_status`: "job.path, job.sep, name").
                let windows = self.form.windows;
                let with_slash = |name: &str| if windows { name.replace('\\', "/") } else { name.to_string() };
                if dir.entries.iter().any(|e| e.name.is_empty() || !relative_name_is_safe(&with_slash(&e.name))) {
                    return self.fail(&req, "The computer sent a file list with a name that leads out of the folder").await;
                }
                let files: Vec<String> = dir.entries.into_iter().map(|e| e.name).collect();
                self.remove_next(dir.id, req, path, files, 0);
                true
            }
            Some(other) => {
                self.ops.insert(dir.id, other);
                true
            }
            None => true,
        }
    }

    /// file_model.dart `removeAction` for a folder: each file of it
    /// (`sendRemoveFile`, the next when the one before is done), then
    /// `sendRemoveEmptyDir` (io_loop.rs `Data::RemoveDir`: `FileRemoveDir`
    /// with `recursive`, which takes the folders that are empty by then).
    fn remove_next(&mut self, job: i32, req: String, path: String, files: Vec<String>, next: usize) {
        match files.get(next) {
            Some(name) => {
                let file = format!("{}{}{}", path.trim_end_matches(['/', '\\']), self.form.sep(), name);
                self.out.push(action(file_action::Union::RemoveFile(proto::FileRemoveFile { id: job, path: file, file_num: next as i32 })));
                self.ops.insert(job, Op::RemoveFiles { req, path, files, next });
            }
            None => {
                self.out.push(action(file_action::Union::RemoveDir(proto::FileRemoveDir { id: job, path, recursive: true })));
                self.ops.insert(job, Op::Simple { req });
            }
        }
    }

    async fn on_listing(&mut self, waiting: Listing, dir: FileDirectory) -> bool {
        // Windows names differ by more than their case.
        let windows = self.form.windows;
        let same = move |a: &str, b: &str| if windows { a.to_lowercase() == b.to_lowercase() } else { a == b };
        match waiting {
            Listing::List { req, path } => {
                // The folder asked for, or the one the host chose ("" is its home).
                let path = if dir.path.is_empty() { path } else { self.form.normalize(&dir.path) };
                let parent = self.form.parent(&path);
                let entries = dir.entries.iter().map(entry_of).collect();
                self.emit(FileEvent::Dir { id: req, path, parent, entries }).await
            }
            Listing::Stat { req, name } => {
                let entry = dir.entries.iter().find(|e| same(&e.name, &name)).map(entry_of);
                self.emit(FileEvent::Stat { id: req, entry }).await
            }
            Listing::Remove { req, path, name } => {
                let Some(entry) = dir.entries.iter().find(|e| same(&e.name, &name)) else {
                    return self.fail(&req, format!("{} is not there", name)).await;
                };
                let job = self.job_id();
                let kind = FileType::try_from(entry.entry_type).unwrap_or(FileType::File);
                match kind {
                    // io_loop.rs `Data::RemoveFile` (is_remote).
                    FileType::File | FileType::FileLink => {
                        self.out.push(action(file_action::Union::RemoveFile(proto::FileRemoveFile { id: job, path, file_num: 0 })));
                        self.ops.insert(job, Op::Simple { req });
                    }
                    // A link to a folder goes itself, not what it leads to:
                    // on Windows it is removed as a folder
                    // (`FileRemoveDir` without `recursive`: `std::fs::remove_dir`),
                    // elsewhere as a file.
                    FileType::DirLink => {
                        let union = if self.form.windows {
                            file_action::Union::RemoveDir(proto::FileRemoveDir { id: job, path, recursive: false })
                        } else {
                            file_action::Union::RemoveFile(proto::FileRemoveFile { id: job, path, file_num: 0 })
                        };
                        self.out.push(action(union));
                        self.ops.insert(job, Op::Simple { req });
                    }
                    // io_loop.rs `Data::RemoveDirAll` (is_remote): the files
                    // under it first (`ReadAllFiles`).
                    FileType::Dir => {
                        self.out.push(action(file_action::Union::AllFiles(proto::ReadAllFiles { id: job, path: path.clone(), include_hidden: true })));
                        self.ops.insert(job, Op::RemoveList { req, path });
                    }
                    FileType::DirDrive => return self.fail(&req, "A drive cannot be removed").await,
                }
                true
            }
            Listing::Rename { req, path, name, new_name } => {
                if !dir.entries.iter().any(|e| same(&e.name, &name)) {
                    return self.fail(&req, format!("{} is not there", name)).await;
                }
                if !same(&name, &new_name) && dir.entries.iter().any(|e| same(&e.name, &new_name)) {
                    return self.fail(&req, format!("{} already exists", new_name)).await;
                }
                // io_loop.rs `Data::RenameFile` (is_remote).
                let job = self.job_id();
                self.out.push(action(file_action::Union::Rename(proto::FileRename { id: job, path, new_name })));
                self.ops.insert(job, Op::Simple { req });
                true
            }
        }
    }

    /// io_loop.rs `handle_msg_from_peer`, `FileResponse::Digest`.
    async fn on_digest(&mut self, digest: FileTransferDigest) -> Flow {
        if digest.is_upload {
            if self.upload.as_ref().is_some_and(|u| u.job == digest.id && u.state == UploadState::Asked) {
                return self.upload_digest(digest).await;
            }
            return Flow::Continue;
        }
        // A download: the host waits for the word to send the file
        // (`TransferJob::read` gives nothing before `file_confirmed`).
        // RustDesk's client looks at its own disk here
        // (`download_digest` -> `is_write_need_confirmation`); the viewer
        // has said how much of the file it has: the host goes on from
        // there when it can (`TransferJob::confirm` -> `set_stream_offset`,
        // from 1.4.2 on, an offset of 32 bits), and sends the whole file
        // otherwise, of which the start is dropped here.
        let can_resume = self.at_least(MIN_VER_RESUME);
        let Some(download) = self.download.as_mut().filter(|d| d.job == digest.id && !d.started) else {
            return Flow::Continue;
        };
        download.size = digest.file_size;
        download.mtime = digest.last_modified;
        let offset = download.offset.min(download.size);
        let host_offset = if can_resume { u32::try_from(offset).unwrap_or(0) } else { 0 };
        download.to_skip = offset - host_offset as u64;
        download.started = true;
        let event = FileEvent::ReadStart { id: download.req.clone(), size: download.size, mtime: download.mtime, offset };
        self.out.push(send_confirm(digest.id, digest.file_num, confirm::Union::OffsetBlk(host_offset)));
        if self.emit(event).await {
            Flow::Continue
        } else {
            Flow::End("The viewer left".into())
        }
    }

    /// io_loop.rs `handle_msg_from_peer`, `FileResponse::Block`
    /// (`TransferJob::write`): the data goes to the viewer.
    async fn on_block(&mut self, block: FileTransferBlock) -> bool {
        let Some(download) = self.download.as_mut().filter(|d| d.job == block.id) else { return true };
        let mut start = None;
        if !download.started {
            // A host from before the digest (1.1.10) sends the file at once.
            download.started = true;
            let offset = download.offset.min(download.size);
            download.to_skip = offset;
            start = Some(FileEvent::ReadStart { id: download.req.clone(), size: download.size, mtime: download.mtime, offset });
        }
        let req = download.req.clone();
        let Some(mut data) = data_of(block) else {
            let job = download.job;
            self.download = None;
            self.cancel_job(job);
            return self.fail(&req, "The computer sent data that cannot be read").await;
        };
        let skip = download.to_skip.min(data.len() as u64);
        download.to_skip -= skip;
        let _ = data.split_to(skip as usize);
        if let Some(event) = start {
            if !self.emit(event).await {
                return false;
            }
        }
        // The empty block at a file's end is the host's own business.
        data.is_empty() || self.emit(FileEvent::ReadData { id: req, data }).await
    }

    /// io_loop.rs `handle_msg_from_peer`, `FileResponse::Done`
    /// (`handle_job_status` without an error).
    async fn on_done(&mut self, job: i32) -> bool {
        if self.over.remove(&job) {
            return true;
        }
        if self.download.as_ref().is_some_and(|d| d.job == job) {
            let download = self.download.take().expect("checked");
            if !download.started {
                // An empty file from a host without the digest.
                let start = FileEvent::ReadStart { id: download.req.clone(), size: download.size, mtime: download.mtime, offset: 0 };
                if !self.emit(start).await {
                    return false;
                }
            }
            return self.emit(FileEvent::ReadEnd { id: download.req, cancelled: false }).await;
        }
        if self.upload.as_ref().is_some_and(|u| u.job == job && u.state == UploadState::Finishing) {
            let upload = self.upload.take().expect("checked");
            return self.emit(FileEvent::WriteDone { id: upload.req, path: upload.path }).await;
        }
        match self.ops.remove(&job) {
            Some(Op::Simple { req }) => self.emit(FileEvent::Ok { id: req }).await,
            Some(Op::RemoveFiles { req, path, files, next }) => {
                self.remove_next(job, req, path, files, next + 1);
                true
            }
            Some(other) => {
                self.ops.insert(job, other);
                true
            }
            None => true,
        }
    }

    /// io_loop.rs `handle_msg_from_peer`, `FileResponse::Error`
    /// (`handle_job_status` with the host's words).
    async fn on_error(&mut self, job: i32, error: String) -> bool {
        if job <= 0 {
            // ui_cm_interface.rs `read_dir`: `fs::new_error(0, err, -1)`.
            if self.skip_listings > 0 {
                self.skip_listings -= 1;
                return true;
            }
            return match self.listings.pop_front() {
                Some(waiting) => self.fail(waiting.req(), error).await,
                None => true,
            };
        }
        if self.over.remove(&job) {
            return true;
        }
        if self.download.as_ref().is_some_and(|d| d.job == job) {
            let download = self.download.take().expect("checked");
            return self.fail(&download.req, error).await;
        }
        if self.upload.as_ref().is_some_and(|u| u.job == job) {
            let upload = self.upload.take().expect("checked");
            return self.fail(&upload.req, error).await;
        }
        match self.ops.remove(&job) {
            Some(op) => self.fail(op.req(), error).await,
            None => true,
        }
    }
}

/// hbb_common/src/bytes_codec.rs: the next framed `Message`; `None` when
/// the host closed the connection. What was read and not yet a whole
/// message stays in `buf`: it can be dropped between its awaits.
async fn next_message(
    read: &mut tokio::net::tcp::OwnedReadHalf,
    decoder: &mut frame::Decoder,
    buf: &mut BytesMut,
) -> std::io::Result<Option<Message>> {
    loop {
        if let Some(payload) = decoder.decode(buf)? {
            match Message::decode(payload.freeze()) {
                Ok(msg) => return Ok(Some(msg)),
                // io_loop.rs handle_msg_from_peer: what does not parse is skipped.
                Err(_) => continue,
            }
        }
        if read.read_buf(buf).await? == 0 {
            return Ok(None);
        }
    }
}

fn encode(messages: &mut Vec<Message>, out: &mut BytesMut) -> std::io::Result<()> {
    for msg in messages.drain(..) {
        let len = msg.encoded_len();
        out.reserve(len + 4);
        frame::put_header(len, out)?;
        msg.encode(out).map_err(std::io::Error::other)?;
    }
    Ok(())
}

/// The session after the login (io_loop.rs `io_loop`). Reading and writing
/// never wait for each other: while the host does not take what is sent to
/// it (it is sending itself), what it sends is still read. Returns why it
/// ended.
async fn run(conn: Conn, mut files: Files, mut commands: mpsc::Receiver<FileCommand>) -> String {
    let Conn { stream, mut decoder, mut buf } = conn;
    let (mut read, mut write) = stream.into_split();
    let mut out = BytesMut::new();
    let reason = loop {
        if let Err(e) = encode(&mut files.out, &mut out) {
            break format!("The connection to the computer broke: {}", e);
        }
        // Not `biased`: a command (a cancel) is not left waiting while the
        // host has more to say, nor the host while the viewer has more to send.
        tokio::select! {
            written = write.write_buf(&mut out), if !out.is_empty() => {
                if written.is_err() {
                    break "The connection to the computer broke".into();
                }
            }
            msg = next_message(&mut read, &mut decoder, &mut buf) => {
                let msg = match msg {
                    Ok(Some(msg)) => msg,
                    Ok(None) => break "The computer closed the connection".into(),
                    Err(e) => break format!("The connection to the computer broke: {}", e),
                };
                if let Flow::End(reason) = files.on_message(msg).await {
                    break reason;
                }
            }
            command = commands.recv(), if out.len() < OUT_LIMIT => {
                let flow = match command {
                    None => Flow::End("Closed".into()),
                    Some(command) => files.on_command(command).await,
                };
                if let Flow::End(reason) = flow {
                    break reason;
                }
            }
        }
    };
    // io_loop.rs send_close_reason, then the connection ends. What was to
    // be sent goes first, when the host still takes it.
    files.out.push(Message { union: Some(message::Union::Misc(Misc { union: Some(misc::Union::CloseReason(String::new())) })) });
    let _ = encode(&mut files.out, &mut out);
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        let _ = write.write_all(&out).await;
        let _ = write.shutdown().await;
    })
    .await;
    reason
}

#[cfg(test)]
#[path = "rustdesk_files_tests.rs"]
pub(crate) mod tests;
