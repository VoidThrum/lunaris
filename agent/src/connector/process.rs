//! External connectors: a program in `<connectors dir>/<id>/`, described by
//! the `connector.json` beside it, started for one session and spoken to over
//! its standard input and output.
//!
//! The protocol (reference: `connectors/template/README.md`): each message is
//!
//! ```text
//! u32 LE  length of the header
//! u32 LE  length of the payload
//! header  JSON object with a "type"
//! payload bytes (video, sound, cursor pixels, text), may be empty
//! ```
//!
//! and the headers are the agent's `Event`s and `Command`s, one to one
//! (`EventHeader`, `CommandHeader` below). First the agent sends `hello` with
//! its version and the connector answers `hello` with its own; then the
//! agent sends `connect` with the target and the viewer. What the program
//! writes to its standard error goes to the agent's log.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use super::{Button, Capabilities, Codec, Command, Connector, Description, DisplayInfo, Event, Field, Session, Target, Viewer, VERSION};

/// A header or a payload larger than this ends the session (a 4K key frame
/// is a few megabytes).
const MAX_HEADER: usize = 64 * 1024;
const MAX_PAYLOAD: usize = 64 * 1024 * 1024;
const HELLO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// `connector.json`: what the connector is and how it is started.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    /// The program, a path inside the connector's own folder.
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub fields: Vec<Field>,
    #[serde(default)]
    pub capabilities: Capabilities,
}

/// What the agent sends: `hello`, `connect`, then the commands.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CommandHeader {
    Hello { version: u32 },
    Connect { target: Target, viewer: Viewer },
    PointerMove { x: i32, y: i32 },
    PointerMoveRelative { dx: i32, dy: i32 },
    PointerButton { button: Button, down: bool },
    Wheel { dx: i32, dy: i32 },
    Key { position: u32, down: bool },
    /// Payload: the text, UTF-8.
    Text,
    /// Payload: the text, UTF-8.
    Clipboard,
    RequestKeyFrame,
    Quality { fps: u32, bitrate_kbps: u32 },
    SwitchDisplay { index: usize },
    Close,
}

/// What the connector sends: `hello`, then the events.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventHeader {
    Hello { version: u32 },
    /// Payload: the access unit.
    Video {
        codec: Codec,
        key: bool,
        #[serde(default)]
        capture_time_ms: Option<u64>,
    },
    /// Payload: the Opus packet.
    Audio { channels: u8 },
    /// Payload: RGBA pixels.
    CursorShape { width: u32, height: u32, hot_x: u32, hot_y: u32 },
    CursorPosition { x: i32, y: i32, visible: bool },
    CursorEmbedded { embedded: bool },
    Platform { os: String },
    /// Payload: the text, UTF-8.
    Clipboard,
    Displays { displays: Vec<DisplayInfo>, current: usize },
    Closed { reason: String },
}

pub fn command_to_wire(command: &Command) -> (CommandHeader, Bytes) {
    match command {
        Command::PointerMove { x, y } => (CommandHeader::PointerMove { x: *x, y: *y }, Bytes::new()),
        Command::PointerMoveRelative { dx, dy } => (CommandHeader::PointerMoveRelative { dx: *dx, dy: *dy }, Bytes::new()),
        Command::PointerButton { button, down } => (CommandHeader::PointerButton { button: *button, down: *down }, Bytes::new()),
        Command::Wheel { dx, dy } => (CommandHeader::Wheel { dx: *dx, dy: *dy }, Bytes::new()),
        Command::Key { position, down } => (CommandHeader::Key { position: *position, down: *down }, Bytes::new()),
        Command::Text { text } => (CommandHeader::Text, Bytes::copy_from_slice(text.as_bytes())),
        Command::Clipboard { text } => (CommandHeader::Clipboard, Bytes::copy_from_slice(text.as_bytes())),
        Command::RequestKeyFrame => (CommandHeader::RequestKeyFrame, Bytes::new()),
        Command::Quality { fps, bitrate_kbps } => (CommandHeader::Quality { fps: *fps, bitrate_kbps: *bitrate_kbps }, Bytes::new()),
        Command::SwitchDisplay { index } => (CommandHeader::SwitchDisplay { index: *index }, Bytes::new()),
        Command::Close => (CommandHeader::Close, Bytes::new()),
    }
}

