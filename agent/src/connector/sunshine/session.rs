//! One session with a Sunshine host, from the paired identity to the stream
//! of events: the stages of moonlight-common-c `Connection.c`
//! LiStartConnection in their order (serverinfo and launch come before it,
//! as in moonlight-embedded `libgamestream/client.c` gs_start_app), then one
//! task that receives video and audio, passes input on and ends the session
//! cleanly.
//!
//! Also ported here: the UDP pings of `VideoStream.c` VideoPingThreadProc and
//! `AudioStream.c` AudioPingThreadProc, the "no video" time-outs of
//! VideoReceiveThreadProc, and the dropping of the first audio of
//! AudioReceiveThreadProc.
//!
//! What GameStream fixes at the start and Lunaris may ask to change later is
//! ignored here: `Command::Quality` (bitrate and frame rate are part of the
//! launch and of the ANNOUNCE; there is no message to change them),
//! `Command::SwitchDisplay` (the host shows the display its configuration
//! names), `Command::Clipboard` (the protocol has none).
//!
//! Not in the references, and why it is here:
//! - the sessions of one host are counted (`hosts`): the application this
//!   side started is ended when the last of them ends, and starting and
//!   ending never overlap, so that a viewer who comes back at once does not
//!   join a session that is being ended under it;
//! - `Event::Closed` is said before the goodbye to the host, which can take
//!   seconds when the host is gone;
//! - the size and the bitrate are the viewer's wish within what the
//!   computer's fields allow (`stream_size`, `stream_bitrate`);
//! - the pointer can be the viewer's own (`pointer`, see `pointer_shortcut`);
//! - when the program is told to end (SIGTERM, SIGINT), the applications it
//!   started are ended first (`end_at_exit`).
//!
//! Log lines: one per stage and one at the end with the receivers' counters;
//! never a key code, text, PIN, key or certificate.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use bytes::Bytes;

use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use super::super::{Codec, Command, Event, Session, Target, Viewer};
use super::audio::{AudioEncryption, AudioOut, AudioParams, AudioReceiver, MAX_AUDIO_DATAGRAM};
use super::control::{termination_text, Control, ControlEvent, ControlParams};
use super::http::{App, Error, HostClient, Identity, LaunchParams, ServerInfo};
use super::input::{self, InputEncoder};
use super::net;
use super::rtsp::{self, RtspClient, StreamRequest};
use super::video::{AppVersion, VideoCodec, VideoEncryption, VideoOut, VideoParams, VideoReceiver};

/// VideoStream.c / AudioStream.c: "PltSleepMsInterruptible(&udpPingThread, 500)".
const UDP_PING_INTERVAL: Duration = Duration::from_millis(500);
/// VideoStream.c FIRST_FRAME_TIMEOUT_SEC.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(10);
/// VideoStream.c RTP_RECV_PACKETS_BUFFERED: how many video packets the
/// socket's buffer should hold.
const RTP_RECV_PACKETS_BUFFERED: usize = 2048;
/// PlatformSockets.h UDP_RECV_POLL_TIMEOUT_MS: an audio socket silent for
/// this long has nothing queued at the host.
const AUDIO_IDLE: Duration = Duration::from_millis(100);
/// Requests for an IDR frame that the receiver makes within this time of
/// the last one sent are sent as one, when the time is over: never dropped.
/// (The reference sends one message for every `LiRequestIdrFrame` that comes
/// after the last was sent, ControlStream.c requestIdrFrameFunc; requests
/// made while one waits to be sent are the same request there too.) A
/// request that is thrown away instead leaves the receiver waiting for a key
/// frame nobody asked for: when the frame that answered an earlier request
/// (the viewer's) is lost on the way, the receiver's own request follows
/// that one within milliseconds, and the next time it asks is 120 dropped
/// pictures later (VideoDepacketizer.c CONSECUTIVE_DROP_LIMIT), seconds of
/// a standing picture.
const IDR_REQUEST_INTERVAL: Duration = Duration::from_millis(100);
/// How long ending the application at the host may take before this side
/// stops waiting (the request itself has moonlight-qt's 30 s).
const CANCEL_PATIENCE: Duration = Duration::from_secs(10);
/// How long the same may take when the program itself is ending.
const EXIT_PATIENCE: Duration = Duration::from_secs(3);
/// How long the agent may leave a picture untaken before the session is
/// given up (while it waits, nothing is read from the host).
const EVENT_PATIENCE: Duration = Duration::from_secs(15);
/// How many video datagrams are taken in a row before the other sockets and
/// the commands are looked at again.
const VIDEO_BURST: usize = 64;

/// The field `pointer`: the pointer stays in the picture (as Sunshine sends it).
pub(super) const POINTER_PICTURE: &str = "picture";
/// The field `pointer`: the viewer's own pointer, the host's taken out of the picture.
pub(super) const POINTER_OWN: &str = "own";

pub(super) const DEFAULT_RESOLUTION: &str = "1920x1080";
pub(super) const DEFAULT_BITRATE_MBIT: &str = "20";
pub(super) const DEFAULT_APP: &str = "Desktop";

/// `1920x1080` (also with `X`, `×`, `*` or spaces): width and height. The
/// height is made even ("Height must not be odd or NVENC will fail to
/// initialize", Connection.c LiStartConnection).
pub(super) fn parse_resolution(text: &str) -> Option<(u32, u32)> {
    let text = if text.trim().is_empty() { DEFAULT_RESOLUTION } else { text.trim() };
    let (w, h) = text.split_once(|c: char| matches!(c, 'x' | 'X' | '×' | '*'))?;
    let width: u32 = w.trim().parse().ok()?;
    let height: u32 = h.trim().parse().ok()?;
    let height = height & !1;
    ((64..=8192).contains(&width) && (64..=8192).contains(&height)).then_some((width, height))
}

/// Megabits per second, as a person writes them (`20`, `7.5`, `7,5`), in
/// kbit/s, kept within what Moonlight's own setting allows (0.5 to 150).
pub(super) fn parse_bitrate_kbps(text: &str) -> Option<u32> {
    let text = if text.trim().is_empty() { DEFAULT_BITRATE_MBIT } else { text.trim() };
    let mbit: f64 = text.replace(',', ".").parse().ok()?;
    (mbit.is_finite() && mbit > 0.0).then(|| ((mbit * 1000.0) as u32).clamp(500, 150_000))
}

/// The field `pointer`: `Some(true)` for the viewer's own pointer.
pub(super) fn parse_pointer(text: &str) -> Option<bool> {
    match text.trim().to_lowercase().as_str() {
        "" | POINTER_PICTURE | "embedded" | "host" => Some(false),
        POINTER_OWN | "viewer" | "local" => Some(true),
        _ => None,
    }
}

