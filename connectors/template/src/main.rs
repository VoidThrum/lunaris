//! A minimal external connector for the Lunaris agent, to copy and start
//! from. It "connects" to nothing: it plays a recorded picture (an IVF file
//! of VP8, VP9 or AV1 frames) in a loop, with silence for sound, a cursor
//! going round, and it answers every command, so that each message of the
//! protocol is seen once. The protocol is described in README.md.
//!
//! Only the standard library, serde and serde_json.

use std::io::{self, Read, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// The version of the protocol this program speaks.
const VERSION: u32 = 1;

/// What the agent sends. A type this program does not know fails to parse
/// and is skipped: an agent with a newer protocol of the same version may
/// add messages.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum FromAgent {
    Hello { version: u32 },
    Connect { target: Target, viewer: Viewer },
    PointerMove { x: i32, y: i32 },
    PointerMoveRelative { dx: i32, dy: i32 },
    PointerButton { button: String, down: bool },
    Wheel { dx: i32, dy: i32 },
    Key {
        /// Where the key is on the keyboard (a Linux input key code).
        #[allow(dead_code)]
        position: u32,
        down: bool,
    },
    /// Payload: the text.
    Text,
    /// Payload: the text.
    Clipboard,
    RequestKeyFrame,
    Quality {
        fps: u32,
        #[allow(dead_code)]
        bitrate_kbps: u32,
    },
    SwitchDisplay { index: usize },
    Close,
}

#[derive(Debug, Deserialize)]
struct Target {
    #[allow(dead_code)]
    connector: String,
    #[serde(default)]
    file: String,
    #[serde(default)]
    fps: String,
    /// A second recording to change to after `then_after` seconds: with
    /// another codec it shows what the agent does when a computer changes
    /// its video codec in the middle of a session.
    #[serde(default)]
    then_file: String,
    #[serde(default)]
    then_after: String,
}

#[derive(Debug, Deserialize)]
struct Viewer {
    codecs: Vec<String>,
    fps: u32,
}

/// What this program sends.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ToAgent<'a> {
    Hello { version: u32 },
    /// Payload: one encoded frame.
    Video { codec: &'a str, key: bool, capture_time_ms: u64 },
    /// Payload: one Opus packet.
    Audio { channels: u8 },
    /// Payload: width * height * 4 bytes, RGBA.
    CursorShape { width: u32, height: u32, hot_x: u32, hot_y: u32 },
    CursorPosition { x: i32, y: i32, visible: bool },
    /// Payload: the text.
    Clipboard,
    Displays { displays: Vec<Display<'a>>, current: usize },
    Closed { reason: &'a str },
}

#[derive(Debug, Serialize)]
struct Display<'a> {
    name: &'a str,
    width: u32,
    height: u32,
}

/// One message: both lengths (u32, little endian), the JSON header, the payload.
fn send(header: &ToAgent, payload: &[u8]) -> io::Result<()> {
    let header = serde_json::to_vec(header)?;
    let mut out = io::stdout().lock();
    out.write_all(&(header.len() as u32).to_le_bytes())?;
    out.write_all(&(payload.len() as u32).to_le_bytes())?;
    out.write_all(&header)?;
    out.write_all(payload)?;
    out.flush()
}

/// The next message from the agent; `None` when it closed our input.
fn receive(input: &mut impl Read) -> io::Result<Option<(Vec<u8>, Vec<u8>)>> {
    let mut lengths = [0u8; 8];
    match input.read_exact(&mut lengths) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let header_len = u32::from_le_bytes(lengths[..4].try_into().unwrap()) as usize;
    let payload_len = u32::from_le_bytes(lengths[4..].try_into().unwrap()) as usize;
    if header_len > 64 * 1024 || payload_len > 64 * 1024 * 1024 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "message too large"));
    }
    let mut header = vec![0u8; header_len];
    input.read_exact(&mut header)?;
    let mut payload = vec![0u8; payload_len];
    input.read_exact(&mut payload)?;
    Ok(Some((header, payload)))
}

/// A recorded picture: the frames of an IVF file.
struct Recording {
    codec: &'static str,
    width: u32,
    height: u32,
    /// (key frame, data)
    frames: Vec<(bool, Vec<u8>)>,
}