/// The event of a message from a connector; `None` for `hello` (not an
/// event) and for text that is not UTF-8.
pub fn event_from_wire(header: EventHeader, payload: Bytes) -> Option<Event> {
    Some(match header {
        EventHeader::Hello { .. } => return None,
        EventHeader::Video { codec, key, capture_time_ms } => Event::Video { codec, key, capture_time_ms, data: payload },
        EventHeader::Audio { channels } => Event::Audio { channels, data: payload },
        EventHeader::CursorShape { width, height, hot_x, hot_y } => {
            if payload.len() as u64 != width as u64 * height as u64 * 4 {
                return None;
            }
            Event::CursorShape { width, height, hot_x, hot_y, rgba: payload }
        }
        EventHeader::CursorPosition { x, y, visible } => Event::CursorPosition { x, y, visible },
        EventHeader::CursorEmbedded { embedded } => Event::CursorEmbedded { embedded },
        EventHeader::Platform { os } => Event::Platform { os },
        EventHeader::Clipboard => Event::Clipboard { text: String::from_utf8(payload.to_vec()).ok()? },
        EventHeader::Displays { displays, current } => Event::Displays { displays, current },
        EventHeader::Closed { reason } => Event::Closed { reason },
    })
}

pub async fn write_message<W: AsyncWrite + Unpin>(out: &mut W, header: &impl Serialize, payload: &[u8]) -> std::io::Result<()> {
    let header = serde_json::to_vec(header).map_err(std::io::Error::other)?;
    let mut buf = Vec::with_capacity(8 + header.len());
    buf.extend_from_slice(&(header.len() as u32).to_le_bytes());
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(&header);
    out.write_all(&buf).await?;
    out.write_all(payload).await?;
    out.flush().await
}

/// The next message; `None` at the end of the stream.
pub async fn read_message<R: AsyncRead + Unpin>(input: &mut R) -> std::io::Result<Option<(Vec<u8>, Bytes)>> {
    let mut lengths = [0u8; 8];
    match input.read_exact(&mut lengths).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let header_len = u32::from_le_bytes(lengths[..4].try_into().unwrap()) as usize;
    let payload_len = u32::from_le_bytes(lengths[4..].try_into().unwrap()) as usize;
    if header_len > MAX_HEADER || payload_len > MAX_PAYLOAD {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "message too large"));
    }
    let mut header = vec![0u8; header_len];
    input.read_exact(&mut header).await?;
    let mut payload = vec![0u8; payload_len];
    input.read_exact(&mut payload).await?;
    Ok(Some((header, Bytes::from(payload))))
}

/// An external connector, as its manifest describes it.
pub struct ProcessConnector {
    manifest: Manifest,
    program: PathBuf,
    dir: PathBuf,
}

impl ProcessConnector {
    /// Reads `dir/<id>/connector.json`. Nothing is run.
    pub fn load(dir: &Path, id: &str) -> anyhow::Result<Self> {
        if !super::valid_id(id) {
            anyhow::bail!("\"{}\" is not a connector's name", id);
        }
        let folder = dir.join(id);
        let text = std::fs::read_to_string(folder.join("connector.json"))
            .map_err(|e| anyhow::anyhow!("No connector \"{}\" is installed ({}: {})", id, folder.display(), e))?;
        let manifest: Manifest = serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("connector.json of \"{}\": {}", id, e))?;
        if manifest.id != id {
            anyhow::bail!("connector.json in {} is of connector \"{}\"", folder.display(), manifest.id);
        }
        // The program is a file of the connector's own folder: a manifest
        // cannot point the agent at something else on the computer.
        let folder = folder.canonicalize()?;
        let program = folder.join(&manifest.command).canonicalize().map_err(|e| {
            anyhow::anyhow!("The program of connector \"{}\" ({}) is not there: {}", id, manifest.command, e)
        })?;
        if !program.starts_with(&folder) || !program.is_file() {
            anyhow::bail!("The program of connector \"{}\" must be a file in its own folder", id);
        }
        Ok(ProcessConnector { manifest, program, dir: folder })
    }
}