/// The size to ask the host for: what the viewer asked for, made to fit
/// into the computer's `resolution` (never larger than it in either
/// direction, the viewer's proportions kept), or `limit` itself when the
/// viewer has no wish. Sunshine scales its picture to the size a client
/// names (`mode` of the launch, nvhttp.cpp make_launch_session;
/// `x-nv-video[0].clientViewportWd/Ht`, rtsp.cpp cmd_announce), so a viewer
/// that wants 1280x720 need not be sent 1920x1080. Even numbers ("Height
/// must not be odd", Connection.c LiStartConnection).
pub(super) fn stream_size(limit: (u32, u32), wish: (u32, u32)) -> (u32, u32) {
    if wish.0 == 0 || wish.1 == 0 {
        return limit;
    }
    let scale = (limit.0 as f64 / wish.0 as f64).min(limit.1 as f64 / wish.1 as f64).min(1.0);
    let even = |v: f64, most: u32| (((v.round() as u32) & !1).max(64)).min(most);
    (even(wish.0 as f64 * scale, limit.0), even(wish.1 as f64 * scale, limit.1 & !1))
}

/// The bitrate to ask for: the viewer's wish, at most the computer's
/// `bitrate` (and at least what Moonlight's own setting allows, 500 kbit/s).
pub(super) fn stream_bitrate(limit_kbps: u32, wish_kbps: u32) -> u32 {
    if wish_kbps == 0 {
        limit_kbps
    } else {
        wish_kbps.clamp(500, limit_kbps.max(500))
    }
}

fn video_codec(codec: Codec) -> Option<VideoCodec> {
    match codec {
        Codec::H264 => Some(VideoCodec::H264),
        Codec::H265 => Some(VideoCodec::H265),
        Codec::Av1 => Some(VideoCodec::Av1),
        Codec::Vp8 | Codec::Vp9 => None,
    }
}

/// The first of the viewer's codecs that the host says it has, in serverinfo
/// (`ServerCodecModeSupport`) and in the DESCRIBE answer both: the two places
/// RtspConnection.c performRtspHandshake reads.
pub(super) fn choose_codec(wanted: &[Codec], info: &ServerInfo, describe: &rtsp::Describe) -> Option<(Codec, VideoCodec)> {
    wanted.iter().find_map(|codec| {
        let video = video_codec(*codec)?;
        let in_info = match video {
            VideoCodec::H264 => info.supports_h264(),
            VideoCodec::H265 => info.supports_hevc(),
            VideoCodec::Av1 => info.supports_av1(),
        };
        (in_info && describe.offers(video)).then_some((*codec, video))
    })
}

/// The application of that name (letters compared without case); the error
/// names the ones there are.
pub(super) fn find_app<'a>(apps: &'a [App], name: &str) -> anyhow::Result<&'a App> {
    let name = if name.trim().is_empty() { DEFAULT_APP } else { name.trim() };
    if let Some(app) = apps.iter().find(|app| app.title.trim().eq_ignore_ascii_case(name) || app.title.trim().to_lowercase() == name.to_lowercase()) {
        return Ok(app);
    }
    if apps.is_empty() {
        anyhow::bail!("The other computer's Sunshine lists no applications");
    }
    // The titles are the host's own words: shown one line each, printable,
    // short, and no more of them than a person reads.
    let short = |text: &str| super::http::tidy_message(text).chars().take(MAX_TITLE_SHOWN).collect::<String>();
    let mut titles: Vec<String> = apps.iter().take(MAX_TITLES_SHOWN).map(|app| format!("\"{}\"", short(&app.title))).collect();
    if apps.len() > MAX_TITLES_SHOWN {
        titles.push(format!("and {} more", apps.len() - MAX_TITLES_SHOWN));
    }
    anyhow::bail!("The other computer's Sunshine has no application \"{}\". It has: {}", short(name), titles.join(", "))
}

/// How many of the host's application titles an error text names, and how
/// much of each.
const MAX_TITLES_SHOWN: usize = 20;
const MAX_TITLE_SHOWN: usize = 60;

/// VideoStream.c VideoPingThreadProc: the payload of the SETUP answer and a
/// counter from 1 (SS_PING, big-endian), or the old four letters.
pub(super) fn ping_datagram(payload: &Option<[u8; 16]>, count: &mut u32) -> Vec<u8> {
    match payload {
        Some(payload) if payload[0] != 0 => {
            *count += 1;
            let mut datagram = payload.to_vec();
            datagram.extend_from_slice(&count.to_be_bytes());
            datagram
        }
        _ => b"PING".to_vec(),
    }
}

/// A task that ends with whoever holds this.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Pings the host's port every 500 ms so that it learns where to send (and
/// any NAT on the way keeps the path open). Errors are not looked at here
/// ("Socket errors will be handled on the read-side").
///
/// `again` sends the next ping at once. Sunshine gives a ping only to a
/// session that is waiting for it (stream.cpp recv_ping registers the
/// session; the receiving side, `recv_func_init` in the same file, drops
/// what finds no entry), and the audio pings start, as in the reference,
/// right after the audio SETUP, before the host has the session at all: the
/// first one is lost and sound would wait for the second, half a second
/// later. One more ping when the handshake is done is of the same kind as
/// the others and costs nothing.
fn start_pings(socket: Arc<UdpSocket>, to: SocketAddr, payload: Option<[u8; 16]>) -> (AbortOnDrop, Arc<tokio::sync::Notify>) {
    let again = Arc::new(tokio::sync::Notify::new());
    let kick = again.clone();
    let task = AbortOnDrop(tokio::spawn(async move {
        let mut count = 0u32;
        loop {
            let _ = socket.send_to(&ping_datagram(&payload, &mut count), to).await;
            tokio::select! {
                _ = tokio::time::sleep(UDP_PING_INTERVAL) => {}
                _ = kick.notified() => {}
            }
        }
    }));
    (task, again)
}

/// PlatformSockets.c bindUdpSocket: a UDP socket of the host's address
/// family on any port, with a receive buffer when one is asked for.
fn udp_socket(remote: &IpAddr, receive_buffer: usize) -> std::io::Result<UdpSocket> {
    let (domain, bind): (_, SocketAddr) = match remote {
        IpAddr::V4(_) => (socket2::Domain::IPV4, "0.0.0.0:0".parse().unwrap()),
        IpAddr::V6(_) => (socket2::Domain::IPV6, "[::]:0".parse().unwrap()),
    };
    let socket = socket2::Socket::new(domain, socket2::Type::DGRAM, Some(socket2::Protocol::UDP))?;
    if receive_buffer > 0 {
        // The system may give less than asked; what it gives is what there is.
        let _ = socket.set_recv_buffer_size(receive_buffer);
    }
    socket.set_nonblocking(true)?;
    socket.bind(&bind.into())?;
    UdpSocket::from_std(socket.into())
}

/// What this program knows of one host (by address and port), for as long
/// as it runs.
struct HostState {
    /// Held while a session is being started and while one is being ended:
    /// the two never overlap for one host.
    gate: Arc<tokio::sync::Mutex<()>>,
    /// Sessions that are being started or run.
    sessions: usize,
    /// Of those, how many started the application (rather than joined it).
    launches: usize,
    /// For the request that ends the application.
    client: Option<Arc<HostClient>>,
    /// The HTTPS port the host named last (moonlight-qt nvcomputer.cpp keeps
    /// `activeHttpsPort` from the last serverinfo in the same way): with it
    /// the next connect needs no question over plain HTTP first.
    https_port: u16,
    /// This program turned the host's pointer off (`pointer_shortcut`) and
    /// has not turned it on again.
    pointer_hidden: bool,
}

