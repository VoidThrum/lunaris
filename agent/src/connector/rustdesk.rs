//! The RustDesk connector: the agent as a RustDesk *client*, connecting to a
//! RustDesk host by its address on the LAN ("direct IP access" on the host,
//! TCP port 21118).
//!
//! Ported from RustDesk 1.5.3 (AGPL-3.0, https://github.com/rustdesk/rustdesk),
//! the client's side: `src/client.rs` and `src/client/io_loop.rs`. Each step
//! names the function it follows. The framing, the messages and the login
//! proof are the ones the agent's RustDesk listener uses
//! (`crate::rustdesk::{frame, proto, login}`).
//!
//! It carries the picture, the sound, the cursor, clipboard text both ways,
//! the pointer, keys (map mode, to Windows and Linux hosts), typed text and
//! the choice of the display. Files are not carried.

use std::time::Duration;

use bytes::{Bytes, BytesMut};
use prost::Message as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::{keys, Button, Capabilities, Codec, Command, Connector, Description, DisplayInfo, Event, Field, Session, Target, Viewer};
use crate::rustdesk::proto::{
    key_event, login_response, message, misc, supported_decoding::PreferCodec, video_frame, ControlKey, CursorData,
    EncodedVideoFrames, Hash, KeyEvent, KeyboardMode, LoginRequest, Message, Misc, MouseEvent, OptionMessage, PeerInfo,
    SupportedDecoding, SwitchDisplay,
};
use crate::rustdesk::{clipboard, cursor, frame, login, qos};

/// hbb_common/src/config.rs: `RELAY_PORT` + 1, the port of direct access
/// (client.rs `Client::_start`: `check_port(peer, RELAY_PORT + 1)`).
pub const DEFAULT_PORT: u16 = 21118;
/// hbb_common/src/config.rs `CONNECT_TIMEOUT` and `READ_TIMEOUT`.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(18_000);
const READ_TIMEOUT: Duration = Duration::from_millis(18_000);
/// The version of the client this follows; a host chooses what it sends by it.
const CLIENT_VERSION: &str = "1.5.3";
/// common.rs `MIN_VER_MULTI_UI_SESSION`.
const MIN_VER_MULTI_UI_SESSION: &str = "1.2.4";

pub struct RustdeskConnector;

#[async_trait::async_trait]
impl Connector for RustdeskConnector {
    fn describe(&self) -> Description {
        let field = |key: &str, label: &str, kind: &str, default: &str, required: bool| Field {
            key: key.into(),
            label: label.into(),
            kind: kind.into(),
            default: default.into(),
            required,
        };
        Description {
            id: "rustdesk".into(),
            name: "RustDesk (by IP address)".into(),
            fields: vec![
                field("host", "IP address", "text", "", true),
                field("port", "Port", "port", &DEFAULT_PORT.to_string(), false),
                field("password", "Password", "secret", "", true),
            ],
            capabilities: Capabilities {
                video: vec![Codec::H264, Codec::H265, Codec::Vp9, Codec::Vp8, Codec::Av1],
                audio: true,
                input: true,
                clipboard: true,
                cursor: true,
            },
        }
    }

