//! Files of a target, through its connector: what the Lunaris file manager
//! needs of a computer that speaks another protocol. A file session is a
//! connection of its own to the target, beside any picture session (as the
//! agent's own file sessions are, `crate::files`).
//!
//! The same shape as a picture [`Session`](super::Session): the agent sends
//! [`FileCommand`]s and the session answers with [`FileEvent`]s, each naming
//! the request it belongs to by its `id` (the requester's own text).
//!
//! - [`Connector::open_files`](super::Connector::open_files): log in to a
//!   target for files; gives a [`FileSession`] with what is known of the
//!   computer ([`FilesInfo`]: its system, its path separator, the folder it
//!   starts in when it can tell, its places).
//! - `List`, `Stat`, `Walk` (everything under a folder), `Mkdir`, `Remove`
//!   (a file, or a folder with what is in it), `Rename`.
//! - download: `Read {path, offset}` -> `ReadStart {size, mtime, offset}`,
//!   `ReadData` chunks (their sizes are the progress), `ReadEnd`.
//! - upload: `Write {path, size, mtime, on_exists}` -> `WriteReady {offset,
//!   path}` (offset > 0: the target has that much of this file already) or
//!   `WriteSkip {reason}`; then `WriteData` chunks, `WriteEnd` -> `WriteDone`.
//! - `Cancel {id}` stops a transfer: a download answers `ReadEnd {cancelled:
//!   true}` and nothing of it follows; an upload is dropped.
//! - Any failure: `Error {id, message}`, a text for the person.
//!
//! Paths are the target's own, as text; "" is the folder the session starts
//! in. One download and one upload run at a time (the file manager's queue
//! is one after another).

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

/// What a connector without files answers.
pub const NO_FILES: &str = "This kind of computer has no file transfer";

/// Events and commands waiting between a file session and the agent.
pub const QUEUE: usize = 16;

/// An entry of a folder.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub name: String,
    pub dir: bool,
    #[serde(default)]
    pub size: u64,
    /// Seconds since 1970; 0 = not known.
    #[serde(default)]
    pub mtime: u64,
    #[serde(default)]
    pub hidden: bool,
}

/// A file found under a folder: its path from the folder, with `/`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalkFile {
    pub rel: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub mtime: u64,
}

/// A place to start from (a drive, the home folder).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Place {
    pub name: String,
    pub path: String,
}

/// What is known of the target's files when the session opens.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilesInfo {
    /// "windows", "linux", "macos", "android"; "" = not known.
    #[serde(default)]
    pub os: String,
    /// The path separator there: "/" or "\\".
    pub sep: String,
    /// The folder the session starts in, when the connector can tell.
    #[serde(default)]
    pub home: Option<String>,
    #[serde(default)]
    pub roots: Vec<Place>,
}

/// What to do when the file to write is there already (and is another file).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnExists {
    /// Keep both: the new one gets another name.
    #[default]
    Rename,
    Replace,
    Skip,
}

/// What the agent asks of a file session.
#[derive(Clone, Debug, PartialEq)]
pub enum FileCommand {
    List { id: String, path: String, hidden: bool },
    Stat { id: String, path: String },
    Walk { id: String, path: String },
    Mkdir { id: String, path: String },
    Remove { id: String, path: String },
    Rename { id: String, path: String, name: String },
    Read { id: String, path: String, offset: u64 },
    Write { id: String, path: String, size: u64, mtime: u64, on_exists: OnExists },
    WriteData { id: String, data: Bytes },
    WriteEnd { id: String },
    Cancel { id: String },
    Close,
}

/// What a file session tells the agent.
#[derive(Clone, Debug, PartialEq)]
pub enum FileEvent {
    /// The answer to `List`. `parent` is `None` where nothing is above.
    Dir { id: String, path: String, parent: Option<String>, entries: Vec<FileEntry> },
    /// The answer to `Stat`; `None`: nothing is there by that name.
    Stat { id: String, entry: Option<FileEntry> },
    /// The answer to `Walk`: the files under the folder, and its folders.
    Walk { id: String, path: String, files: Vec<WalkFile>, dirs: Vec<String> },
    /// `Mkdir`, `Remove` or `Rename` was done.
    Ok { id: String },
    ReadStart { id: String, size: u64, mtime: u64, offset: u64 },
    ReadData { id: String, data: Bytes },
    ReadEnd { id: String, cancelled: bool },
    WriteReady { id: String, offset: u64, path: String },
    /// Not written: `reason` is "identical" (the same file is there) or "exists".
    WriteSkip { id: String, reason: String, path: String },
    WriteDone { id: String, path: String },
    Error { id: String, message: String },
    /// The end; nothing follows.
    Closed { reason: String },
}