fn hosts() -> &'static Mutex<HashMap<String, HostState>> {
    static HOSTS: OnceLock<Mutex<HashMap<String, HostState>>> = OnceLock::new();
    HOSTS.get_or_init(Default::default)
}

fn with_host<T>(key: &str, f: impl FnOnce(&mut HostState) -> T) -> T {
    let mut all = hosts().lock().unwrap_or_else(|e| e.into_inner());
    let state = all.entry(key.to_string()).or_insert_with(|| HostState {
        gate: Arc::new(tokio::sync::Mutex::new(())),
        sessions: 0,
        launches: 0,
        client: None,
        https_port: 0,
        pointer_hidden: false,
    });
    f(state)
}

/// One session leaves: the client to end the application with, when this
/// was the host's last session here and one of them had started it.
fn leave_host(key: &str) -> Option<Arc<HostClient>> {
    with_host(key, |state| {
        state.sessions = state.sessions.saturating_sub(1);
        if state.sessions > 0 {
            return None;
        }
        let owed = std::mem::take(&mut state.launches) > 0;
        let client = state.client.take();
        if owed { client } else { None }
    })
}

/// What is owed to the host at the end: when this side started the
/// application, it ends it (`/cancel`), so that the host is free again and
/// puts its display back (Sunshine nvhttp.cpp cancel: terminate_sessions,
/// proc.terminate, display_device::revert_configuration). When something was
/// already running there and the session only joined it (`/resume`), it is
/// left running: ending it would close a program somebody else started and
/// every other client's session with it; that is what "resume" means in
/// Moonlight too, which never quits an application it did not just start
/// unless the person asks.
///
/// `/cancel` ends every session of the host, so with several sessions of
/// this program at one host (a viewer that left and came back before the
/// first was over, two viewers) it is sent when the last of them ends.
///
/// Runs on every way out after a successful launch: [`HostSession::finish`]
/// at the orderly end, `Drop` (as a task of its own) when the connecting or
/// the session's task is dropped half-way.
struct HostSession {
    key: String,
    /// This session started the application (or may have).
    launched: bool,
    done: bool,
    /// The host's gate while this session is being started.
    entering: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl HostSession {
    /// Counts the session in. `entering`: the host's gate, held.
    fn begin(key: &str, client: Arc<HostClient>, entering: tokio::sync::OwnedMutexGuard<()>) -> HostSession {
        with_host(key, |state| {
            state.sessions += 1;
            state.client = Some(client);
        });
        install_exit_hook();
        HostSession { key: key.to_string(), launched: false, done: false, entering: Some(entering) }
    }

    fn set_launched(&mut self, launched: bool) {
        if launched != self.launched {
            self.launched = launched;
            with_host(&self.key, |state| {
                state.launches = if launched { state.launches + 1 } else { state.launches.saturating_sub(1) };
            });
        }
    }

    /// The session runs: others may start and end at this host again.
    fn entered(&mut self) {
        self.entering = None;
    }

    async fn cancel(client: Arc<HostClient>, patience: Duration) {
        let started = Instant::now();
        match tokio::time::timeout(patience, client.cancel()).await {
            Ok(Ok(())) => log::info!("sunshine: the application was ended at the host after {} ms; it is free again", started.elapsed().as_millis()),
            Ok(Err(e)) => log::warn!("sunshine: ending the application at the host failed: {e:?}"),
            Err(_) => log::warn!("sunshine: the host did not answer the request to end the application"),
        }
    }

    async fn leave(key: String, entering: Option<tokio::sync::OwnedMutexGuard<()>>) {
        let _gate = match entering {
            Some(gate) => gate,
            None => with_host(&key, |state| state.gate.clone()).lock_owned().await,
        };
        if let Some(client) = leave_host(&key) {
            Self::cancel(client, CANCEL_PATIENCE).await;
        }
    }

    async fn finish(&mut self) {
        if !self.done {
            self.done = true;
            Self::leave(self.key.clone(), self.entering.take()).await;
        }
    }
}

impl Drop for HostSession {
    fn drop(&mut self) {
        if !self.done {
            self.done = true;
            match tokio::runtime::Handle::try_current() {
                Ok(runtime) => {
                    runtime.spawn(Self::leave(self.key.clone(), self.entering.take()));
                }
                // Nothing can be sent any more; the count at least is kept right.
                Err(_) => {
                    let _ = leave_host(&self.key);
                }
            }
        }
    }
}

/// Ends, at once and together, every application this program started and
/// has not ended: for the moment the program itself ends. Returns how many
/// hosts were asked. (`only`: one host's key, for tests that share the
/// program with other tests' hosts.)
pub(super) async fn end_at_exit(only: Option<&str>) -> usize {
    let owed: Vec<Arc<HostClient>> = {
        let mut all = hosts().lock().unwrap_or_else(|e| e.into_inner());
        all.iter_mut()
            .filter(|(key, _)| only.is_none_or(|only| only == key.as_str()))
            .filter_map(|(_, state)| if std::mem::take(&mut state.launches) > 0 { state.client.take() } else { None })
            .collect()
    };
    let count = owed.len();
    let all: Vec<_> = owed.into_iter().map(|client| tokio::spawn(HostSession::cancel(client, EXIT_PATIENCE))).collect();
    for task in all {
        let _ = task.await;
    }
    count
}

/// When the program is told to end while an application it started still
/// runs at a host, that host would stay busy (and its display as the session
/// set it) until somebody ends the application by hand: a `Drop` cannot
/// help, the process is gone before its task runs. So, from the first
/// session on, the signals that end the program are waited for here; the
/// applications are ended (at most `EXIT_PATIENCE`), and then the signal is
/// given back to the system, which ends the program as it always did.
/// Unix only: on Windows the program is ended in other ways, which are not
/// handled here.
fn install_exit_hook() {
    #[cfg(unix)]
    {
        static INSTALLED: OnceLock<()> = OnceLock::new();
        if std::env::var_os(NO_EXIT_HOOK_VARIABLE).is_some() || INSTALLED.set(()).is_err() {
            return;
        }
        // A thread and a runtime of their own: the signal may come when the
        // runtime that started the sessions is busy ending.
        let spawned = std::thread::Builder::new().name("sunshine-exit".into()).spawn(|| {
            use tokio::signal::unix::{signal, SignalKind};
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
            runtime.block_on(async {
                let (Ok(mut terminate), Ok(mut interrupt)) = (signal(SignalKind::terminate()), signal(SignalKind::interrupt())) else {
                    return;
                };
                let number = tokio::select! {
                    _ = terminate.recv() => libc::SIGTERM,
                    _ = interrupt.recv() => libc::SIGINT,
                };
                let asked = end_at_exit(None).await;
                if asked > 0 {
                    log::info!("sunshine: the program is ending: {asked} host(s) were asked to end the application started from here");
                }
                // SAFETY: puts the signal's default action back and raises it
                // again: the program ends as if nothing had waited for it.
                unsafe {
                    libc::signal(number, libc::SIG_DFL);
                    libc::raise(number);
                }
            });
        });
        if spawned.is_err() {
            log::warn!("sunshine: no thread for the program's end: an application started at a host is not ended when this program is");
        }
    }
}

/// With this in the environment the program's end is left alone (a program
/// that embeds the agent and handles its own signals).
#[cfg(unix)]
const NO_EXIT_HOOK_VARIABLE: &str = "LUNARIS_SUNSHINE_NO_EXIT_HOOK";

fn person(error: Error) -> anyhow::Error {
    anyhow::anyhow!("{error}")
}

/// `Connector::connect` of the Sunshine connector.
pub(super) async fn connect(target: &Target, viewer: &Viewer, no_input: bool) -> anyhow::Result<Session> {
    let started = Instant::now();
    let host = target.get("host").trim().to_string();
    if host.is_empty() {
        anyhow::bail!("The computer's address is missing");
    }
    let http_port = match target.get("port").trim() {
        "" => super::http::DEFAULT_HTTP_PORT,
        port => port.parse::<u16>().ok().filter(|p| *p != 0).ok_or_else(|| anyhow::anyhow!("\"{port}\" is not a port"))?,
    };
    let limit = parse_resolution(target.get("resolution"))
        .ok_or_else(|| anyhow::anyhow!("\"{}\" is not a resolution. Write it like 1920x1080.", target.get("resolution").trim()))?;
    let bitrate_limit = parse_bitrate_kbps(target.get("bitrate"))
        .ok_or_else(|| anyhow::anyhow!("\"{}\" is not a bitrate. Write the megabits per second, like 20.", target.get("bitrate").trim()))?;
    let own_pointer = parse_pointer(target.get("pointer")).ok_or_else(|| {
        anyhow::anyhow!("\"{}\" is not a choice for the pointer. Write {POINTER_PICTURE} or {POINTER_OWN}.", target.get("pointer").trim())
    })?;
    // The viewer's wishes, within what the computer's fields allow.
    let (width, height) = stream_size(limit, (viewer.width, viewer.height));
    let bitrate_kbps = stream_bitrate(bitrate_limit, viewer.bitrate_kbps);
    let fps = if viewer.fps == 0 { 60 } else { viewer.fps.clamp(10, 240) };
    if !viewer.codecs.iter().any(|c| video_codec(*c).is_some()) {
        anyhow::bail!("Sunshine sends H.264, H.265 or AV1 video, and this viewer shows none of them");
    }

    let identity = Identity {
        client_cert_pem: target.get("client_cert").to_string(),
        client_key_pem: target.get("client_key").to_string(),
        unique_id: target.get("unique_id").to_string(),
    };
    let server_cert = target.get("server_cert");
    if identity.client_cert_pem.is_empty() || identity.client_key_pem.is_empty() || identity.unique_id.is_empty() || server_cert.is_empty() {
        anyhow::bail!("This computer is not paired yet. Remove it and add it again.");
    }
    let client = Arc::new(HostClient::new(&host, http_port, &identity, Some(server_cert)).map_err(person)?);

    // One at a time per host: a session of this host that is being ended
    // (its `/cancel` on the way) is over before this one asks what runs there.
    let key = format!("{}:{http_port}", host.to_lowercase());
    let (gate, known_https_port) = with_host(&key, |state| (state.gate.clone(), state.https_port));
    let entering = gate.lock_owned().await;
    let waited = started.elapsed();

    // libgamestream/client.c gs_init / load_server_status, and gs_applist.
    // The two answers do not depend on each other: asked together, each on
    // its own connection as always. The HTTPS port is the one the host named
    // the last time; when that does not work it is asked for again over
    // HTTP, as the first time.
    let asked = Instant::now();
    let both = |client: Arc<HostClient>| async move { tokio::join!(client.server_info(), client.app_list()) };
    let (mut info, mut apps) = if known_https_port != 0 {
        client.set_https_port(known_https_port);
        both(client.clone()).await
    } else {
        (Err(Error::Unreachable), Err(Error::Unreachable))
    };
    // (Another certificate at a remembered port may only mean the host moved
    // its ports and something else answers there now: asked once more.)
    let again = match &info {
        Err(Error::Unreachable | Error::Timeout | Error::ConnectionLost) => true,
        Err(Error::HostChanged) => known_https_port != 0,
        _ => false,
    };
    if again {
        client.set_https_port(0);
        with_host(&key, |state| state.https_port = 0);
        client.server_info_http().await.map_err(person)?;
        (info, apps) = both(client.clone()).await;
    }
    let info = info.map_err(person)?;
    if !info.over_https || !info.paired {
        return Err(person(Error::NotPaired));
    }
    with_host(&key, |state| state.https_port = client.https_port());
    let app_version = AppVersion::parse(&info.app_version).ok_or_else(|| person(Error::Protocol("appversion")))?;
    log::info!(
        "sunshine: {host}: host version {}, codec support {:#x}, {}",
        info.app_version,
        info.server_codec_mode_support,
        if info.current_game != 0 { "an application is running" } else { "free" }
    );
    // Everything from RTSP on is written for the encrypted control stream
    // that came with this version (ControlStream.c initializeControlStream
    // `encryptedControlStream`); every Sunshine reports it.
    if !app_version.at_least(7, 1, 431) {
        anyhow::bail!("The other computer's GameStream software is too old for this program (version {})", info.app_version);
    }

    let apps = apps.map_err(person)?;
    let app = find_app(&apps, target.get("app"))?;
    let asked = asked.elapsed();

    // libgamestream/client.c gs_start_app: resume what runs, launch otherwise.
    let launch = LaunchParams::new(app.id, width, height, fps).map_err(person)?;
    // From here on something may run at the host because of this call: the
    // guard is in place before the request, so that giving up while the host
    // is still starting the application (it may take its time: display mode,
    // encoder probing) ends it too.
    let launching = Instant::now();
    let mut host_session = HostSession::begin(&key, client.clone(), entering);
    host_session.set_launched(info.current_game == 0);
    let answer = match client.start_app(&info, &launch).await {
        Ok(answer) => answer,
        Err(e) => {
            // A refusal means nothing was started (Sunshine nvhttp.cpp launch
            // answers before or instead of proc.execute). Only when no answer
            // came at all may the host have gone on with it.
            if !matches!(e, Error::Timeout | Error::ConnectionLost) {
                host_session.set_launched(false);
            }
            host_session.finish().await;
            return Err(person(e));
        }
    };
    host_session.set_launched(!answer.resumed);
    let launching = launching.elapsed();
    log::info!(
        "sunshine: {host}: {} \"{}\" at {width}x{height}, {fps} fps, {bitrate_kbps} kbit/s",
        if answer.resumed { "joined the running application; asked for" } else { "started" },
        app.title.trim()
    );
    if answer.resumed && i64::from(info.current_game) != i64::from(app.id) {
        log::info!("sunshine: {host}: another application than the one asked for is running there; it is shown as it is");
    }

    let establishing = Instant::now();
    match establish(&host, &info, app_version, &launch, answer.session_url.as_deref(), viewer, bitrate_kbps).await {
        Ok(streams) => {
            // Times only: where connecting spends its time.
            log::info!(
                "sunshine: {host}: connected in {} ms (waiting for an ending session {}, serverinfo and applications {}, {} {}, RTSP and control stream {})",
                started.elapsed().as_millis(),
                waited.as_millis(),
                asked.as_millis(),
                if answer.resumed { "resume" } else { "launch" },
                launching.as_millis(),
                establishing.elapsed().as_millis(),
            );
            host_session.entered();
            let (event_tx, events) = mpsc::channel::<Event>(64);
            let (commands, command_rx) = mpsc::channel::<Command>(64);
            let run = Run {
                host,
                streams,
                host_session,
                input: InputEncoder::new(width, height),
                size: (width, height),
                no_input,
                // Without input nothing can be said to the host's pointer.
                own_pointer: own_pointer && !no_input,
                control_alive: true,
                foreign_datagrams: 0,
                audio_dropped_unsent: 0,
            };
            tokio::spawn(run.run(event_tx, command_rx));
            Ok(Session { events, commands })
        }
        Err(e) => {
            // The launch stood: the host must not stay busy for nothing.
            host_session.finish().await;
            Err(e)
        }
    }
}

/// Everything a running session holds.
struct Streams {
    codec: Codec,
    control: Control,
    video_socket: Arc<UdpSocket>,
    audio_socket: Arc<UdpSocket>,
    video: VideoReceiver,
    audio: AudioReceiver,
    audio_packet_duration_ms: u32,
    video_port: u16,
    /// Where the host's video and audio come from.
    video_from: Origin,
    audio_from: Origin,
    _video_pings: AbortOnDrop,
    _audio_pings: AbortOnDrop,
}

/// Where a stream of the host comes from, to tell its datagrams from
/// anybody else's (the reference takes whatever arrives at the port,
/// VideoStream.c / AudioStream.c `recvUdpSocket(..., NULL, NULL)`).
///
/// The port is known: Sunshine sends from the sockets it bound to the ports
/// it named in the SETUP answers (stream.cpp start_broadcast binds
/// `video_sock` / `audio_sock`, which videoBroadcastThread and
/// audioBroadcastThread send with). The address is not: those sockets are
/// bound to every address of the host, so a host with several (a LAN and a
/// VPN one) answers from the one its own routes choose, which need not be
/// the one it was asked at. So: the address it was asked at always counts;
/// until something came from there, so does the first other address that
/// sends from the right port (and only that one).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Origin {
    port: u16,
    /// The address the host was asked at.
    host: IpAddr,
    /// Something came from `host`.
    host_seen: bool,
    /// Another address that sent from the right port before `host` did.
    other: Option<IpAddr>,
}