fn load_ivf(path: &std::path::Path) -> Result<Recording, String> {
    let data = std::fs::read(path).map_err(|e| format!("{}: {}", path.display(), e))?;
    if data.len() < 32 || &data[..4] != b"DKIF" {
        return Err(format!("{} is not an IVF file", path.display()));
    }
    let codec = match &data[8..12] {
        b"VP80" => "vp8",
        b"VP90" => "vp9",
        b"AV01" => "av1",
        other => return Err(format!("IVF codec {:?} is not VP8, VP9 or AV1", String::from_utf8_lossy(other))),
    };
    let header_len = u16::from_le_bytes([data[6], data[7]]) as usize;
    let width = u16::from_le_bytes([data[12], data[13]]) as u32;
    let height = u16::from_le_bytes([data[14], data[15]]) as u32;
    let mut frames = Vec::new();
    let mut at = header_len;
    while at + 12 <= data.len() {
        let size = u32::from_le_bytes(data[at..at + 4].try_into().unwrap()) as usize;
        at += 12;
        if size == 0 || at + size > data.len() {
            break;
        }
        let frame = &data[at..at + size];
        at += size;
        let key = match codec {
            // RFC 6386 section 9.1: the low bit of the first byte is 0 in a key frame.
            "vp8" => frame[0] & 1 == 0,
            // VP9 uncompressed header, profile 0: marker (2 bits), profile (2),
            // show_existing_frame (1), frame_type (1; 0 is a key frame).
            "vp9" => frame[0] & 0x08 == 0 && frame[0] & 0x04 == 0,
            // AV1: only the file's first frame is taken as one.
            _ => frames.is_empty(),
        };
        frames.push((key, frame.to_vec()));
    }
    if frames.first().map(|f| f.0) != Some(true) {
        return Err(format!("{} does not start with a key frame", path.display()));
    }
    Ok(Recording { codec, width, height, frames })
}

/// A 16 x 16 white arrow-like triangle with a black edge.
fn cursor_rgba() -> Vec<u8> {
    let mut rgba = vec![0u8; 16 * 16 * 4];
    for y in 0..16usize {
        for x in 0..=y.min(11) {
            let edge = x == 0 || x == y || y == 15;
            let pixel = &mut rgba[(y * 16 + x) * 4..][..4];
            pixel.copy_from_slice(if edge { &[0, 0, 0, 255] } else { &[255, 255, 255, 255] });
        }
    }
    rgba
}

/// 20 ms of silence as an Opus packet (CELT, fullband, mono).
const OPUS_SILENCE: [u8; 3] = [0xF8, 0xFF, 0xFE];