/// A file connection to a target. Dropping it (or `FileCommand::Close`) ends it.
pub struct FileSession {
    pub info: FilesInfo,
    pub events: mpsc::Receiver<FileEvent>,
    pub commands: mpsc::Sender<FileCommand>,
}

/// How the target writes paths, for the string handling a connector needs
/// when its protocol has none (parents, joining, the last name).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathForm {
    pub windows: bool,
}

impl PathForm {
    pub fn sep(self) -> &'static str {
        if self.windows {
            "\\"
        } else {
            "/"
        }
    }

    fn is_sep(self, c: char) -> bool {
        c == '/' || (self.windows && c == '\\')
    }

    /// Where a Windows path's root ends: `C:\` (3) or `\\server\share` (its
    /// length); `None` when it has none.
    fn windows_root(path: &str) -> Option<usize> {
        let b = path.as_bytes();
        if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
            return Some(if b.len() >= 3 && b[2] == b'\\' { 3 } else { 2 });
        }
        let rest = path.strip_prefix(r"\\")?;
        let mut parts = rest.splitn(3, '\\');
        let (server, share) = (parts.next()?, parts.next()?);
        (!server.is_empty() && !share.is_empty()).then(|| 2 + server.len() + 1 + share.len())
    }

    /// The path in one form: the target's separator, none at the end (but
    /// at a root), a bare drive as its root (`C:` -> `C:\`). "" stays "".
    /// On Windows the lone `/` stays (a protocol may list the drives by it)
    /// and a drive under it is the drive (`/C:` -> `C:\`).
    pub fn normalize(self, path: &str) -> String {
        let path = path.trim();
        if !self.windows {
            let trimmed = path.trim_end_matches('/');
            return if trimmed.is_empty() && !path.is_empty() { "/".into() } else { trimmed.to_string() };
        }
        if path.is_empty() || path == "/" || path == "\\" {
            return if path.is_empty() { String::new() } else { "/".into() };
        }
        let mut p = path.replace('/', "\\");
        // "\C:" and "\C:\x": a drive named under the list of drives.
        {
            let b = p.as_bytes();
            if b.len() >= 3 && b[0] == b'\\' && b[1].is_ascii_alphabetic() && b[2] == b':' {
                p.remove(0);
            }
        }
        let b = p.as_bytes();
        if b.len() == 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
            p.push('\\');
            return p;
        }
        let root = Self::windows_root(&p).unwrap_or(0);
        while p.len() > root && p.ends_with('\\') {
            p.pop();
        }
        p
    }

    /// The folder above `path` (normalized); `None` at a root.
    pub fn parent(self, path: &str) -> Option<String> {
        let p = self.normalize(path);
        if p.is_empty() {
            return None;
        }
        if self.windows {
            if p == "/" {
                return None;
            }
            let root = Self::windows_root(&p)?;
            if p.len() <= root {
                return None;
            }
            let cut = p.rfind('\\')?;
            return Some(if cut < root { p[..root].to_string() } else { p[..cut].to_string() });
        }
        if p == "/" {
            return None;
        }
        match p.rfind('/') {
            Some(0) => Some("/".into()),
            Some(cut) => Some(p[..cut].to_string()),
            None => None,
        }
    }

    /// The last name of `path`; "" at a root.
    pub fn name(self, path: &str) -> String {
        let p = self.normalize(path);
        match self.parent(&p) {
            Some(parent) => p[parent.len()..].trim_start_matches(|c| self.is_sep(c)).to_string(),
            None => String::new(),
        }
    }

    /// `name` (one name, or a path from `dir` written with `/`) in `dir`.
    pub fn join(self, dir: &str, name: &str) -> String {
        let name = if self.windows { name.replace('/', "\\") } else { name.to_string() };
        if dir.is_empty() || dir.ends_with(|c| self.is_sep(c)) {
            format!("{}{}", dir, name)
        } else {
            format!("{}{}{}", dir, self.sep(), name)
        }
    }

    /// Whether `name` is one name of a folder's entry: not empty, not "."
    /// or "..", no separator of either kind, no NUL.
    pub fn is_plain_name(name: &str) -> bool {
        !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', '\0'])
    }
}