impl Origin {
    pub(super) fn new(host: IpAddr, port: u16) -> Origin {
        Origin { port, host: host.to_canonical(), host_seen: false, other: None }
    }

    /// Whether a datagram from `from` is taken. (An IPv4 address may show
    /// as an IPv4-mapped IPv6 one.)
    pub(super) fn takes(&mut self, from: &SocketAddr) -> bool {
        if from.port() != self.port {
            return false;
        }
        let address = from.ip().to_canonical();
        if address == self.host {
            self.host_seen = true;
            self.other = None;
            return true;
        }
        !self.host_seen && *self.other.get_or_insert(address) == address
    }
}

/// Connection.c LiStartConnection from "Resolving host name" to "Starting
/// audio stream".
async fn establish(
    host: &str,
    info: &ServerInfo,
    app_version: AppVersion,
    launch: &LaunchParams,
    session_url: Option<&str>,
    viewer: &Viewer,
    bitrate_kbps: u32,
) -> anyhow::Result<Streams> {
    // The reference talks to the address the person gave, on the port the
    // session URL names (the URL's own address is the host's view of itself).
    let rtsp_port = rtsp::rtsp_port_from_url(session_url);
    let remote = tokio::net::lookup_host((host, rtsp_port))
        .await
        .ok()
        .and_then(|mut addresses| addresses.next())
        .ok_or_else(|| person(Error::Unreachable))?
        .ip();

    // RtspConnection.c performRtspHandshake: high-quality audio from 15 Mbit/s on.
    let high_quality_audio = bitrate_kbps >= rtsp::HIGH_AUDIO_BITRATE_THRESHOLD;
    let mut rtsp = RtspClient::new(session_url, remote, &launch.rikey, app_version, high_quality_audio);
    rtsp.options().await.map_err(person)?;
    let describe = rtsp.describe().await.map_err(person)?;
    // A host that encrypts RTSP is a Sunshine with its encryption extension,
    // and that one always has the control stream's new encryption (rtsp.cpp
    // cmd_describe: `encryption_flags_supported = SS_ENC_CONTROL_V2 |
    // SS_ENC_AUDIO` before anything else). An answer without it is not
    // followed down to the old scheme, whose IV repeats after 256 messages.
    if rtsp.is_encrypted() && describe.encryption_supported & rtsp::SS_ENC_CONTROL_V2 == 0 {
        log::warn!("sunshine: {host}: the host encrypts RTSP but does not offer its control stream encryption; not connecting");
        return Err(person(Error::Protocol("control stream encryption not offered")));
    }
    let (codec, video_codec) = choose_codec(&viewer.codecs, info, &describe)
        .ok_or_else(|| anyhow::anyhow!("The other computer offers no video format that this viewer can show"))?;

    let (audio_port, audio_ping) = rtsp.setup_audio().await.map_err(person)?;
    // AudioStream.c notifyAudioPortNegotiationComplete: the audio socket and
    // its pings start here, before the rest of the handshake.
    let audio_socket = Arc::new(udp_socket(&remote, 0).map_err(|_| person(Error::Internal("no UDP socket for audio")))?);
    let (audio_pings, audio_ping_again) = start_pings(audio_socket.clone(), SocketAddr::new(remote, audio_port), audio_ping);
    let (video_port, video_ping) = rtsp.setup_video().await.map_err(person)?;
    let (control_port, connect_data) = rtsp.setup_control().await.map_err(person)?;

    // Encryption: everything the host can do is asked for (ENCFLG_ALL, what
    // moonlight-qt chooses on a machine with AES instructions and more than
    // two cores, app/streaming/session.cpp). The control stream and with it
    // all input are encrypted in any case, audio whenever this is set, video
    // when the host offers it: a Sunshine with default settings does not on
    // a private network (`lan_encryption_mode` 0) and does elsewhere. The
    // agent's machine decrypts a 150 Mbit/s stream without effort, so there
    // is no reason to hold back.
    let request = StreamRequest {
        codec: video_codec,
        width: launch.width,
        height: launch.height,
        fps: launch.fps,
        bitrate_kbps,
        encryption_flags: rtsp::ENCFLG_ALL,
    };
    // moonlight-qt session.cpp: the size of the video packets follows how
    // the host is reached (net.rs).
    let reach = net::reachability(&remote);
    let (sdp, negotiated) = rtsp::build_sdp(&request, &describe, app_version, &remote, reach, video_port);
    rtsp.announce(&sdp).await.map_err(person)?;
    rtsp.play().await.map_err(person)?;
    log::info!(
        "sunshine: {host}: RTSP done ({}): {codec:?}, video packets of {} bytes (the host is reached {}), encryption {:#x}{}{}, ports {video_port}/{audio_port}/{control_port}",
        if rtsp.is_encrypted() { "encrypted" } else { "plain" },
        negotiated.packet_size,
        match reach {
            net::Reachability::Lan => "on the local network",
            net::Reachability::Vpn => "through a VPN or a narrow link",
            net::Reachability::Unknown => "by a way that is not known",
        },
        negotiated.encryption_enabled,
        if negotiated.video_encrypted { ", video encrypted" } else { "" },
        if negotiated.audio_encrypted { ", audio encrypted" } else { "" },
    );

    // startControlStream
    let control = Control::connect(ControlParams {
        remote: SocketAddr::new(remote, control_port),
        connect_data,
        key: launch.rikey,
        encryption_v2: negotiated.encryption_enabled & rtsp::SS_ENC_CONTROL_V2 != 0,
    })
    .await
    .map_err(|e| match e {
        Error::Timeout | Error::Unreachable => {
            anyhow::anyhow!("The other computer does not answer on UDP port {control_port}. A firewall may be in the way.")
        }
        other => person(other),
    })?;
    log::info!("sunshine: {host}: control stream connected");
    // The host has the session now: the ping it could not take before.
    audio_ping_again.notify_one();

    // startVideoStream: the socket (with room for RTP_RECV_PACKETS_BUFFERED
    // packets) and its pings.
    let video = VideoReceiver::new(VideoParams {
        codec: video_codec,
        packet_size: negotiated.packet_size,
        app_version,
        encryption: negotiated.video_encrypted.then_some(VideoEncryption { key: launch.rikey }),
    })
    .map_err(|_| person(Error::Internal("video receiver")))?;
    let video_socket = Arc::new(
        udp_socket(&remote, RTP_RECV_PACKETS_BUFFERED * (negotiated.packet_size + 16))
            .map_err(|_| person(Error::Internal("no UDP socket for video")))?,
    );
    if let Ok(size) = socket2::SockRef::from(&*video_socket).recv_buffer_size() {
        log::debug!("sunshine: {host}: the video socket's buffer holds {size} bytes");
    }
    let (video_pings, _) = start_pings(video_socket.clone(), SocketAddr::new(remote, video_port), video_ping);

    let audio = AudioReceiver::new(AudioParams {
        packet_duration_ms: negotiated.audio_packet_duration_ms,
        app_version,
        // AudioStream.c initializeAudioStream avRiKeyId: the first four bytes of the IV, big-endian.
        encryption: negotiated.audio_encrypted.then_some(AudioEncryption { key: launch.rikey, rikeyid: launch.rikey_id as u32 }),
    })
    .map_err(|_| person(Error::Internal("audio receiver")))?;

    Ok(Streams {
        codec,
        control,
        video_socket,
        audio_socket,
        video,
        audio,
        audio_packet_duration_ms: negotiated.audio_packet_duration_ms,
        video_port,
        video_from: Origin::new(remote, video_port),
        audio_from: Origin::new(remote, audio_port),
        _video_pings: video_pings,
        _audio_pings: audio_pings,
    })
}

