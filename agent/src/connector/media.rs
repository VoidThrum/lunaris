//! A connector's session as the source of an agent session: its picture
//! and sound go to the RTP packager as they came (no decoding, no
//! encoding), its cursor and clipboard to the viewer, and the viewer's
//! input and the session's requests (key frame, frame rate, bitrate, stop)
//! to the connector. Nothing of the viewer's reaches this computer's own
//! screen, input or clipboard.
//!
//! The environment variables below are for tests only; this is the one
//! place they are described.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tracing::{debug, info};

use super::{Button, Codec, Command, DisplayInfo, Event, Session, SourceError, Target, Viewer};
use crate::bridge::VideoFramePayload;
use crate::input::{InboundPacket, KeyAction, MouseButton, MouseButtonAction};
use lunaris_media::pipeline::{MediaEvent, PipelineCommand};

/// Test only: the path of a JSON file with one target (`{"connector":
/// "rustdesk", "host": ..., "password": ...}`). An agent started with it
/// shows that target instead of its own screen, and injects no input.
pub const TEST_TARGET_ENV: &str = "LUNARIS_CONNECTOR_TEST_TARGET";
/// Test only: a folder to look for external connectors in, instead of
/// `connectors/` beside the agent's config.
pub const TEST_DIR_ENV: &str = "LUNARIS_CONNECTOR_TEST_DIR";

/// Test only: when set, an agent session never opens this computer's own
/// input injector (a test that must not move the user's pointer whatever
/// goes wrong sets it).
pub const TEST_NO_LOCAL_INPUT_ENV: &str = "LUNARIS_TEST_NO_LOCAL_INPUT";
/// Test only: ask the connector for a key frame every so many
/// milliseconds, to measure how long one takes.
pub const TEST_KEY_FRAME_EVERY_ENV: &str = "LUNARIS_CONNECTOR_TEST_KEY_FRAME_EVERY_MS";

/// How long the first picture may take after the login.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(15);

/// The test target, when the variable names one. A file that cannot be
/// read is an error, never "no target": the session must not fall back to
/// this computer's own screen and input.
pub fn test_target() -> anyhow::Result<Option<Target>> {
    let Some(path) = std::env::var(TEST_TARGET_ENV).ok().filter(|p| !p.is_empty()) else { return Ok(None) };
    let text = std::fs::read_to_string(&path).map_err(|e| anyhow::anyhow!("{}={}: {}", TEST_TARGET_ENV, path, e))?;
    let target = serde_json::from_str::<Target>(&text).map_err(|e| anyhow::anyhow!("{}={}: {}", TEST_TARGET_ENV, path, e))?;
    Ok(Some(target))
}

fn connectors_dir() -> std::path::PathBuf {
    std::env::var(TEST_DIR_ENV).ok().filter(|p| !p.is_empty()).map(Into::into).unwrap_or_else(super::connectors_dir)
}

pub fn codec_from_name(name: &str) -> Option<Codec> {
    Some(match name {
        "h264" => Codec::H264,
        "h265" => Codec::H265,
        "vp8" => Codec::Vp8,
        "vp9" => Codec::Vp9,
        "av1" => Codec::Av1,
        _ => return None,
    })
}

pub fn codec_name(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 => "h264",
        Codec::H265 => "h265",
        Codec::Vp8 => "vp8",
        Codec::Vp9 => "vp9",
        Codec::Av1 => "av1",
    }
}

/// What the connector is told the viewer takes: the codec asked for
/// first, then the others it decodes. A client from before it said what it
/// decodes (`decodes` None) is taken to decode what every WebRTC browser
/// does, and H.265 only when it asked for it.
pub fn viewer_codecs(requested: &str, decodes: Option<&[String]>) -> Vec<Codec> {
    let mut codecs = Vec::new();
    let known: Vec<Codec> = match decodes {
        Some(names) => names.iter().filter_map(|name| codec_from_name(&name.to_lowercase())).collect(),
        None => vec![Codec::H264, Codec::Vp9, Codec::Vp8, Codec::Av1],
    };
    if let Some(first) = codec_from_name(requested) {
        if decodes.is_none() || known.contains(&first) {
            codecs.push(first);
        }
    }
    for codec in known {
        if !codecs.contains(&codec) {
            codecs.push(codec);
        }
    }
    codecs
}