#[async_trait::async_trait]
impl Connector for ProcessConnector {
    fn describe(&self) -> Description {
        Description {
            id: self.manifest.id.clone(),
            name: self.manifest.name.clone(),
            fields: self.manifest.fields.clone(),
            capabilities: self.manifest.capabilities.clone(),
        }
    }

    async fn connect(&self, target: &Target, viewer: &Viewer) -> anyhow::Result<Session> {
        let id = self.manifest.id.clone();
        let mut child = tokio::process::Command::new(&self.program)
            .args(&self.manifest.args)
            .current_dir(&self.dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| anyhow::anyhow!("Connector \"{}\" could not be started: {}", id, e))?;
        let mut stdin = child.stdin.take().expect("piped");
        let mut stdout = child.stdout.take().expect("piped");
        if let Some(stderr) = child.stderr.take() {
            let id = id.clone();
            tokio::spawn(async move {
                use tokio::io::AsyncBufReadExt;
                let mut lines = tokio::io::BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    debug!("connector {}: {}", id, line);
                }
            });
        }

        // hello, both ways: the versions must be the same.
        write_message(&mut stdin, &CommandHeader::Hello { version: VERSION }, &[]).await?;
        let hello = tokio::time::timeout(HELLO_TIMEOUT, read_message(&mut stdout))
            .await
            .map_err(|_| anyhow::anyhow!("Connector \"{}\" did not answer", id))??;
        match hello.and_then(|(header, _)| serde_json::from_slice::<EventHeader>(&header).ok()) {
            Some(EventHeader::Hello { version }) if version == VERSION => {}
            Some(EventHeader::Hello { version }) => {
                anyhow::bail!("Connector \"{}\" speaks version {} of the protocol, this agent version {}", id, version, VERSION)
            }
            _ => anyhow::bail!("Connector \"{}\" did not say hello", id),
        }
        write_message(&mut stdin, &CommandHeader::Connect { target: target.clone(), viewer: viewer.clone() }, &[]).await?;

        let (event_tx, events) = mpsc::channel::<Event>(64);
        let (commands, mut command_rx) = mpsc::channel::<Command>(64);
        // Commands to the program; the end of the commands ends it.
        tokio::spawn(async move {
            while let Some(command) = command_rx.recv().await {
                let close = command == Command::Close;
                let (header, payload) = command_to_wire(&command);
                if write_message(&mut stdin, &header, &payload).await.is_err() || close {
                    break;
                }
            }
            drop(stdin);
            // It has a moment to leave by itself, then it is stopped.
            if tokio::time::timeout(std::time::Duration::from_secs(2), child.wait()).await.is_err() {
                let _ = child.kill().await;
            }
        });
        // Events from the program.
        tokio::spawn(async move {
            let reason = loop {
                let (header, payload) = match read_message(&mut stdout).await {
                    Ok(Some(message)) => message,
                    Ok(None) => break "The connector ended".to_string(),
                    Err(e) => break format!("The connector's output is broken: {}", e),
                };
                let header = match serde_json::from_slice::<EventHeader>(&header) {
                    Ok(header) => header,
                    Err(e) => {
                        warn!("connector {}: a message that is not understood: {}", id, e);
                        continue;
                    }
                };
                let Some(event) = event_from_wire(header, payload) else { continue };
                let closed = matches!(event, Event::Closed { .. });
                if event_tx.send(event).await.is_err() || closed {
                    return;
                }
            };
            let _ = event_tx.send(Event::Closed { reason }).await;
        });
        Ok(Session { events, commands })
    }
}