struct Run {
    host: String,
    streams: Streams,
    host_session: HostSession,
    input: InputEncoder,
    /// The picture's size as launched: the space of pointer positions.
    size: (u32, u32),
    no_input: bool,
    /// The viewer shows its own pointer and the host's is taken out of the
    /// picture (the field `pointer`).
    own_pointer: bool,
    /// The control stream has not ended (what is sent may still arrive).
    control_alive: bool,
    /// Datagrams on the video and audio ports that did not come from the host.
    foreign_datagrams: u64,
    /// Sound the agent had no room for.
    audio_dropped_unsent: u64,
}

/// A socket error that only says an earlier datagram found no listener yet.
fn harmless(e: &std::io::Error) -> bool {
    matches!(e.kind(), std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::ConnectionReset)
}

/// Sunshine's own shortcut for "Hide/Unhide the cursor" (docs/getting_started.md,
/// Shortcuts: "Ctrl+Alt+Shift+N ... This may be useful for Remote Desktop Mode
/// for Moonlight"), as key presses of this client: input.cpp
/// passthrough(PNV_KEYBOARD_PACKET) follows the three modifiers
/// (update_shortcutFlags) and, with all of them held, gives a new press of N
/// to apply_shortcut, which turns `display_cursor` over and does not pass the
/// N on. That flag (globals.cpp) is what every capture is told
/// (`disp->capture(..., &display_cursor)`, video.cpp).
///
/// This is all the protocol has for the pointer: no message carries its
/// shape, its place or whether it shows (the host's control messages are
/// rumble, HDR mode, the controller extensions and the termination,
/// stream.cpp `packetTypes`), and neither the launch nor the ANNOUNCE has a
/// word for it. So "the viewer's own pointer" means: the host's is taken
/// out of the picture and the viewer shows the one of its own system, where
/// its mouse is, in its own shape (never the host's: an I-beam over text
/// there stays an arrow here).
///
/// What cannot be known from here: whether the flag was on before (it
/// belongs to the whole Sunshine program, starts as on, and stays as it was
/// left when a session ends), whether the host takes keys at all (its
/// `keyboard` setting), and whether its way of capturing follows the flag
/// (X11, KMS, wlroots, NvFBC and Windows do; the PipeWire/portal and KWin
/// captures of 2026.914 always have the pointer in the picture). The three
/// modifier keys themselves are passed on to the host's system like any
/// keys.
fn pointer_shortcut(input: &mut InputEncoder, control: &Control) {
    // KEY_LEFTCTRL, KEY_LEFTALT, KEY_LEFTSHIFT, KEY_N (linux/input-event-codes.h)
    const KEYS: [u32; 4] = [29, 56, 42, 49];
    let presses = KEYS.iter().map(|key| (*key, true)).chain(KEYS.iter().rev().map(|key| (*key, false)));
    for (position, down) in presses {
        for packet in input.encode(&Command::Key { position, down }) {
            control.send_input(packet.channel, packet.data);
        }
    }
}

