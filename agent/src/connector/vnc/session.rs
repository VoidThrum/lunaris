//! One VNC connection from the handshake to its end: the server's
//! rectangles go into the picture (raw_video.rs, which encodes it for the
//! viewer), its cursor and cut text to the viewer, the viewer's input to
//! the server.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::{debug, info, warn};

use super::keysym;
use super::rfb::{self, Rect, ServerMessage};
use crate::connector::raw_video::{EncoderChoice, RawVideo};
use crate::connector::{Button, Command, Event, Session, Target, Viewer};

pub const DEFAULT_PORT: u16 = 5900;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);
/// Longest clipboard text sent (bytes of Latin-1).
const MAX_CLIPBOARD: usize = 1 << 20;

/// §7.5.5: the button mask's bits: 1 left, 2 middle, 3 right, 4 and 5 the
/// wheel up and down; 6 and 7 the wheel left and right (the X convention
/// RFB servers follow: x11vnc, TigerVNC and libvncserver map mask bits to X
/// buttons one to one, and X buttons 6/7 are the horizontal wheel).
const MASK_LEFT: u8 = 1;
const MASK_MIDDLE: u8 = 1 << 1;
const MASK_RIGHT: u8 = 1 << 2;
const MASK_WHEEL_UP: u8 = 1 << 3;
const MASK_WHEEL_DOWN: u8 = 1 << 4;
const MASK_WHEEL_LEFT: u8 = 1 << 5;
const MASK_WHEEL_RIGHT: u8 = 1 << 6;

/// What a session is asked from a target's fields.
pub struct Options {
    pub host: String,
    pub port: u16,
    pub password: String,
    pub view_only: bool,
}

impl Options {
    pub fn of(target: &Target) -> anyhow::Result<Options> {
        let host = target.get("host").trim().to_string();
        if host.is_empty() {
            anyhow::bail!("The computer's address is missing");
        }
        let port = match target.get("port").trim() {
            "" => DEFAULT_PORT,
            p => p.parse::<u16>().ok().filter(|p| *p != 0).ok_or_else(|| anyhow::anyhow!("\"{p}\" is not a port"))?,
        };
        Ok(Options {
            host,
            port,
            password: target.get("password").to_string(),
            view_only: target.get("view_only").trim() == "on",
        })
    }
}

/// What the reading task tells the session's loop.
enum FromServer {
    UpdateDone,
    Cursor { hot_x: u16, hot_y: u16, w: u16, h: u16, rgba: Vec<u8> },
    Resized { w: u16, h: u16 },
    CutText(String),
    Failed(String),
}

pub async fn connect(target: &Target, viewer: &Viewer, choice: EncoderChoice) -> anyhow::Result<Session> {
    let options = Options::of(target)?;
    let address = format!("{}:{}", options.host, options.port);
    let mut stream = match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&address)).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => anyhow::bail!("The VNC server could not be reached ({e})"),
        Err(_) => anyhow::bail!("The VNC server did not answer"),
    };
    let _ = stream.set_nodelay(true);
    let began = std::time::Instant::now();
    let (version, init) = match tokio::time::timeout(HANDSHAKE_TIMEOUT, rfb::handshake(&mut stream, &options.password)).await {
        Ok(Ok(result)) => result,
        Ok(Err(e)) => anyhow::bail!("{e}"),
        Err(_) => anyhow::bail!("The VNC server did not finish the handshake"),
    };
    info!(
        "VNC: connected ({:?}, {}x{}, {} ms){}",
        version,
        init.width,
        init.height,
        began.elapsed().as_millis(),
        if options.view_only { ", view only" } else { "" }
    );
    // §7.5.1, §7.5.2, §7.5.3: our pixel format, our encodings, the whole picture.
    let mut first = rfb::set_pixel_format(rfb::PixelFormat::BGRX);
    first.extend(rfb::set_encodings(&rfb::ENCODINGS));
    first.extend(rfb::update_request(false, 0, 0, init.width, init.height));
    stream.write_all(&first).await?;

    #[cfg(unix)]
    let rtt_fd = {
        use std::os::fd::AsFd;
        stream.as_fd().try_clone_to_owned().ok()
    };
    #[cfg(not(unix))]
    let rtt_fd: Option<()> = None;

    let (events_tx, events_rx) = mpsc::channel(64);
    let (commands_tx, commands_rx) = mpsc::channel(256);
    let video = Arc::new(RawVideo::start(viewer, events_tx.clone(), "VNC", choice));
    video.resize(init.width as u32, init.height as u32);

    let (read_half, write_half) = stream.into_split();
    let (from_server_tx, from_server_rx) = mpsc::channel(64);
    let reader = tokio::spawn(read_loop(BufReader::with_capacity(256 * 1024, read_half), video.clone(), from_server_tx));
    let state = State {
        writer: write_half,
        video,
        events: events_tx,
        width: init.width,
        height: init.height,
        mask: 0,
        x: 0,
        y: 0,
        held: BTreeMap::new(),
        view_only: options.view_only,
        update_pending: true,
        next_request: None,
        last_request: Instant::now(),
    };
    tokio::spawn(run(state, reader, commands_rx, from_server_rx, rtt_fd));
    Ok(Session { events: events_rx, commands: commands_tx })
}