    async fn connect(&self, target: &Target, viewer: &Viewer) -> anyhow::Result<Session> {
        let host = target.get("host").trim();
        if host.is_empty() {
            anyhow::bail!("The computer's IP address is missing");
        }
        let port = match target.get("port").trim() {
            "" => DEFAULT_PORT,
            port => port.parse::<u16>().map_err(|_| anyhow::anyhow!("\"{}\" is not a port", port))?,
        };
        if viewer.codecs.is_empty() {
            anyhow::bail!("The viewer decodes no video codec");
        }
        let address = format!("{}:{}", host, port);

        // client.rs Client::_start, the `is_ip_str` branch: a plain TCP
        // connection, no rendezvous and no secure_connection (key exchange).
        // The errors are for the viewer, who is not told where the computer
        // is: the address stays in this computer's log.
        let stream = match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&address)).await {
            Ok(Ok(stream)) => stream,
            Ok(Err(e)) => {
                warn!("RustDesk connector: {} cannot be reached: {}", address, e);
                anyhow::bail!("The RustDesk computer cannot be reached (is it on, and is direct IP access switched on there?)");
            }
            Err(_) => {
                warn!("RustDesk connector: {} does not answer", address);
                anyhow::bail!("The RustDesk computer does not answer");
            }
        };
        let _ = stream.set_nodelay(true);
        let mut conn = Conn { stream, decoder: frame::Decoder::new(frame::MAX_FRAME_LENGTH), buf: BytesMut::with_capacity(64 * 1024) };

        // io_loop.rs handle_msg_from_peer: the host's Hash, then our
        // LoginRequest, then its LoginResponse.
        let peer_info = loop {
            let msg = tokio::time::timeout(READ_TIMEOUT, conn.next())
                .await
                .map_err(|_| anyhow::anyhow!("The RustDesk computer did not answer the login"))?
                .map_err(|e| anyhow::anyhow!("The connection to the RustDesk computer broke: {}", e.kind()))?
                .ok_or_else(|| anyhow::anyhow!("The RustDesk computer closed the connection"))?;
            match msg.union {
                Some(message::Union::Hash(hash)) => {
                    conn.send(&login_message(&address, target.get("password"), &hash, viewer)).await?;
                }
                Some(message::Union::LoginResponse(response)) => match response.union {
                    Some(login_response::Union::Error(error)) => anyhow::bail!("{}", login_error(&error)),
                    Some(login_response::Union::PeerInfo(info)) => break info,
                    None => {}
                },
                // client.rs handle_test_delay.
                Some(message::Union::TestDelay(delay)) if !delay.from_client => {
                    conn.send(&Message { union: Some(message::Union::TestDelay(delay)) }).await?;
                }
                // A host that asks someone at the computer first says so
                // (io_loop.rs: Misc::CloseReason ends the session).
                Some(message::Union::Misc(Misc { union: Some(misc::Union::CloseReason(reason)) })) => {
                    anyhow::bail!("{}", if reason.is_empty() { "The computer closed the connection".to_string() } else { reason })
                }
                _ => {}
            }
        };
        info!(
            "RustDesk connector: logged in to {} (version {}, platform {}, {} displays)",
            address,
            peer_info.version,
            peer_info.platform,
            peer_info.displays.len()
        );

        let (event_tx, events) = mpsc::channel::<Event>(64);
        let (commands, command_rx) = mpsc::channel::<Command>(64);
        let _ = event_tx.try_send(displays_event(&peer_info));
        let wanted = viewer.codecs.clone();
        tokio::spawn(async move {
            let reason = run(conn, peer_info, wanted, &event_tx, command_rx).await;
            debug!("RustDesk connector: session with {} ended: {}", address, reason);
            let _ = event_tx.send(Event::Closed { reason }).await;
        });
        Ok(Session { events, commands })
    }
}

struct Conn {
    stream: TcpStream,
    decoder: frame::Decoder,
    buf: BytesMut,
}

impl Conn {
    /// hbb_common/src/bytes_codec.rs: one framed protobuf `Message`.
    async fn send(&mut self, msg: &Message) -> std::io::Result<()> {
        let len = msg.encoded_len();
        let mut buf = BytesMut::with_capacity(len + 4);
        frame::put_header(len, &mut buf)?;
        msg.encode(&mut buf).map_err(std::io::Error::other)?;
        self.stream.write_all(&buf).await
    }