/// `name (2).ext`, `name (3).ext`, ...: the `n`th other name for a file.
pub fn numbered_name(name: &str, n: u32) -> String {
    match name.rfind('.') {
        Some(i) if i > 0 => format!("{} ({}){}", &name[..i], n, &name[i..]),
        _ => format!("{} ({})", name, n),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIN: PathForm = PathForm { windows: true };
    const UNIX: PathForm = PathForm { windows: false };

    #[test]
    fn windows_paths_as_text() {
        assert_eq!(WIN.normalize("C:"), "C:\\");
        assert_eq!(WIN.normalize("c:/Users/An/"), "c:\\Users\\An");
        assert_eq!(WIN.normalize("C:\\"), "C:\\");
        assert_eq!(WIN.normalize("/"), "/");
        assert_eq!(WIN.normalize(""), "");
        // A drive opened from the list of drives.
        assert_eq!(WIN.normalize("/C:"), "C:\\");
        assert_eq!(WIN.normalize("/D:/Phim"), "D:\\Phim");
        assert_eq!(WIN.normalize(r"\\nas\chung\"), r"\\nas\chung");

        assert_eq!(WIN.parent(r"C:\Users\An"), Some(r"C:\Users".into()));
        assert_eq!(WIN.parent(r"C:\Users"), Some("C:\\".into()));
        assert_eq!(WIN.parent("C:\\"), None);
        assert_eq!(WIN.parent("C:"), None);
        assert_eq!(WIN.parent("/"), None);
        assert_eq!(WIN.parent(r"\\nas\chung\a"), Some(r"\\nas\chung".into()));
        assert_eq!(WIN.parent(r"\\nas\chung"), None);

        assert_eq!(WIN.name(r"C:\Users\Tài liệu.txt"), "Tài liệu.txt");
        assert_eq!(WIN.name("C:\\"), "");
        assert_eq!(WIN.join("C:\\", "a.txt"), r"C:\a.txt");
        assert_eq!(WIN.join(r"C:\Users", "a.txt"), r"C:\Users\a.txt");
        assert_eq!(WIN.join(r"C:\Users", "x/y.txt"), r"C:\Users\x\y.txt");
        assert_eq!(WIN.sep(), "\\");
    }

    #[test]
    fn unix_paths_as_text() {
        assert_eq!(UNIX.normalize("/home/an/"), "/home/an");
        assert_eq!(UNIX.normalize("/"), "/");
        assert_eq!(UNIX.normalize("//"), "/");
        assert_eq!(UNIX.normalize(""), "");
        // A backslash is part of a name there.
        assert_eq!(UNIX.normalize("/tmp/a\\b"), "/tmp/a\\b");
        assert_eq!(UNIX.parent("/home/an"), Some("/home".into()));
        assert_eq!(UNIX.parent("/home"), Some("/".into()));
        assert_eq!(UNIX.parent("/"), None);
        assert_eq!(UNIX.name("/home/an/Ảnh cưới.jpg"), "Ảnh cưới.jpg");
        assert_eq!(UNIX.name("/"), "");
        assert_eq!(UNIX.join("/", "a"), "/a");
        assert_eq!(UNIX.join("/home/an", "x/y.txt"), "/home/an/x/y.txt");
    }

    #[test]
    fn names() {
        assert!(PathForm::is_plain_name("Báo cáo.docx"));
        for bad in ["", ".", "..", "a/b", "a\\b", "a\0"] {
            assert!(!PathForm::is_plain_name(bad), "{:?}", bad);
        }
        assert_eq!(numbered_name("a.txt", 2), "a (2).txt");
        assert_eq!(numbered_name("Makefile", 3), "Makefile (3)");
        assert_eq!(numbered_name(".bashrc", 2), ".bashrc (2)");
        assert_eq!(numbered_name("lưu trữ.tar.gz", 2), "lưu trữ.tar (2).gz");
    }
}