async fn read_loop(mut reader: BufReader<tokio::net::tcp::OwnedReadHalf>, video: Arc<RawVideo>, out: mpsc::Sender<FromServer>) {
    let mut zlib = flate2::Decompress::new(true);
    loop {
        let message = match rfb::read_message(&mut reader, &mut zlib).await {
            Ok(m) => m,
            Err(e) => {
                let reason = if e.kind() == std::io::ErrorKind::UnexpectedEof {
                    "The VNC server closed the connection".to_string()
                } else {
                    format!("The VNC connection failed ({e})")
                };
                let _ = out.send(FromServer::Failed(reason)).await;
                return;
            }
        };
        let to_tell: Vec<FromServer> = match message {
            ServerMessage::Update(rects) => {
                let mut tell = Vec::new();
                for rect in rects {
                    match rect {
                        Rect::Pixels { x, y, w, h, bgrx } => {
                            video.update(|fb| fb.put_bgra(x as u32, y as u32, w as u32, h as u32, &bgrx));
                        }
                        Rect::CopyRect { x, y, w, h, src_x, src_y } => {
                            video.update(|fb| fb.copy_rect(src_x as u32, src_y as u32, x as u32, y as u32, w as u32, h as u32));
                        }
                        Rect::Cursor { hot_x, hot_y, w, h, rgba } => tell.push(FromServer::Cursor { hot_x, hot_y, w, h, rgba }),
                        Rect::Resize { w, h } => {
                            video.resize(w as u32, h as u32);
                            tell.push(FromServer::Resized { w, h });
                        }
                        Rect::Ignored => {}
                    }
                }
                tell.push(FromServer::UpdateDone);
                tell
            }
            ServerMessage::CutText(text) => vec![FromServer::CutText(text)],
            ServerMessage::Bell | ServerMessage::ColourMap => Vec::new(),
        };
        for t in to_tell {
            if out.send(t).await.is_err() {
                return;
            }
        }
    }
}

struct State {
    writer: tokio::net::tcp::OwnedWriteHalf,
    video: Arc<RawVideo>,
    events: mpsc::Sender<Event>,
    width: u16,
    height: u16,
    mask: u8,
    x: u16,
    y: u16,
    /// Keys held, by position, with the keysym sent at the press (the
    /// release must name the same one).
    held: BTreeMap<u32, u32>,
    view_only: bool,
    update_pending: bool,
    next_request: Option<Instant>,
    last_request: Instant,
}

impl State {
    async fn send(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.writer.write_all(bytes).await
    }

    fn shift(&self) -> bool {
        self.held.contains_key(&keysym::KEY_LEFTSHIFT) || self.held.contains_key(&keysym::KEY_RIGHTSHIFT)
    }

    fn clamp(&self, x: i64, y: i64) -> (u16, u16) {
        (x.clamp(0, self.width.saturating_sub(1) as i64) as u16, y.clamp(0, self.height.saturating_sub(1) as i64) as u16)
    }