/// A target logged in to, with its first picture: the codec is known.
pub struct Source {
    pub codec: Codec,
    session: Session,
    first: Option<Event>,
    pending: Vec<Event>,
}

/// Logs in to `target` and waits for the first picture. The codec of an
/// agent session is fixed when it is offered to the viewer, and which codec
/// a computer sends is only known when it sends: so this comes first.
/// Every failure is a [`SourceError`] with words for the viewer.
pub async fn open(target: &Target, requested_codec: &str, decodes: Option<&[String]>, fps: u32) -> Result<Source, SourceError> {
    let fail = |e: anyhow::Error| SourceError(e.to_string());
    let connector = super::find(&target.connector, &connectors_dir()).map_err(fail)?;
    let viewer = Viewer { codecs: viewer_codecs(requested_codec, decodes), fps };
    if viewer.codecs.is_empty() {
        return Err(SourceError("This viewer decodes no video the computer could send".into()));
    }
    info!("Connector {}: connecting ({:?}) for a viewer that takes {:?}", target.connector, target, viewer.codecs);
    let mut session = connector.connect(target, &viewer).await.map_err(fail)?;
    let mut pending = Vec::new();
    let deadline = tokio::time::Instant::now() + FIRST_FRAME_TIMEOUT;
    loop {
        let event = tokio::time::timeout_at(deadline, session.events.recv())
            .await
            .map_err(|_| SourceError("The computer sent no picture".into()))?
            .ok_or_else(|| SourceError("The connector ended".into()))?;
        match event {
            Event::Video { codec, .. } => {
                if !viewer.codecs.contains(&codec) {
                    return Err(SourceError(format!("The computer sends {:?} video, which this viewer cannot show", codec)));
                }
                info!("Connector {}: the computer sends {:?}", target.connector, codec);
                return Ok(Source { codec, session, first: Some(event), pending });
            }
            Event::Closed { reason } => return Err(SourceError(reason)),
            // Sound before the first picture is of no use.
            Event::Audio { .. } => {}
            other => pending.push(other),
        }
    }
}

/// RTP time (90 kHz) of the frames of one session. A connector's capture
/// time is used while it is sane: moving forward, and in step with the
/// clock here (a computer whose times jump, stand still or run at another
/// speed gets the times of arrival instead, for the rest of the session).
pub struct Timeline {
    start: Instant,
    /// (capture time, arrival) of the first frame.
    origin: Option<(u64, Duration)>,
    last_capture_ms: u64,
    use_capture: bool,
}

/// How far a capture time may be from where the arrival clock puts it.
const MAX_CAPTURE_DRIFT_MS: i64 = 1_000;

impl Timeline {
    pub fn new(start: Instant) -> Self {
        Timeline { start, origin: None, last_capture_ms: 0, use_capture: true }
    }

    /// The RTP time of a frame that arrived at `now`.
    pub fn rtp(&mut self, capture_time_ms: Option<u64>, now: Instant) -> u32 {
        let arrival = now.saturating_duration_since(self.start);
        let by_arrival = (arrival.as_micros() * 90 / 1000) as u32;
        let Some(capture) = capture_time_ms.filter(|_| self.use_capture) else {
            self.use_capture = false;
            return by_arrival;
        };
        let (first_capture, first_arrival) = *self.origin.get_or_insert((capture, arrival));
        let media_ms = capture as i64 - first_capture as i64;
        let clock_ms = (arrival.as_millis() as i64) - (first_arrival.as_millis() as i64);
        if capture < self.last_capture_ms || (media_ms - clock_ms).abs() > MAX_CAPTURE_DRIFT_MS {
            debug!("Connector: capture times are not usable ({} ms against {} ms of the clock); using arrival times", media_ms, clock_ms);
            self.use_capture = false;
            return by_arrival;
        }
        self.last_capture_ms = capture;
        ((first_arrival.as_micros() as u64 * 90 / 1000) as u32).wrapping_add((media_ms as u64 * 90) as u32)
    }
}