impl Run {
    async fn run(mut self, events: mpsc::Sender<Event>, mut commands: mpsc::Receiver<Command>) {
        let reason = self.stream(&events, &mut commands).await;
        // Ending begins: nobody starts a session at this host until it is
        // over (a viewer that comes back the moment it is told then finds
        // the host free and starts afresh, instead of joining what is being
        // ended). Only another session being started right now is waited
        // for, and not for long.
        let gate = with_host(&self.host_session.key, |state| state.gate.clone());
        if let Ok(held) = tokio::time::timeout(Duration::from_secs(2), gate.lock_owned()).await {
            self.host_session.entering = Some(held);
        }
        // The viewer is told now: what follows (the goodbye, ending the
        // application) can take seconds when the host is gone, and is not
        // the viewer's to wait for. The channel itself closes when all of
        // it is done.
        let _ = events.send_timeout(Event::Closed { reason: reason.clone() }, EVENT_PATIENCE).await;
        self.restore_pointer();
        let Run { host, streams, mut host_session, foreign_datagrams, audio_dropped_unsent, .. } = self;
        let Streams { control, video, audio, _video_pings, _audio_pings, .. } = streams;
        // Connection.c LiStopConnection: the control stream says goodbye
        // (stopControlStream), then the application is ended if it is ours.
        let control_stats = control.close().await;
        drop((_video_pings, _audio_pings));
        host_session.finish().await;
        log::info!(
            "sunshine: {host}: session ended ({reason}); video {:?}; audio {:?}; control {:?}; {foreign_datagrams} datagrams from elsewhere dropped; {audio_dropped_unsent} sound packets not passed on",
            video.stats(),
            audio.stats(),
            control_stats
        );
    }