    /// The messages a command makes (none for one that is not input).
    fn input(&mut self, command: &Command) -> Vec<u8> {
        let mut out = Vec::new();
        match *command {
            Command::PointerMove { x, y } => {
                (self.x, self.y) = self.clamp(x as i64, y as i64);
                out.extend(rfb::pointer_event(self.mask, self.x, self.y));
            }
            Command::PointerMoveRelative { dx, dy } => {
                (self.x, self.y) = self.clamp(self.x as i64 + dx as i64, self.y as i64 + dy as i64);
                out.extend(rfb::pointer_event(self.mask, self.x, self.y));
            }
            Command::PointerButton { button, down } => {
                let bit = match button {
                    Button::Left => MASK_LEFT,
                    Button::Middle => MASK_MIDDLE,
                    Button::Right => MASK_RIGHT,
                    // §7.5.5 has 8 buttons of which 4-7 are the wheel; Back
                    // and Forward (X buttons 8 and 9) need an extension
                    // this client does not speak.
                    Button::Back | Button::Forward => return out,
                };
                if down {
                    self.mask |= bit;
                } else {
                    self.mask &= !bit;
                }
                out.extend(rfb::pointer_event(self.mask, self.x, self.y));
            }
            Command::Wheel { dx, dy } => {
                // A notch is a press and a release of the wheel's button.
                let mut notch = |bit: u8, count: i32| {
                    for _ in 0..count.unsigned_abs().min(64) {
                        out.extend(rfb::pointer_event(self.mask | bit, self.x, self.y));
                        out.extend(rfb::pointer_event(self.mask, self.x, self.y));
                    }
                };
                notch(if dy > 0 { MASK_WHEEL_UP } else { MASK_WHEEL_DOWN }, dy);
                notch(if dx > 0 { MASK_WHEEL_RIGHT } else { MASK_WHEEL_LEFT }, dx);
            }
            Command::Key { position, down } => {
                if down {
                    if let Some(sym) = keysym::keysym_of(position, self.shift()) {
                        self.held.insert(position, sym);
                        out.extend(rfb::key_event(true, sym));
                    }
                } else if let Some(sym) = self.held.remove(&position) {
                    out.extend(rfb::key_event(false, sym));
                }
            }
            Command::Text { ref text } => {
                for c in text.chars() {
                    if let Some(sym) = keysym::keysym_of_char(c) {
                        out.extend(rfb::key_event(true, sym));
                        out.extend(rfb::key_event(false, sym));
                    }
                }
            }
            _ => {}
        }
        out
    }

    /// Everything held is let go (the end of a session).
    fn release_all(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        for (_, sym) in std::mem::take(&mut self.held) {
            out.extend(rfb::key_event(false, sym));
        }
        if self.mask != 0 {
            self.mask = 0;
            out.extend(rfb::pointer_event(0, self.x, self.y));
        }
        out
    }

    /// The next incremental request, not sooner than the frame rate allows.
    fn schedule_request(&mut self) {
        let interval = Duration::from_secs_f64(1.0 / self.video.fps() as f64);
        self.next_request = Some((self.last_request + interval).max(Instant::now()));
    }
}

#[cfg(unix)]
type RttFd = Option<std::os::fd::OwnedFd>;
#[cfg(not(unix))]
type RttFd = Option<()>;

fn round_trip_ms(fd: &RttFd) -> Option<f32> {
    #[cfg(unix)]
    {
        let fd = fd.as_ref()?;
        crate::connector::rustdesk::tcp_round_trip_us(fd).map(|us| us as f32 / 1000.0)
    }
    #[cfg(not(unix))]
    {
        let _ = fd;
        None
    }
}