/// The length of an Opus packet (RFC 6716, section 3.1: the TOC byte gives
/// the frame size, its low two bits the number of frames).
pub fn opus_duration(packet: &[u8]) -> Option<Duration> {
    let toc = *packet.first()?;
    let config = toc >> 3;
    let frame_us: u64 = match config {
        0..=11 => [10_000, 20_000, 40_000, 60_000][(config % 4) as usize],
        12..=15 => [10_000, 20_000][(config % 2) as usize],
        _ => [2_500, 5_000, 10_000, 20_000][(config % 4) as usize],
    };
    let frames: u64 = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => (*packet.get(1)? & 0x3f) as u64,
    };
    Some(Duration::from_micros(frame_us * frames))
}

// --- measurements of the hop (arrival from the connector -> RTP sent) ---

static MEASURING: AtomicBool = AtomicBool::new(false);
static ARRIVALS: Mutex<VecDeque<(u32, Instant)>> = Mutex::new(VecDeque::new());
static HOP: Mutex<Vec<f32>> = Mutex::new(Vec::new());

/// Called by the RTP writer when the last packet of the frame with this RTP
/// time was written. Does nothing unless a connector is the source.
pub fn frame_sent(rtp_time: u32) {
    if !MEASURING.load(Ordering::Relaxed) {
        return;
    }
    let now = Instant::now();
    let mut arrivals = ARRIVALS.lock().unwrap();
    while let Some((time, arrived)) = arrivals.pop_front() {
        if time == rtp_time {
            HOP.lock().unwrap().push(now.duration_since(arrived).as_secs_f32() * 1000.0);
            break;
        }
    }
}

fn hop_report() -> Option<String> {
    let mut hop = std::mem::take(&mut *HOP.lock().unwrap());
    if hop.is_empty() {
        return None;
    }
    hop.sort_by(|a, b| a.total_cmp(b));
    let at = |p: f32| hop[((hop.len() as f32 * p) as usize).min(hop.len() - 1)];
    Some(format!("hop ms median={:.2} p95={:.2} p99={:.2} max={:.2} (n={})", at(0.5), at(0.95), at(0.99), hop[hop.len() - 1], hop.len()))
}

/// What the viewer sends that is for the computer shown.
pub enum ViewerInput {
    Packet(InboundPacket),
    /// The viewer's clipboard text changed.
    Clipboard(String),
}

/// The viewer's input as connector commands. Positions are scaled to the
/// display shown; wheel steps finer than a notch are added up.
#[derive(Default)]
pub struct InputMapper {
    /// The size of the display shown, in pixels (0: not known yet).
    pub width: u32,
    pub height: u32,
    wheel_rest: (i32, i32),
}

impl InputMapper {
    pub fn commands(&mut self, packet: InboundPacket) -> Vec<Command> {
        match packet {
            InboundPacket::MousePosition { x, y, reference_width, reference_height, .. } => {
                if self.width == 0 || self.height == 0 || reference_width <= 0 || reference_height <= 0 {
                    return Vec::new();
                }
                let scale = |v: i16, reference: i16, size: u32| {
                    ((v.max(0) as i64 * size as i64) / reference as i64).clamp(0, size as i64 - 1) as i32
                };
                vec![Command::PointerMove { x: scale(x, reference_width, self.width), y: scale(y, reference_height, self.height) }]
            }
            InboundPacket::MouseMove { delta_x, delta_y, .. } => vec![Command::PointerMoveRelative { dx: delta_x as i32, dy: delta_y as i32 }],
            InboundPacket::MouseButton { action, button } => vec![Command::PointerButton {
                button: match button {
                    MouseButton::Left => Button::Left,
                    MouseButton::Middle => Button::Middle,
                    MouseButton::Right => Button::Right,
                    MouseButton::X1 => Button::Back,
                    MouseButton::X2 => Button::Forward,
                },
                down: matches!(action, MouseButtonAction::Press),
            }],
            // As the local injector takes them: delta_y > 0 is up, delta_x > 0 right.
            InboundPacket::Scroll { delta_x, delta_y } => self.wheel(delta_x as i32 * 120, delta_y as i32 * 120),
            InboundPacket::HighResScroll { delta_x, delta_y } => self.wheel(delta_x as i32, delta_y as i32),
            InboundPacket::Key { action, key, .. } => match super::keys::position_of_vk(key) {
                Some(position) => vec![Command::Key { position, down: matches!(action, KeyAction::Down) }],
                None => Vec::new(),
            },
            InboundPacket::Text { text } => vec![Command::Text { text }],
            // Touch, controllers and the rest have no place in the interface.
            _ => Vec::new(),
        }
    }