    /// The next message; `None` when the host closed the connection.
    async fn next(&mut self) -> std::io::Result<Option<Message>> {
        loop {
            if let Some(payload) = self.decoder.decode(&mut self.buf)? {
                match Message::decode(payload.freeze()) {
                    Ok(msg) => return Ok(Some(msg)),
                    // io_loop.rs handle_msg_from_peer: what does not parse is skipped.
                    Err(_) => continue,
                }
            }
            if self.stream.read_buf(&mut self.buf).await? == 0 {
                return Ok(None);
            }
        }
    }
}

/// scrap codec.rs `Decoder::supported_decodings`, for a viewer that is a
/// WebRTC receiver: the abilities are the codecs it negotiated, the
/// preference its first.
fn supported_decoding(viewer: &Viewer) -> SupportedDecoding {
    let has = |codec| viewer.codecs.contains(&codec) as i32;
    SupportedDecoding {
        ability_vp8: has(Codec::Vp8),
        ability_vp9: has(Codec::Vp9),
        ability_av1: has(Codec::Av1),
        ability_h264: has(Codec::H264),
        ability_h265: has(Codec::H265),
        prefer: match viewer.codecs.first() {
            Some(Codec::H264) => PreferCodec::H264,
            Some(Codec::H265) => PreferCodec::H265,
            Some(Codec::Vp8) => PreferCodec::Vp8,
            Some(Codec::Vp9) => PreferCodec::Vp9,
            Some(Codec::Av1) => PreferCodec::Av1,
            None => PreferCodec::Auto,
        } as i32,
        ..Default::default()
    }
}

/// client.rs `handle_hash` with a preset password, then
/// `handle_login_from_ui` and `LoginConfigHandler::create_login_msg`:
/// the password goes as sha256(sha256(password + salt) + challenge).
fn login_message(address: &str, password: &str, hash: &Hash, viewer: &Viewer) -> Message {
    let proof = login::proof(&login::h1(password, &hash.salt), &hash.challenge);
    Message {
        union: Some(message::Union::LoginRequest(LoginRequest {
            // create_login_msg: `username` is the peer's id, here its address.
            username: address.to_string(),
            password: Bytes::copy_from_slice(&proof[..]),
            my_id: "lunaris".into(),
            my_name: "Lunaris".into(),
            my_platform: match std::env::consts::OS {
                "windows" => "Windows",
                "macos" => "Mac OS",
                _ => "Linux",
            }
            .into(),
            version: CLIENT_VERSION.into(),
            session_id: u64::from_le_bytes(crate::pairing::random_bytes(8).try_into().unwrap_or_default()),
            // client.rs get_option_message(true): the frame rate wanted and
            // what can be decoded.
            option: Some(OptionMessage {
                custom_fps: viewer.fps as i32,
                supported_decoding: Some(supported_decoding(viewer)),
                ..Default::default()
            }),
            ..Default::default()
        })),
    }
}

/// client.rs `handle_login_error`: the host's words, with what they mean
/// for someone who set the target up.
fn login_error(error: &str) -> String {
    match error {
        login::LOGIN_MSG_PASSWORD_WRONG => "Wrong password for the RustDesk computer".to_string(),
        login::LOGIN_MSG_NO_PASSWORD_ACCESS => "The RustDesk computer does not accept a password (only a click at the computer)".to_string(),
        other => other.to_string(),
    }
}

fn displays_event(info: &PeerInfo) -> Event {
    Event::Displays {
        displays: info
            .displays
            .iter()
            .map(|d| DisplayInfo { name: d.name.clone(), width: d.width.max(0) as u32, height: d.height.max(0) as u32 })
            .collect(),
        current: info.current_display.max(0) as usize,
    }
}

fn misc(union: misc::Union) -> Message {
    Message { union: Some(message::Union::Misc(Misc { union: Some(union) })) }
}

/// ui_session_interface.rs `refresh_video`: `LoginConfigHandler::refresh_display`
/// for a peer from 1.2.4 on, else `LoginConfigHandler::refresh`.
fn refresh_message(peer_version: &str, display: i32) -> Message {
    use crate::rustdesk::clipboard::get_version_number;
    if get_version_number(peer_version) >= get_version_number(MIN_VER_MULTI_UI_SESSION) {
        misc(misc::Union::RefreshVideoDisplay(display))
    } else {
        misc(misc::Union::RefreshVideo(true))
    }
}