    /// The pointer as this session wants it, once the host is seen to send:
    /// taken out of the picture for a viewer with its own, or put back when
    /// an earlier session of this program left it out and could not say so
    /// any more (its connection was lost).
    fn set_pointer(&mut self) {
        if self.no_input {
            return;
        }
        let key = self.host_session.key.clone();
        let hidden = with_host(&key, |state| state.pointer_hidden);
        if hidden != self.own_pointer {
            pointer_shortcut(&mut self.input, &self.streams.control);
            with_host(&key, |state| state.pointer_hidden = self.own_pointer);
            log::info!(
                "sunshine: {}: asked the host to {} its pointer (its own shortcut)",
                self.host,
                if self.own_pointer { "leave out" } else { "show" }
            );
        }
    }

    /// At the end of a session that had the pointer taken out: back into the
    /// picture, for whoever looks at this host next (Moonlight too). Only
    /// the last session here does it, and only while the host still listens;
    /// otherwise it is remembered for the next session (`set_pointer`).
    fn restore_pointer(&mut self) {
        if !self.own_pointer {
            return;
        }
        let key = self.host_session.key.clone();
        let (hidden, last) = with_host(&key, |state| (state.pointer_hidden, state.sessions <= 1));
        if !hidden || !last {
            return;
        }
        if self.control_alive {
            pointer_shortcut(&mut self.input, &self.streams.control);
            with_host(&key, |state| state.pointer_hidden = false);
        } else {
            log::warn!(
                "sunshine: {}: the host's pointer stays out of its picture (the connection was lost before it could be put back); the next session puts it back, or Ctrl+Alt+Shift+N at that computer",
                self.host
            );
        }
    }