    fn wheel(&mut self, dx: i32, dy: i32) -> Vec<Command> {
        self.wheel_rest.0 += dx;
        self.wheel_rest.1 += dy;
        let (notches_x, notches_y) = (self.wheel_rest.0 / 120, self.wheel_rest.1 / 120);
        self.wheel_rest.0 %= 120;
        self.wheel_rest.1 %= 120;
        if notches_x == 0 && notches_y == 0 {
            return Vec::new();
        }
        vec![Command::Wheel { dx: notches_x, dy: notches_y }]
    }
}

/// The ends of the agent session a source is connected to.
pub struct ViewerLink {
    pub video: mpsc::Sender<VideoFramePayload>,
    pub audio: mpsc::Sender<rtc::media::Sample>,
    /// Cursor updates, in the form the session already forwards.
    pub media_events: mpsc::Sender<MediaEvent>,
    /// The session's requests of its pipeline (key frame, rate, stop).
    pub pipeline_commands: mpsc::Receiver<PipelineCommand>,
    pub input: mpsc::UnboundedReceiver<ViewerInput>,
    /// Clipboard text for the viewer.
    pub clipboard: mpsc::UnboundedSender<String>,
    pub general: Arc<dyn webrtc::data_channel::DataChannel>,
    pub signaling: mpsc::UnboundedSender<common::AgentMessage>,
    pub client_id: String,
    pub peer_connection: Arc<dyn webrtc::peer_connection::PeerConnection>,
    pub connected: Arc<AtomicBool>,
    /// What the viewer asked for at the start.
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// The display of the source to show (its index), when the viewer chose one.
    pub display: Option<usize>,
}

fn displays_message(displays: &[DisplayInfo], current: usize) -> String {
    serde_json::json!({
        "type": "source_displays",
        "current": current.to_string(),
        "displays": displays.iter().enumerate().map(|(index, d)| serde_json::json!({
            "id": index.to_string(),
            "name": if d.name.is_empty() { format!("Display {}", index + 1) } else { d.name.clone() },
            "width": d.width,
            "height": d.height,
        })).collect::<Vec<_>>(),
    })
    .to_string()
}