/// The frames of a `VideoFrame` and their codec; `None` for raw pictures
/// (rgb, yuv), which no host sends to a client of this age.
fn video_frames(union: video_frame::Union) -> Option<(Codec, EncodedVideoFrames)> {
    Some(match union {
        video_frame::Union::H264s(frames) => (Codec::H264, frames),
        video_frame::Union::H265s(frames) => (Codec::H265, frames),
        video_frame::Union::Vp8s(frames) => (Codec::Vp8, frames),
        video_frame::Union::Vp9s(frames) => (Codec::Vp9, frames),
        video_frame::Union::Av1s(frames) => (Codec::Av1, frames),
        _ => return None,
    })
}

// common.rs `input`: the mouse event's mask is `buttons << 3 | type`.
const MOUSE_TYPE_MOVE: i32 = 0;
const MOUSE_TYPE_DOWN: i32 = 1;
const MOUSE_TYPE_UP: i32 = 2;
const MOUSE_TYPE_WHEEL: i32 = 3;
const MOUSE_TYPE_MOVE_RELATIVE: i32 = 5;
const MOUSE_BUTTON_LEFT: i32 = 0x01;
const MOUSE_BUTTON_RIGHT: i32 = 0x02;
const MOUSE_BUTTON_WHEEL: i32 = 0x04;
const MOUSE_BUTTON_BACK: i32 = 0x08;
const MOUSE_BUTTON_FORWARD: i32 = 0x10;

/// io_loop.rs `decode_cursor_data`: the largest cursor taken.
const MAX_CURSOR_SIZE: i32 = 512;
/// Cursor shapes kept to answer a `CursorId` (the host sends each once).
const MAX_KEPT_CURSORS: usize = 64;
/// The image quality is changed for the viewer's connection at most this
/// often, and only by a tenth or more.
const QUALITY_INTERVAL: Duration = Duration::from_secs(1);

/// What the client knows of the host and of its own keyboard.
struct Remote {
    version: String,
    /// keyboard.rs: the peer's platform, lower case without spaces
    /// ("windows", "linux", "macos", "android").
    platform: String,
    displays: Vec<crate::rustdesk::proto::DisplayInfo>,
    display: i32,
    codec: Option<Codec>,
    /// The modifier keys held (by position), for the mouse events.
    held: Vec<u32>,
    /// The viewer's lock keys as this client counts them: off and on at
    /// the start, toggled by the viewer's own CapsLock and NumLock keys.
    caps_lock: bool,
    num_lock: bool,
    cursors: Vec<(u64, Event)>,
    quality_sent: Option<(i32, std::time::Instant)>,
    keys_unsendable_logged: bool,
}

impl Remote {
    fn new(info: &PeerInfo) -> Self {
        let mut platform = info.platform.to_lowercase();
        platform.retain(|c| !c.is_whitespace());
        Remote {
            version: info.version.clone(),
            platform,
            displays: info.displays.clone(),
            display: info.current_display,
            codec: None,
            held: Vec::new(),
            caps_lock: false,
            num_lock: true,
            cursors: Vec::new(),
            quality_sent: None,
            keys_unsendable_logged: false,
        }
    }

    /// Where the display shown is on the host's desktop, and its size.
    fn shown(&self) -> (i32, i32, i32, i32) {
        self.displays.get(self.display.max(0) as usize).map(|d| (d.x, d.y, d.width, d.height)).unwrap_or((0, 0, 0, 0))
    }

    fn displays_event(&self) -> Event {
        Event::Displays {
            displays: self
                .displays
                .iter()
                .map(|d| DisplayInfo { name: d.name.clone(), width: d.width.max(0) as u32, height: d.height.max(0) as u32 })
                .collect(),
            current: self.display.max(0) as usize,
        }
    }