fn main() {
    if let Err(e) = run() {
        // Standard error goes to the agent's log. Never write there what a
        // person typed (keys, text, the clipboard) or a password.
        eprintln!("template connector: {}", e);
        let _ = send(&ToAgent::Closed { reason: &e }, &[]);
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut stdin = io::stdin().lock();
    let io_err = |e: io::Error| e.to_string();

    // 1. hello, both ways. The agent goes on only if the versions are the same.
    let Some((header, _)) = receive(&mut stdin).map_err(io_err)? else { return Ok(()) };
    match serde_json::from_slice::<FromAgent>(&header) {
        Ok(FromAgent::Hello { version }) => {
            send(&ToAgent::Hello { version: VERSION }, &[]).map_err(io_err)?;
            if version != VERSION {
                return Err(format!("the agent speaks version {}, this connector version {}", version, VERSION));
            }
        }
        _ => return Err("the first message was not hello".into()),
    }

    // 2. connect: the target (this connector's own fields) and the viewer.
    let Some((header, _)) = receive(&mut stdin).map_err(io_err)? else { return Ok(()) };
    let (target, viewer) = match serde_json::from_slice::<FromAgent>(&header) {
        Ok(FromAgent::Connect { target, viewer }) => (target, viewer),
        _ => return Err("the second message was not connect".into()),
    };
    let file = if target.file.is_empty() { "pattern.ivf" } else { &target.file };
    let mut recording = load_ivf(std::path::Path::new(file))?;
    let mut change = match (target.then_file.is_empty(), target.then_after.parse::<u64>()) {
        (false, Ok(seconds)) => Some((load_ivf(std::path::Path::new(&target.then_file))?, Duration::from_secs(seconds))),
        _ => None,
    };
    // A connector must send a codec the viewer takes: the agent does not transcode.
    if !viewer.codecs.iter().any(|c| c == recording.codec) {
        return Err(format!("the viewer does not take {} (it takes {:?})", recording.codec, viewer.codecs));
    }
    let mut fps = target.fps.parse::<u32>().ok().filter(|f| *f > 0).unwrap_or(viewer.fps.clamp(1, 30));
    eprintln!(
        "template connector: playing {} ({}, {}x{}, {} frames) at {} frames a second",
        file,
        recording.codec,
        recording.width,
        recording.height,
        recording.frames.len(),
        fps
    );

    // 3. What there is to see, once, then the stream.
    send(
        &ToAgent::Displays { displays: vec![Display { name: "Test pattern", width: recording.width, height: recording.height }], current: 0 },
        &[],
    )
    .map_err(io_err)?;
    send(&ToAgent::CursorShape { width: 16, height: 16, hot_x: 0, hot_y: 0 }, &cursor_rgba()).map_err(io_err)?;

    // Commands are read on their own thread; the stream is timed here.
    drop(stdin);
    let (commands_tx, commands) = mpsc::channel::<Option<(FromAgent, Vec<u8>)>>();
    std::thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        loop {
            match receive(&mut stdin) {
                Ok(Some((header, payload))) => {
                    // What is not understood is skipped.
                    if let Ok(command) = serde_json::from_slice::<FromAgent>(&header) {
                        if commands_tx.send(Some((command, payload))).is_err() {
                            return;
                        }
                    }
                }
                _ => {
                    let _ = commands_tx.send(None);
                    return;
                }
            }
        }
    });

    let start = Instant::now();
    let mut next_frame = start;
    let mut next_sound = start;
    let mut index = 0usize;
    let mut shown = 0u64;
    loop {
        // Commands, until the next thing is due.
        let due = next_frame.min(next_sound);
        match commands.recv_timeout(due.saturating_duration_since(Instant::now())) {
            Ok(Some((command, payload))) => match command {
                FromAgent::Close => {
                    send(&ToAgent::Closed { reason: "Closed" }, &[]).map_err(io_err)?;
                    return Ok(());
                }
                // A recording can only start again at its key frame.
                FromAgent::RequestKeyFrame => index = 0,
                FromAgent::Quality { fps: wanted, .. } => {
                    if wanted > 0 {
                        fps = wanted.min(120);
                    }
                }
                // A real connector hands these to the other computer. This one
                // only shows that they arrived: the clipboard comes back as
                // the "remote" clipboard, the rest is counted.
                FromAgent::Clipboard => send(&ToAgent::Clipboard, &payload).map_err(io_err)?,
                FromAgent::PointerMove { x, y } => send(&ToAgent::CursorPosition { x, y, visible: true }, &[]).map_err(io_err)?,
                FromAgent::PointerMoveRelative { dx, dy } => eprintln!("template connector: pointer moved by {} {}", dx, dy),
                FromAgent::PointerButton { button, down } => eprintln!("template connector: button {} {}", button, if down { "down" } else { "up" }),
                FromAgent::Wheel { dx, dy } => eprintln!("template connector: wheel {} {}", dx, dy),
                // Which key or what text is never written to a log.
                FromAgent::Key { position: _, down } => eprintln!("template connector: a key {}", if down { "down" } else { "up" }),
                FromAgent::Text => eprintln!("template connector: text of {} bytes", payload.len()),
                FromAgent::SwitchDisplay { index } => eprintln!("template connector: display {} asked for (there is one)", index),
                FromAgent::Hello { .. } | FromAgent::Connect { .. } => {}
            },
            // The agent closed our input: the session is over.
            Ok(None) | Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }

        let now = Instant::now();
        if now >= next_sound {
            send(&ToAgent::Audio { channels: 1 }, &OPUS_SILENCE).map_err(io_err)?;
            next_sound += Duration::from_millis(20);
        }
        if now >= next_frame {
            if change.as_ref().is_some_and(|(_, after)| now.duration_since(start) >= *after) {
                // The other recording, from its key frame.
                recording = change.take().expect("checked").0;
                index = 0;
            }
            let (key, data) = &recording.frames[index];
            let capture_time_ms = now.duration_since(start).as_millis() as u64;
            send(&ToAgent::Video { codec: recording.codec, key: *key, capture_time_ms }, data).map_err(io_err)?;
            index = (index + 1) % recording.frames.len();
            shown += 1;
            if shown % 15 == 0 {
                let angle = shown as f64 / 30.0;
                let x = (recording.width as f64 * (0.5 + 0.4 * angle.cos())) as i32;
                let y = (recording.height as f64 * (0.5 + 0.4 * angle.sin())) as i32;
                send(&ToAgent::CursorPosition { x, y, visible: true }, &[]).map_err(io_err)?;
            }
            next_frame += Duration::from_secs_f64(1.0 / fps as f64);
            // After a pause (the agent not reading) the stream goes on from now.
            if next_frame + Duration::from_millis(500) < Instant::now() {
                next_frame = Instant::now();
            }
        }
    }
}