/// Runs the source for one agent session: until the connector's session
/// ends, the agent's session stops it, or the packager is gone. When the
/// source ends by itself the viewer is told and the session closed.
pub async fn run(source: Source, link: ViewerLink) {
    let Source { codec: session_codec, session, first, pending } = source;
    let Session { mut events, commands } = session;
    let ViewerLink {
        video,
        audio,
        media_events,
        mut pipeline_commands,
        mut input,
        clipboard,
        general,
        signaling,
        client_id,
        peer_connection,
        connected,
        fps,
        bitrate_kbps,
        display,
    } = link;
    let mut timeline = Timeline::new(Instant::now());
    let mut queued: VecDeque<Event> = pending.into_iter().chain(first).collect();
    let mut last_key_request: Option<Instant> = None;
    // A key frame was asked for at this time and has not come yet.
    let mut key_awaited: Option<Instant> = None;
    let test_key_every = std::env::var(TEST_KEY_FRAME_EVERY_ENV).ok().and_then(|v| v.parse::<u64>().ok()).filter(|ms| *ms >= 100);
    let mut test_key_tick = tokio::time::interval(Duration::from_millis(test_key_every.unwrap_or(3_600_000)));
    test_key_tick.tick().await;
    let mut report_at = Instant::now() + Duration::from_secs(5);
    let (mut frames, mut keys, mut bytes, mut dropped) = (0u64, 0u64, 0u64, 0u64);
    let mut mapper = InputMapper::default();
    let mut cursor = lunaris_media::types::CursorState {
        x: 0,
        y: 0,
        visible: true,
        kind: lunaris_media::types::CursorKind::Arrow,
        image: None,
        in_window_move_size: false,
    };
    // The source's displays, to tell the viewer once its channel is open.
    let mut displays_unsent: Option<String> = None;
    let mut channel_poll = tokio::time::interval(Duration::from_millis(250));
    // (viewer input events, commands made of them): kinds only, never which key.
    let (mut input_events, mut input_commands) = (0u64, 0u64);
    ARRIVALS.lock().unwrap().clear();
    HOP.lock().unwrap().clear();
    MEASURING.store(true, Ordering::Relaxed);

    // What the viewer set: the frame rate and the bitrate it would like.
    let _ = commands.try_send(Command::Quality { fps, bitrate_kbps });
    if let Some(index) = display {
        let _ = commands.try_send(Command::SwitchDisplay { index });
    }

    // (why it ended, whether the viewer is to be told, the status to tell)
    let (reason, by_source, status) = loop {
        let event = match queued.pop_front() {
            Some(event) => event,
            None => tokio::select! {
                event = events.recv() => match event {
                    Some(event) => event,
                    None => break ("The connector ended".to_string(), true, "ended"),
                },
                _ = test_key_tick.tick(), if test_key_every.is_some() => {
                    key_awaited.get_or_insert_with(Instant::now);
                    let _ = commands.try_send(Command::RequestKeyFrame);
                    continue;
                }
                _ = channel_poll.tick(), if displays_unsent.is_some() => {
                    if matches!(general.ready_state().await, Ok(webrtc::data_channel::RTCDataChannelState::Open)) {
                        if let Some(message) = displays_unsent.take() {
                            let _ = general.send_text(&message).await;
                        }
                    }
                    continue;
                }
                viewer = input.recv() => {
                    match viewer {
                        Some(ViewerInput::Packet(packet)) => {
                            input_events += 1;
                            for command in mapper.commands(packet) {
                                input_commands += 1;
                                // Input is never dropped silently behind a slow
                                // source: it waits its turn.
                                if commands.send(command).await.is_err() {
                                    break;
                                }
                            }
                        }
                        Some(ViewerInput::Clipboard(text)) => {
                            let _ = commands.send(Command::Clipboard { text }).await;
                        }
                        // The session's channels are gone: the viewer left.
                        None => {
                            let _ = commands.send(Command::Close).await;
                            break ("The viewer left".to_string(), false, "");
                        }
                    }
                    continue;
                }
                command = pipeline_commands.recv() => {
                    let command = match command {
                        None | Some(PipelineCommand::Stop) => {
                            let _ = commands.send(Command::Close).await;
                            break ("The viewer left".to_string(), false, "");
                        }
                        Some(PipelineCommand::RequestKeyframe) => {
                            key_awaited.get_or_insert_with(Instant::now);
                            Command::RequestKeyFrame
                        }
                        Some(PipelineCommand::SetFps(fps)) | Some(PipelineCommand::SetFrameLimit(fps)) => Command::Quality { fps, bitrate_kbps: 0 },
                        Some(PipelineCommand::SetBitrate(kbps)) => Command::Quality { fps: 0, bitrate_kbps: kbps },
                    };
                    let _ = commands.try_send(command);
                    continue;
                }
            },
        };
        match event {
            Event::Video { codec, key, capture_time_ms, data } => {
                if codec != session_codec {
                    // The viewer was offered one codec and another cannot
                    // go on the same track: the viewer is asked to start
                    // the session again, which finds the new codec.
                    let _ = commands.send(Command::Close).await;
                    break (format!("The computer changed its video to {:?}", codec), true, "restart");
                }
                let now = Instant::now();
                let timestamp = timeline.rtp(capture_time_ms, now);
                if key {
                    if let Some(asked) = key_awaited.take() {
                        info!("Connector: key frame {:.0} ms after it was asked for ({} bytes)", now.duration_since(asked).as_secs_f64() * 1000.0, data.len());
                    }
                }
                if !connected.load(Ordering::Relaxed) {
                    continue;
                }
                frames += 1;
                keys += key as u64;
                bytes += data.len() as u64;
                {
                    let mut arrivals = ARRIVALS.lock().unwrap();
                    if arrivals.len() >= 256 {
                        arrivals.pop_front();
                    }
                    arrivals.push_back((timestamp, now));
                }
                if video.try_send(VideoFramePayload { full_frame: data.to_vec(), timestamp }).is_err() {
                    if video.is_closed() {
                        let _ = commands.send(Command::Close).await;
                        break ("The viewer left".to_string(), false, "");
                    }
                    // The frames after a dropped one refer to it.
                    dropped += 1;
                    if last_key_request.is_none_or(|at| now.duration_since(at) >= Duration::from_millis(500)) {
                        last_key_request = Some(now);
                        key_awaited.get_or_insert(now);
                        let _ = commands.try_send(Command::RequestKeyFrame);
                    }
                }
                if now >= report_at {
                    info!(
                        "Connector media: {:.1} frames/s, {} key, {:.2} Mbit/s, {} dropped; {}; input: {} events, {} commands",
                        frames as f64 / 5.0,
                        keys,
                        bytes as f64 * 8.0 / 5.0 / 1e6,
                        dropped,
                        hop_report().unwrap_or_else(|| "no frame sent".into()),
                        input_events,
                        input_commands
                    );
                    (frames, keys, bytes, dropped) = (0, 0, 0, 0);
                    report_at = now + Duration::from_secs(5);
                }
            }
            Event::Audio { data, .. } => {
                let Some(duration) = opus_duration(&data) else { continue };
                let _ = audio.try_send(rtc::media::Sample { data, duration, ..rtc::media::Sample::new(Instant::now()) });
            }
            Event::Displays { displays, current } => {
                info!("Connector: {} displays, showing {} ({:?})", displays.len(), current, displays.get(current));
                if let Some(shown) = displays.get(current) {
                    mapper.width = shown.width;
                    mapper.height = shown.height;
                }
                displays_unsent = Some(displays_message(&displays, current));
            }
            Event::CursorShape { width, height, hot_x, hot_y, rgba } => {
                cursor.image = Some(lunaris_media::types::CursorImage {
                    width,
                    height,
                    hotspot_x: hot_x,
                    hotspot_y: hot_y,
                    rgba_data: rgba.to_vec(),
                });
                let _ = media_events.try_send(MediaEvent::CursorUpdate(cursor.clone()));
                // The picture goes once; positions repeat without it.
                cursor.image = None;
            }
            Event::CursorPosition { x, y, visible } => {
                cursor.x = x;
                cursor.y = y;
                cursor.visible = visible;
                let _ = media_events.try_send(MediaEvent::CursorUpdate(cursor.clone()));
            }
            Event::Clipboard { text } => {
                let _ = clipboard.send(text);
            }
            Event::Closed { reason } => break (reason, true, "ended"),
        }
    };
    MEASURING.store(false, Ordering::Relaxed);
    info!("Connector: the source ended: {} ({} input events, {} commands)", reason, input_events, input_commands);
    if by_source {
        // The viewer is told why, and the session is closed: no frozen picture.
        let _ = signaling.send(common::AgentMessage::Signaling(common::SignalingMessage::SourceStatus {
            target_id: client_id,
            status: status.to_string(),
            reason: Some(reason),
        }));
        let _ = peer_connection.close().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_times_are_used_while_they_follow_the_clock() {
        let start = Instant::now();
        let mut timeline = Timeline::new(start);
        let at = |ms: u64| start + Duration::from_millis(ms);
        // The first frame arrives 100 ms in; its capture time is on the host's own clock.
        assert_eq!(timeline.rtp(Some(5_000), at(100)), 9_000);
        // 16 ms later by the host, 30 ms later here (network jitter): the host's spacing is kept.
        assert_eq!(timeline.rtp(Some(5_016), at(130)), 9_000 + 16 * 90);
        assert_eq!(timeline.rtp(Some(5_033), at(134)), 9_000 + 33 * 90);
    }

    #[test]
    fn capture_times_that_jump_or_go_back_are_dropped_for_good() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut jump = Timeline::new(start);
        jump.rtp(Some(0), at(0));
        assert_eq!(jump.rtp(Some(60_000), at(16)), 16 * 90);
        assert_eq!(jump.rtp(Some(60_016), at(32)), 32 * 90);
        let mut back = Timeline::new(start);
        back.rtp(Some(1_000), at(0));
        back.rtp(Some(1_016), at(16));
        assert_eq!(back.rtp(Some(1_000), at(32)), 32 * 90);
        let mut none = Timeline::new(start);
        assert_eq!(none.rtp(None, at(50)), 50 * 90);
    }

    #[test]
    fn opus_packet_lengths() {
        // CELT fullband 10 ms, one frame (config 30); 20 ms (config 31); two 20 ms frames.
        assert_eq!(opus_duration(&[30 << 3]), Some(Duration::from_millis(10)));
        assert_eq!(opus_duration(&[31 << 3]), Some(Duration::from_millis(20)));
        assert_eq!(opus_duration(&[(31 << 3) | 1]), Some(Duration::from_millis(40)));
        // SILK 60 ms (config 3), three frames by the count byte.
        assert_eq!(opus_duration(&[(3 << 3) | 3, 3]), Some(Duration::from_millis(180)));
        assert_eq!(opus_duration(&[]), None);
    }

    #[test]
    fn the_viewer_is_offered_what_it_asked_for_first() {
        // A client from before it said what it decodes: today's guess.
        assert_eq!(viewer_codecs("h265", None), vec![Codec::H265, Codec::H264, Codec::Vp9, Codec::Vp8, Codec::Av1]);
        assert_eq!(viewer_codecs("vp9", None)[0], Codec::Vp9);
        assert!(!viewer_codecs("h264", None).contains(&Codec::H265));
        // A client that says: only what it names, the one asked for first.
        let names = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(viewer_codecs("av1", Some(&names(&["H264", "av1", "vp8", "theora"]))), vec![Codec::Av1, Codec::H264, Codec::Vp8]);
        // Asked for something it does not decode: not offered.
        assert_eq!(viewer_codecs("h265", Some(&names(&["h264"]))), vec![Codec::H264]);
        assert!(viewer_codecs("h264", Some(&[])).is_empty());
    }

    #[test]
    fn the_viewers_input_becomes_commands_for_the_display_shown() {
        use crate::input::{KeyFlags, KeyModifiers};
        let mut mapper = InputMapper::default();
        let position = |x, y| InboundPacket::MousePosition { x, y, reference_width: 1000, reference_height: 500, timestamp: 0 };
        // Before the display's size is known a position means nothing.
        assert!(mapper.commands(position(500, 250)).is_empty());
        mapper.width = 1920;
        mapper.height = 1080;
        assert_eq!(mapper.commands(position(500, 250)), vec![Command::PointerMove { x: 960, y: 540 }]);
        assert_eq!(mapper.commands(position(1000, 500)), vec![Command::PointerMove { x: 1919, y: 1079 }]);
        assert_eq!(
            mapper.commands(InboundPacket::MouseMove { delta_x: -3, delta_y: 4, timestamp: 0 }),
            vec![Command::PointerMoveRelative { dx: -3, dy: 4 }]
        );
        assert_eq!(
            mapper.commands(InboundPacket::MouseButton { action: MouseButtonAction::Press, button: MouseButton::X1 }),
            vec![Command::PointerButton { button: Button::Back, down: true }]
        );
        // Whole notches at once; fine steps add up to one.
        assert_eq!(mapper.commands(InboundPacket::Scroll { delta_x: 0, delta_y: -2 }), vec![Command::Wheel { dx: 0, dy: -2 }]);
        assert!(mapper.commands(InboundPacket::HighResScroll { delta_x: 0, delta_y: 60 }).is_empty());
        assert_eq!(mapper.commands(InboundPacket::HighResScroll { delta_x: 0, delta_y: 60 }), vec![Command::Wheel { dx: 0, dy: 1 }]);
        // A key goes by its place on the keyboard (VK_A is KEY_A, 30).
        let key = |action| InboundPacket::Key { action, modifiers: KeyModifiers::empty(), key: 0x41, flags: KeyFlags::empty() };
        assert_eq!(mapper.commands(key(KeyAction::Down)), vec![Command::Key { position: 30, down: true }]);
        assert_eq!(mapper.commands(key(KeyAction::Up)), vec![Command::Key { position: 30, down: false }]);
        assert_eq!(mapper.commands(InboundPacket::Text { text: "ố".into() }), vec![Command::Text { text: "ố".into() }]);
        assert!(mapper.commands(InboundPacket::GeneralStop).is_empty());
    }
}