    /// client.rs `send_mouse`: the modifiers held go with every mouse event.
    fn mouse(&self, mask: i32, x: i32, y: i32) -> Message {
        let held = |keys: [u32; 2]| self.held.iter().any(|k| keys.contains(k));
        let mut modifiers = Vec::new();
        if held([56, 100]) {
            modifiers.push(ControlKey::Alt as i32);
        }
        if held([42, 54]) {
            modifiers.push(ControlKey::Shift as i32);
        }
        if held([29, 97]) {
            modifiers.push(ControlKey::Control as i32);
        }
        if held([125, 126]) {
            modifiers.push(ControlKey::Meta as i32);
        }
        Message { union: Some(message::Union::MouseEvent(MouseEvent { mask, x, y, modifiers })) }
    }

    /// keyboard.rs `event_to_key_events` in map mode: `map_keyboard_mode`
    /// (`windows_peer_special_key`, `_map_keyboard_mode` as a Linux client
    /// has it) and `parse_add_lock_modes_modifiers`.
    fn key(&mut self, position: u32, down: bool) -> Option<Message> {
        const MODIFIERS: [u32; 8] = [29, 97, 42, 54, 56, 100, 125, 126];
        if MODIFIERS.contains(&position) {
            self.held.retain(|k| *k != position);
            if down {
                self.held.push(position);
            }
        }
        if down && position == keys::KEY_CAPSLOCK {
            self.caps_lock = !self.caps_lock;
        }
        if down && position == keys::KEY_NUMLOCK {
            self.num_lock = !self.num_lock;
        }
        let mut event = KeyEvent { down, mode: KeyboardMode::Map as i32, ..Default::default() };
        // windows_peer_special_key: Windows has no scan code for Pause.
        const KEY_PAUSE: u32 = 119;
        if self.platform == "windows" && position == KEY_PAUSE {
            event.mode = KeyboardMode::Legacy as i32;
            event.union = Some(key_event::Union::ControlKey(ControlKey::Pause as i32));
            return Some(Message { union: Some(message::Union::KeyEvent(event)) });
        }
        let keycode = match self.platform.as_str() {
            // rdev::linux_code_to_win_scancode
            "windows" => keys::win_scancode_of(position)?,
            // The X keycode (`event.position_code` of a Linux client).
            "linux" => position + 8,
            // macOS and Android hosts take their own key codes
            // (rdev::linux_code_to_macos_code, ..._to_android_key_code),
            // whose tables are not here yet.
            _ => {
                if !self.keys_unsendable_logged {
                    self.keys_unsendable_logged = true;
                    warn!("RustDesk connector: keys are not sent to a {} computer yet", self.platform);
                }
                return None;
            }
        };
        event.union = Some(key_event::Union::Chr(keycode));
        if (keys::is_letter(position) || matches!(position, 26 | 27 | 39 | 40 | 51 | 52)) && self.caps_lock {
            event.modifiers.push(ControlKey::CapsLock as i32);
        }
        if keys::is_numpad(position) && self.num_lock {
            event.modifiers.push(ControlKey::NumLock as i32);
        }
        Some(Message { union: Some(message::Union::KeyEvent(event)) })
    }