    /// The session until something ends it; the reason for the person.
    async fn stream(&mut self, events: &mpsc::Sender<Event>, commands: &mut mpsc::Receiver<Command>) -> String {
        const VIEWER_GONE: &str = "The viewer left";
        const VIEWER_STUCK: &str = "The viewer stopped taking the picture";
        const LOST: &str = "The connection to the other computer was lost";
        let codec = self.streams.codec;

        // Sunshine draws the pointer into the picture it captures
        // (globals.cpp `bool display_cursor = true`, handed to every
        // `disp->capture(..., &display_cursor)` in video.cpp), and the
        // protocol has no pointer shape or place of its own: for the whole
        // session, whatever the host's system is (it is not known, see
        // below), so not `CursorEmbedded`, which viewers honour for a
        // Windows computer while a button is held. Not said when the
        // computer is set to the viewer's own pointer: the host's is then
        // taken out of the picture (`pointer_shortcut`) and the viewer shows
        // its own, as it does when it is told nothing.
        if !self.own_pointer && events.send(Event::CursorAlwaysEmbedded).await.is_err() {
            return VIEWER_GONE.to_string();
        }
        // The one display there is, in the size of the stream: the agent
        // scales the viewer's pointer positions to it (without a size it
        // passes no position on), and `InputEncoder` sends them with the same
        // size as their reference (the host maps them to its screen).
        let (width, height) = self.size;
        let display = super::super::DisplayInfo { name: String::new(), width, height };
        if events.send(Event::Displays { displays: vec![display], current: 0 }).await.is_err() {
            return VIEWER_GONE.to_string();
        }
        // No `Event::Platform`: nothing the host tells names its system.
        // serverinfo has no such field, and `x-ss-general.featureFlags` of
        // DESCRIBE is platf::get_capabilities(), the same pen/touch bits on
        // Linux, Windows and macOS.

        let mut video_buf = vec![0u8; self.streams.video.max_datagram_size().max(2048)];
        let mut audio_buf = vec![0u8; MAX_AUDIO_DATAGRAM.max(2048)];
        let started = Instant::now();
        let mut first_video_data: Option<Instant> = None;
        let mut saw_frame = false;
        let mut pointer_set = false;
        let mut last_idr_request: Option<Instant> = None;
        // The receiver's request that waits for `IDR_REQUEST_INTERVAL` to pass.
        let mut idr_due: Option<tokio::time::Instant> = None;
        // AudioStream.c AudioReceiveThreadProc: "we will drop the ones that
        // arrived before the receive thread was ready" (500 ms of them).
        let mut audio_to_drop = 500 / self.streams.audio_packet_duration_ms.max(1);
        let mut last_audio: Option<Instant> = None;
        // The first byte of the stream's Opus packets (they are all of one
        // kind): what a lost one is stood in for with.
        let mut opus_toc: Option<u8> = None;
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        // The round trip to the host last told to the viewer (tenths of a
        // millisecond), and when it was looked at last (once a second).
        let mut latency_told: Option<u32> = None;
        let mut latency_checked = Instant::now();

        loop {
            // The pointer is seen to once the host is seen to send.
            if saw_frame && !pointer_set {
                pointer_set = true;
                self.set_pointer();
            }
            let streams = &mut self.streams;
            tokio::select! {
                received = streams.video_socket.recv_from(&mut video_buf) => {
                    let mut received = received;
                    // What has queued up is taken in one go (the reference
                    // reads in a loop of its own, VideoReceiveThreadProc).
                    for _ in 0..VIDEO_BURST {
                        let n = match received {
                            Ok((n, from)) if streams.video_from.takes(&from) => n,
                            // Not the host's: anybody can send to this port.
                            Ok(_) => {
                                self.foreign_datagrams += 1;
                                0
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                            Err(e) if harmless(&e) => 0,
                            Err(e) => {
                                log::info!("sunshine: video socket failed: {}", e.kind());
                                return LOST.to_string();
                            }
                        };
                        if n > 0 {
                            let now = Instant::now();
                            first_video_data.get_or_insert(now);
                            for out in streams.video.push(&video_buf[..n], now) {
                                match out {
                                    VideoOut::Frame { key, presentation_time_us, data, .. } => {
                                        if !saw_frame {
                                            saw_frame = true;
                                            log::info!("sunshine: {}: first picture after {} ms", self.host, started.elapsed().as_millis());
                                        }
                                        let event = Event::Video { codec, key, capture_time_ms: Some(presentation_time_us / 1000), data };
                                        // The agent takes pictures as fast as they come;
                                        // one that is not taken for this long never will be.
                                        match events.send_timeout(event, EVENT_PATIENCE).await {
                                            Ok(()) => {}
                                            Err(mpsc::error::SendTimeoutError::Closed(_)) => return VIEWER_GONE.to_string(),
                                            Err(mpsc::error::SendTimeoutError::Timeout(_)) => return VIEWER_STUCK.to_string(),
                                        }
                                    }
                                    VideoOut::NeedIdr { reason } => {
                                        log::debug!("sunshine: the receiver needs a key frame ({reason:?})");
                                        match last_idr_request {
                                            Some(at) if at.elapsed() < IDR_REQUEST_INTERVAL => {
                                                // Held back, not forgotten.
                                                idr_due.get_or_insert(tokio::time::Instant::from_std(at + IDR_REQUEST_INTERVAL));
                                            }
                                            _ => {
                                                last_idr_request = Some(Instant::now());
                                                idr_due = None;
                                                streams.control.request_idr();
                                            }
                                        }
                                    }
                                    // Declared in the ANNOUNCE (ML_FF_FEC_STATUS).
                                    VideoOut::FecStatus(status) => streams.control.send_fec_status(status.to_bytes()),
                                }
                            }
                        }
                        received = streams.video_socket.try_recv_from(&mut video_buf);
                    }
                }
                received = streams.audio_socket.recv_from(&mut audio_buf) => {
                    let n = match received {
                        Ok((n, from)) if streams.audio_from.takes(&from) => n,
                        Ok(_) => {
                            self.foreign_datagrams += 1;
                            continue;
                        }
                        Err(e) if harmless(&e) => continue,
                        Err(e) => {
                            log::info!("sunshine: audio socket failed: {}", e.kind());
                            return LOST.to_string();
                        }
                    };
                    let now = Instant::now();
                    // "If we hit this path, there are no queued audio packets on the host PC"
                    if last_audio.is_some_and(|at| now.duration_since(at) >= AUDIO_IDLE) {
                        audio_to_drop = 0;
                    }
                    last_audio = Some(now);
                    if n < 12 {
                        continue;
                    }
                    if audio_to_drop > 0 {
                        // "Only count actual audio data (not FEC) in the packets to drop calculation"
                        if audio_buf[1] & 0x7F == 97 {
                            audio_to_drop -= 1;
                        }
                        continue;
                    }
                    for out in streams.audio.push(&audio_buf[..n], now) {
                        let data = match out {
                            AudioOut::Packet { data, .. } => {
                                opus_toc = data.first().copied();
                                data
                            }
                            // A packet lost for good: the reference has its
                            // decoder conceal it (AudioStream.c decodeInputData
                            // with no data). The decoder here is the viewer's:
                            // it is given an Opus packet of the same kind
                            // with an empty frame, which RFC 6716 (3.2.1: a
                            // frame of length zero) defines as "lost, conceal"
                            // and which keeps the sound's clock in step.
                            AudioOut::Lost { .. } => match opus_toc {
                                Some(toc) => Bytes::from(vec![toc & 0xFC]),
                                None => continue,
                            },
                        };
                        // Sound that the agent has no room for is of no use later.
                        match events.try_send(Event::Audio { channels: 2, data }) {
                            Ok(()) => {}
                            Err(mpsc::error::TrySendError::Full(_)) => self.audio_dropped_unsent += 1,
                            Err(mpsc::error::TrySendError::Closed(_)) => return VIEWER_GONE.to_string(),
                        }
                    }
                }
                command = commands.recv() => {
                    let Some(first) = command else {
                        // The session was dropped.
                        return "The session was closed".to_string();
                    };
                    // What came at once is looked at together (input.rs coalesce).
                    let mut batch = vec![first];
                    while batch.len() < 256 {
                        match commands.try_recv() {
                            Ok(command) => batch.push(command),
                            Err(_) => break,
                        }
                    }
                    for command in input::coalesce(batch) {
                        match command {
                            Command::Close => return "The session was closed".to_string(),
                            Command::RequestKeyFrame => {
                                last_idr_request = Some(Instant::now());
                                idr_due = None;
                                streams.control.request_idr();
                            }
                            // See the top of this file.
                            Command::Quality { .. } | Command::SwitchDisplay { .. } | Command::Clipboard { .. } => {}
                            command => {
                                // The test switch: nothing of an input command
                                // is turned into a packet.
                                if self.no_input {
                                    continue;
                                }
                                for packet in self.input.encode(&command) {
                                    streams.control.send_input(packet.channel, packet.data);
                                }
                            }
                        }
                    }
                }
                _ = async { tokio::time::sleep_until(idr_due.unwrap_or_else(tokio::time::Instant::now)).await }, if idr_due.is_some() => {
                    idr_due = None;
                    last_idr_request = Some(Instant::now());
                    streams.control.request_idr();
                }
                event = streams.control.event() => {
                    self.control_alive = false;
                    return match event {
                        Some(ControlEvent::Terminated { code }) => termination_text(code, saw_frame),
                        Some(ControlEvent::Lost) | Some(ControlEvent::Failed) | None => LOST.to_string(),
                    };
                }
                _ = tick.tick() => {
                    // The delay to the host, for the viewer's ping
                    // (control.rs `round_trip_to_tell`). Not worth a wait: a
                    // full queue gets it a second later.
                    if latency_checked.elapsed() >= Duration::from_secs(1) {
                        latency_checked = Instant::now();
                        if let Some(tenths) = super::control::round_trip_to_tell(&streams.control.stats(), latency_told) {
                            if events.try_send(Event::Latency { ms: tenths as f32 / 10.0 }).is_ok() {
                                latency_told = Some(tenths);
                            }
                        }
                    }
                    // VideoStream.c VideoReceiveThreadProc: "Terminating
                    // connection due to lack of video traffic" /
                    // "... lack of a successful video frame". After the first
                    // picture the reference has no such limit (a host whose
                    // screen stands still may send little; when it is gone
                    // the control stream says so), and neither has this.
                    if !saw_frame {
                        match first_video_data {
                            None if started.elapsed() >= FIRST_FRAME_TIMEOUT => {
                                return format!(
                                    "No picture arrived from the other computer. A firewall may be blocking UDP port {}.",
                                    streams.video_port
                                );
                            }
                            Some(at) if at.elapsed() >= FIRST_FRAME_TIMEOUT => {
                                return "The other computer's picture arrives incomplete. The network is losing too much of it.".to_string();
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