async fn run(
    mut state: State,
    reader: tokio::task::JoinHandle<()>,
    mut commands: mpsc::Receiver<Command>,
    mut from_server: mpsc::Receiver<FromServer>,
    rtt_fd: RttFd,
) {
    let mut latency_tick = tokio::time::interval(Duration::from_secs(1));
    let mut latency_told: Option<u32> = None;
    let (mut input_messages, mut updates) = (0u64, 0u64);
    let reason = loop {
        let request_at = state.next_request;
        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else { break "The viewer left".to_string() };
                match command {
                    Command::Close => break "Closed".to_string(),
                    Command::RequestKeyFrame => state.video.request_key_frame(),
                    Command::Quality { fps, bitrate_kbps } => state.video.set_quality(fps, bitrate_kbps),
                    Command::SwitchDisplay { .. } => {}
                    Command::Clipboard { text } => {
                        if state.view_only {
                            continue;
                        }
                        match rfb::string_to_latin1(&text) {
                            Some(latin1) if latin1.len() <= MAX_CLIPBOARD => {
                                if let Err(e) = state.send(&rfb::client_cut_text(&latin1)).await {
                                    break format!("The VNC connection failed ({e})");
                                }
                            }
                            Some(_) => {
                                let _ = state.events.send(Event::ClipboardError { text: "The text is too large for the VNC clipboard".into() }).await;
                            }
                            None => {
                                let _ = state
                                    .events
                                    .send(Event::ClipboardError {
                                        text: "VNC carries clipboard text only in Latin-1 (RFC 6143): this text has other characters and was not sent".into(),
                                    })
                                    .await;
                            }
                        }
                    }
                    other => {
                        if state.view_only {
                            continue;
                        }
                        let bytes = state.input(&other);
                        if !bytes.is_empty() {
                            input_messages += 1;
                            if let Err(e) = state.send(&bytes).await {
                                break format!("The VNC connection failed ({e})");
                            }
                        }
                    }
                }
            }
            message = from_server.recv() => {
                let Some(message) = message else { break "The VNC connection ended".to_string() };
                match message {
                    FromServer::UpdateDone => {
                        updates += 1;
                        state.update_pending = false;
                        state.schedule_request();
                    }
                    FromServer::Cursor { hot_x, hot_y, w, h, rgba } => {
                        // §7.8.1: a cursor of no size is no cursor at all.
                        let (w, h, rgba) = if w == 0 || h == 0 { (1, 1, vec![0u8; 4]) } else { (w, h, rgba) };
                        let shape = Event::CursorShape { width: w as u32, height: h as u32, hot_x: hot_x as u32, hot_y: hot_y as u32, rgba: rgba.into() };
                        if state.events.send(shape).await.is_err() {
                            break "The viewer left".to_string();
                        }
                    }
                    FromServer::Resized { w, h } => {
                        info!("VNC: the desktop is now {}x{}", w, h);
                        (state.width, state.height) = (w, h);
                        (state.x, state.y) = state.clamp(state.x as i64, state.y as i64);
                    }
                    FromServer::CutText(text) => {
                        if state.events.send(Event::Clipboard { text }).await.is_err() {
                            break "The viewer left".to_string();
                        }
                    }
                    FromServer::Failed(reason) => break reason,
                }
            }
            _ = async { tokio::time::sleep_until(request_at.unwrap()).await }, if request_at.is_some() && !state.update_pending => {
                state.next_request = None;
                state.update_pending = true;
                state.last_request = Instant::now();
                let (w, h) = (state.width, state.height);
                if let Err(e) = state.send(&rfb::update_request(true, 0, 0, w, h)).await {
                    break format!("The VNC connection failed ({e})");
                }
            }
            _ = latency_tick.tick() => {
                if let Some(ms) = round_trip_ms(&rtt_fd) {
                    let tenths = (ms * 10.0).round() as u32;
                    if latency_told != Some(tenths) {
                        latency_told = Some(tenths);
                        let _ = state.events.try_send(Event::Latency { ms });
                    }
                }
            }
        }
    };
    // Nothing stays pressed on the other computer.
    let release = state.release_all();
    if !release.is_empty() {
        let _ = state.send(&release).await;
    }
    let _ = state.writer.shutdown().await;
    reader.abort();
    let _ = reader.await;
    debug!("VNC: {} updates, {} input messages", updates, input_messages);
    let events = state.events.clone();
    let video = state.video;
    drop(state.writer);
    // The encoder is shut on a thread that may wait (raw_video.rs).
    let _ = tokio::task::spawn_blocking(move || drop(video)).await;
    if reason != "Closed" {
        warn!("VNC: session ended: {reason}");
    } else {
        info!("VNC: session closed");
    }
    let _ = events.send(Event::Closed { reason }).await;
}