    /// The custom image quality (per cent) at which the host's encoder is
    /// given about `kbps`: the inverse of video_qos.rs `user_image_quality`
    /// (ratio = per cent * 2 / 100, within BR_MIN..BR_MAX) and of the
    /// encoders' bitrate from the ratio (hwcodec.rs `calc_bitrate` for
    /// H.264 and H.265; `base_bitrate * ratio` for the others).
    fn quality_for(&self, kbps: u32) -> i32 {
        let (_, _, width, height) = self.shown();
        let (width, height) = (width.max(1) as usize, height.max(1) as usize);
        let bitrate_at = |percent: i32| -> u32 {
            let ratio = percent as f32 * 2.0 / 100.0;
            match self.codec {
                Some(Codec::H264) => qos::calc_bitrate(width, height, ratio, true),
                Some(Codec::H265) => qos::calc_bitrate(width, height, ratio, false),
                _ => (qos::base_bitrate(width as u32, height as u32) as f32 * ratio) as u32,
            }
        };
        // The slider of the client: 10 to 2000 per cent; the bitrate grows with it.
        let (mut low, mut high) = (10, 2000);
        while low < high {
            let middle = (low + high + 1) / 2;
            if bitrate_at(middle) <= kbps {
                low = middle;
            } else {
                high = middle - 1;
            }
        }
        low
    }

    /// io_loop.rs `handle_msg_from_peer` for `CursorData`, with
    /// `decode_cursor_data`: the size, the hot spot and the length are
    /// checked before the pixels are taken.
    fn cursor_shape(&mut self, data: CursorData) -> Option<Event> {
        if !(1..=MAX_CURSOR_SIZE).contains(&data.width) || !(1..=MAX_CURSOR_SIZE).contains(&data.height) {
            return None;
        }
        if !(0..data.width).contains(&data.hotx) || !(0..data.height).contains(&data.hoty) {
            return None;
        }
        let expected = data.width as usize * data.height as usize * 4;
        let rgba = cursor::decompress(&data.colors, expected)?;
        if rgba.len() != expected {
            return None;
        }
        let event = Event::CursorShape {
            width: data.width as u32,
            height: data.height as u32,
            hot_x: data.hotx as u32,
            hot_y: data.hoty as u32,
            rgba: Bytes::from(rgba),
        };
        self.cursors.retain(|(id, _)| *id != data.id);
        if self.cursors.len() >= MAX_KEPT_CURSORS {
            self.cursors.remove(0);
        }
        self.cursors.push((data.id, event.clone()));
        Some(event)
    }

    /// The messages of a command; none for what the host cannot be told.
    fn command(&mut self, command: Command) -> Vec<Message> {
        let (origin_x, origin_y, _, _) = self.shown();
        match command {
            // flutter input_model.dart: the position is on the host's
            // desktop (`x += rect.left`), the display's origin added.
            Command::PointerMove { x, y } => vec![self.mouse(MOUSE_TYPE_MOVE, x + origin_x, y + origin_y)],
            Command::PointerMoveRelative { dx, dy } => vec![self.mouse(MOUSE_TYPE_MOVE_RELATIVE, dx, dy)],
            Command::PointerButton { button, down } => {
                let button = match button {
                    Button::Left => MOUSE_BUTTON_LEFT,
                    Button::Right => MOUSE_BUTTON_RIGHT,
                    Button::Middle => MOUSE_BUTTON_WHEEL,
                    Button::Back => MOUSE_BUTTON_BACK,
                    Button::Forward => MOUSE_BUTTON_FORWARD,
                };
                vec![self.mouse((button << 3) | if down { MOUSE_TYPE_DOWN } else { MOUSE_TYPE_UP }, 0, 0)]
            }
            // server/input_service.rs handle_mouse_: y > 0 turns the wheel
            // up, x > 0 to the left.
            Command::Wheel { dx, dy } => vec![self.mouse(MOUSE_TYPE_WHEEL, -dx, dy)],
            Command::Key { position, down } => self.key(position, down).into_iter().collect(),
            // ui_session_interface.rs input_string.
            Command::Text { text } => vec![Message {
                union: Some(message::Union::KeyEvent(KeyEvent { union: Some(key_event::Union::Seq(text)), ..Default::default() })),
            }],
            // clipboard.rs: the text, as `MultiClipboards` to hosts that know it.
            Command::Clipboard { text } => vec![clipboard::text_message(&text, &self.version, &self.platform_name())],
            Command::RequestKeyFrame => vec![refresh_message(&self.version, self.display)],
            Command::Quality { fps, bitrate_kbps } => {
                let mut out = Vec::new();
                // client.rs LoginConfigHandler::set_custom_fps.
                if fps > 0 {
                    out.push(misc(misc::Union::Option(OptionMessage { custom_fps: fps as i32, ..Default::default() })));
                }
                // The protocol has no message for a bitrate. What a person
                // does on a slow connection is lower the image quality:
                // client.rs LoginConfigHandler::save_custom_image_quality.
                if bitrate_kbps > 0 {
                    let percent = self.quality_for(bitrate_kbps);
                    let now = std::time::Instant::now();
                    let due = match self.quality_sent {
                        None => true,
                        Some((last, at)) => now.duration_since(at) >= QUALITY_INTERVAL && (percent - last).abs() * 10 >= last,
                    };
                    if due {
                        self.quality_sent = Some((percent, now));
                        debug!("RustDesk connector: image quality {} % for {} kbit/s", percent, bitrate_kbps);
                        out.push(misc(misc::Union::Option(OptionMessage { custom_image_quality: percent << 8, ..Default::default() })));
                    }
                }
                out
            }
            // ui_session_interface.rs switch_display, with capture_displays
            // as a client that does not render textures sends it.
            Command::SwitchDisplay { index } => {
                if index >= self.displays.len() {
                    return Vec::new();
                }
                let display = index as i32;
                let mut out = vec![misc(misc::Union::SwitchDisplay(SwitchDisplay { display, ..Default::default() }))];
                if clipboard::get_version_number(&self.version) >= clipboard::get_version_number(MIN_VER_MULTI_UI_SESSION) {
                    out.push(misc(misc::Union::CaptureDisplays(crate::rustdesk::proto::CaptureDisplays {
                        set: vec![display],
                        ..Default::default()
                    })));
                }
                out
            }
            Command::Close => Vec::new(),
        }
    }

    /// The platform as the host named it ("Windows", "Linux", "Mac OS").
    fn platform_name(&self) -> String {
        match self.platform.as_str() {
            "windows" => "Windows".into(),
            "macos" => "Mac OS".into(),
            "android" => "Android".into(),
            _ => "Linux".into(),
        }
    }
}

/// The session after the login (io_loop.rs `io_loop` and
/// `handle_msg_from_peer`). Returns why it ended.
async fn run(
    mut conn: Conn,
    peer_info: PeerInfo,
    wanted: Vec<Codec>,
    events: &mpsc::Sender<Event>,
    mut commands: mpsc::Receiver<Command>,
) -> String {
    let mut remote = Remote::new(&peer_info);
    let mut channels: u8 = 2;
    loop {
        tokio::select! {
            msg = conn.next() => {
                let msg = match msg {
                    Ok(Some(msg)) => msg,
                    Ok(None) => return "The computer closed the connection".into(),
                    Err(e) => return format!("The connection to the computer broke: {}", e),
                };
                // What is not a picture must not wait behind pictures, and
                // must not be lost: it is sent with a wait.
                let mut other: Option<Event> = None;
                match msg.union {
                    Some(message::Union::VideoFrame(video)) => {
                        let Some((codec, frames)) = video.union.and_then(video_frames) else { continue };
                        if !wanted.contains(&codec) {
                            // scrap codec.rs Encoder::update decides by the
                            // abilities sent; a host without an encoder for
                            // them falls back to VP9.
                            warn!("RustDesk connector: the computer sends {:?}, which the viewer did not ask for", codec);
                            return format!("The computer can only send {:?} video, which this viewer cannot show", codec);
                        }
                        remote.codec = Some(codec);
                        for f in frames.frames {
                            let event = Event::Video {
                                codec,
                                key: f.key,
                                capture_time_ms: u64::try_from(f.pts).ok(),
                                data: f.data,
                            };
                            // A full queue holds the reading back: the host
                            // sees it as a slow network and lowers its rate.
                            if events.send(event).await.is_err() {
                                return "The viewer left".into();
                            }
                        }
                    }
                    // client.rs AudioHandler::handle_format / handle_frame:
                    // the format, then one Opus packet per AudioFrame.
                    Some(message::Union::Misc(Misc { union: Some(misc::Union::AudioFormat(format)) })) => {
                        if format.sample_rate != 48000 {
                            warn!("RustDesk connector: sound at {} Hz is not forwarded", format.sample_rate);
                            channels = 0;
                        } else {
                            channels = format.channels.clamp(1, 2) as u8;
                        }
                    }
                    Some(message::Union::AudioFrame(audio)) => {
                        if channels != 0 && events.try_send(Event::Audio { channels, data: audio.data }).is_err() && events.is_closed() {
                            return "The viewer left".into();
                        }
                    }
                    // client.rs handle_test_delay: the host's probe goes back as it came.
                    Some(message::Union::TestDelay(delay)) => {
                        if !delay.from_client && conn.send(&Message { union: Some(message::Union::TestDelay(delay)) }).await.is_err() {
                            return "The connection to the computer broke".into();
                        }
                    }
                    Some(message::Union::Misc(Misc { union: Some(misc::Union::CloseReason(reason)) })) => {
                        return if reason.is_empty() { "The computer closed the connection".into() } else { reason };
                    }
                    // io_loop.rs: the host shows another display, or the
                    // one shown changed its size.
                    Some(message::Union::Misc(Misc { union: Some(misc::Union::SwitchDisplay(switch)) })) => {
                        remote.display = switch.display;
                        if let Some(display) = remote.displays.get_mut(switch.display.max(0) as usize) {
                            display.x = switch.x;
                            display.y = switch.y;
                            if switch.width > 0 && switch.height > 0 {
                                display.width = switch.width;
                                display.height = switch.height;
                            }
                        }
                        other = Some(remote.displays_event());
                    }
                    // io_loop.rs handle_peer_info: the displays changed.
                    Some(message::Union::PeerInfo(info)) => {
                        if !info.displays.is_empty() {
                            remote.displays = info.displays;
                            remote.display = remote.display.clamp(0, remote.displays.len() as i32 - 1);
                            other = Some(remote.displays_event());
                        }
                    }
                    Some(message::Union::CursorData(data)) => other = remote.cursor_shape(data),
                    Some(message::Union::CursorId(id)) => other = remote.cursors.iter().find(|(known, _)| *known == id).map(|(_, shape)| shape.clone()),
                    Some(message::Union::CursorPosition(position)) => {
                        let (origin_x, origin_y, _, _) = remote.shown();
                        // The newest position is enough: not waited for.
                        let _ = events.try_send(Event::CursorPosition { x: position.x - origin_x, y: position.y - origin_y, visible: true });
                    }
                    // io_loop.rs: `Clipboard` and `MultiClipboards` both end
                    // in the client's clipboard; text is what is carried.
                    Some(message::Union::Clipboard(one)) => other = clipboard::text_from(&[one]).map(|text| Event::Clipboard { text }),
                    Some(message::Union::MultiClipboards(many)) => other = clipboard::text_from(&many.clipboards).map(|text| Event::Clipboard { text }),
                    _ => {}
                }
                if let Some(event) = other {
                    if events.send(event).await.is_err() {
                        return "The viewer left".into();
                    }
                }
            }
            command = commands.recv() => {
                let command = match command {
                    None | Some(Command::Close) => {
                        // io_loop.rs send_close_reason, then the connection ends.
                        let _ = conn.send(&misc(misc::Union::CloseReason(String::new()))).await;
                        let _ = conn.stream.shutdown().await;
                        return "Closed".into();
                    }
                    Some(command) => command,
                };
                for out in remote.command(command) {
                    if conn.send(&out).await.is_err() {
                        return "The connection to the computer broke".into();
                    }
                }
            }
        }
    }
}
