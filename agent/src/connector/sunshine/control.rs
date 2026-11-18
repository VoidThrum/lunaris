//! The GameStream control stream as Moonlight runs it with a Sunshine host
//! (generation 7, `appversion` 7.1.431 and later): ENet over UDP, every
//! message encrypted with the session's key.
//!
//! Ported from moonlight-common-c (GPLv3) `ControlStream.c`
//! (`startControlStream`, `sendMessageEnet`, `encryptControlMessage`,
//! `decryptControlMessageToV1`, `lossStatsThreadFunc`, `requestIdrFrame`,
//! `controlReceiveThreadFunc`, `stopControlStream`) and `Misc.c`
//! (`gracefullyDisconnectEnetPeer`). The host's side was read in Sunshine
//! `src/stream.cpp` (`control_server_t`, `controlBroadcastThread`,
//! `encode_control`), tag v2026.914 and master: the same on the wire (master
//! only renames and tidies there).
//!
//! ENet: the reference and Sunshine use cgutman's fork (moonlight-common-c
//! submodule `enet`); this uses `rusty_enet`, stock ENet 1.3.18 in Rust.
//! Compared file by file (fork against stock 2662c0d): `protocol.h` is the
//! same but for a console-only MTU; `protocol.c` differs in how addresses
//! are kept (IPv6, no check of the sender's address, the local address
//! remembered), in the round-trip and retransmission arithmetic, a QoS
//! socket option and how long `enet_host_service` sleeps; `enet.h` has a
//! default MTU of 900 instead of 1392. Nothing in the packet formats. Used
//! the way the reference does: 0x30 channels asked for (Sunshine's host
//! allows 255: `enet_host_create(af, &addr, 128, 0, 0, 0)` in network.cpp),
//! the connect data of the RTSP SETUP, no checksum, no compression, MTU 900.
//! One thing the reference does through ENet's intercept hook, which
//! `rusty_enet` does not have, is done in the socket wrapper instead
//! ([`EnetSocket`]): the host's DISCONNECT is kept back for a moment so that
//! the termination message before it is not lost.
//!
//! Not here: the unencrypted control stream of older hosts and its loss
//! statistics message (0x0201; `usePeriodicPing` is true for every host from
//! 7.1.415 on, which sends the periodic ping instead), reference frame
//! invalidation and LTR acknowledgements (every loss asks for an IDR frame).

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aes_gcm::aead::consts::U16;
use aes_gcm::{AeadInOut, Aes128Gcm, AesGcm, KeyInit};
use rusty_enet as enet;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use super::http::Error;
use super::video::SS_FRAME_FEC_PTYPE;

// Limelight-internal.h
pub const CTRL_CHANNEL_GENERIC: u8 = 0x00;
/// "IDR, LTR ACK and RFI"
pub const CTRL_CHANNEL_URGENT: u8 = 0x01;
pub const CTRL_CHANNEL_KEYBOARD: u8 = 0x02;
pub const CTRL_CHANNEL_MOUSE: u8 = 0x03;
pub const CTRL_CHANNEL_COUNT: usize = 0x30;

// ControlStream.c packetTypesGen7Enc (and stream.cpp packetTypes)
/// "Request IDR frame"; also what stands at the place of "Start A".
pub const PTYPE_REQUEST_IDR_FRAME: u16 = 0x0302;
pub const PTYPE_START_B: u16 = 0x0307;
pub const PTYPE_INPUT_DATA: u16 = 0x0206;
pub const PTYPE_RUMBLE_DATA: u16 = 0x010b;
/// "Termination (extended)": a big-endian 32-bit reason.
pub const PTYPE_TERMINATION: u16 = 0x0109;
/// The short termination message of older hosts (packetTypesGen7).
pub const PTYPE_TERMINATION_SHORT: u16 = 0x0100;
pub const PTYPE_HDR_MODE: u16 = 0x010e;
/// lossStatsThreadFunc: the periodic ping.
pub const PTYPE_PERIODIC_PING: u16 = 0x0200;
/// The encrypted envelope (NVCTL_ENCRYPTED_PACKET_HEADER.encryptedHeaderType).
pub const PTYPE_ENCRYPTED: u16 = 0x0001;

/// requestIdrFrameGen7Enc
const REQUEST_IDR_PAYLOAD: [u8; 2] = [0, 0];
/// startBGen5
const START_B_PAYLOAD: [u8; 1] = [0];
/// lossStatsThreadFunc: `BbPut32(4)` "Length of payload", `BbPut32(0)` "Timestamp?".
const PERIODIC_PING_PAYLOAD: [u8; 8] = [4, 0, 0, 0, 0, 0, 0, 0];

const PERIODIC_PING_INTERVAL: Duration = Duration::from_millis(100);
const CONTROL_STREAM_TIMEOUT: Duration = Duration::from_secs(10);
const CONTROL_STREAM_LINGER_TIMEOUT: Duration = Duration::from_secs(2);
/// controlReceiveThreadFunc: "Wait 100 ms for pending receives after a disconnect".
const DISCONNECT_GRACE: Duration = Duration::from_millis(100);
/// initializeControlStream: "Limits number of frame status reports per periodic ping interval".
const FEC_STATUS_QUEUE: usize = 8;
/// cgutman's enet.h ENET_HOST_DEFAULT_MTU.
const ENET_MTU: u16 = 900;
/// How long the task sleeps at most between two looks at ENet (its
/// retransmission timers have no wake-up of their own here).
const SERVICE_INTERVAL: Duration = Duration::from_millis(5);

const AES_GCM_TAG_LENGTH: usize = 16;
/// encryptedHeaderType, length, seq.
const ENCRYPTED_HEADER_SIZE: usize = 8;

// ControlStream.c controlReceiveThreadFunc
/// NVST_DISCONN_SERVER_TERMINATED_CLOSED: what Sunshine sends when its
/// session ends in an orderly way (stream.cpp controlBroadcastThread).
pub const TERMINATION_CLOSED: u32 = 0x8003_0023;
/// NVST_DISCONN_SERVER_VIDEO_ENCODER_CONVERT_INPUT_FRAME_FAILED
pub const TERMINATION_FRAME_CONVERSION: u32 = 0x800e_9403;
/// NVST_DISCONN_SERVER_VFP_PROTECTED_CONTENT
pub const TERMINATION_PROTECTED_CONTENT: u32 = 0x800e_9302;
/// SERVER_TERMINATED_INTENDED of the short message.
pub const TERMINATION_SHORT_INTENDED: u32 = 0x0100;

/// Why the host ended the session, in words for the person
/// (ControlStream.c controlReceiveThreadFunc's reading of the reason;
/// `saw_frame`: `lastSeenFrame != 0`).
pub fn termination_text(code: u32, saw_frame: bool) -> String {
    match code {
        TERMINATION_CLOSED | TERMINATION_SHORT_INTENDED if saw_frame => {
            "The session was ended at the other computer".to_string()
        }
        // ML_ERROR_UNEXPECTED_EARLY_TERMINATION
        TERMINATION_CLOSED | TERMINATION_SHORT_INTENDED => {
            "The other computer ended the session before it sent a picture. Its screen may be off or locked.".to_string()
        }
        // ML_ERROR_FRAME_CONVERSION
        TERMINATION_FRAME_CONVERSION => "The other computer could not prepare its picture for sending".to_string(),
        // ML_ERROR_PROTECTED_CONTENT
        TERMINATION_PROTECTED_CONTENT => {
            "The other computer shows protected content, which it does not send".to_string()
        }
        other => format!("The other computer ended the session (reason {other:#010x})"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CipherError {
    /// Shorter than its headers, or its length field does not fit.
    Malformed,
    /// Not the encrypted envelope (type 0x0001).
    NotEncrypted,
    /// The tag does not hold.
    Authentication,
}

/// The encryption of control messages.
///
/// A message on the wire (ControlStream.c NVCTL_ENCRYPTED_PACKET_HEADER;
/// stream.cpp control_encrypted_t), little-endian:
/// `0x0001`, `length` (u16: 4 + 16 + what was encrypted), `seq` (u32), the
/// 16-byte tag, then the ciphertext of `type` (u16), `payloadLength` (u16),
/// payload. AES-128-GCM with the launch's `rikey`, no associated data.
///
/// IV (populateOutgoingIv / populateIncomingIv; stream.cpp encode_control
/// and the IDX_ENCRYPTED handler): with `SS_ENC_CONTROL_V2` twelve bytes,
/// `seq` little-endian in the first four, 'C' (client) or 'H' (host) at
/// index 10 and 'C' at 11; without it NVIDIA's sixteen bytes, the first one
/// `seq` cut to a byte, the same for both directions.
#[derive(Clone)]
pub struct ControlCipher {
    key: [u8; 16],
    v2: bool,
}

impl ControlCipher {
    /// `v2`: `SS_ENC_CONTROL_V2` is in `x-ss-general.encryptionEnabled`.
    pub fn new(key: [u8; 16], v2: bool) -> ControlCipher {
        ControlCipher { key, v2 }
    }

    fn crypt(&self, seq: u32, from_host: bool, data: &mut [u8], tag: Option<&[u8; 16]>) -> Result<[u8; 16], CipherError> {
        if self.v2 {
            let mut iv = [0u8; 12];
            iv[..4].copy_from_slice(&seq.to_le_bytes());
            iv[10] = if from_host { b'H' } else { b'C' };
            iv[11] = b'C';
            let cipher = Aes128Gcm::new(&self.key.into());
            match tag {
                None => cipher.encrypt_inout_detached(&iv.into(), &[], data.into()).map(Into::into),
                Some(tag) => cipher.decrypt_inout_detached(&iv.into(), &[], data.into(), tag.into()).map(|_| *tag),
            }
        } else {
            // "This is a truncating cast, but it's what Nvidia does, so we have to mimic it."
            let mut iv = [0u8; 16];
            iv[0] = seq as u8;
            let cipher = AesGcm::<aes_gcm::aes::Aes128, U16>::new(&self.key.into());
            match tag {
                None => cipher.encrypt_inout_detached(&iv.into(), &[], data.into()).map(Into::into),
                Some(tag) => cipher.decrypt_inout_detached(&iv.into(), &[], data.into(), tag.into()).map(|_| *tag),
            }
        }
        .map_err(|_| CipherError::Authentication)
    }

    /// One message ready for ENet (sendMessageEnet + encryptControlMessage).
    /// `from_host` is false for everything this side sends.
    pub fn seal(&self, seq: u32, from_host: bool, ptype: u16, payload: &[u8]) -> Vec<u8> {
        let mut plain = Vec::with_capacity(4 + payload.len());
        plain.extend_from_slice(&ptype.to_le_bytes());
        plain.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        plain.extend_from_slice(payload);
        let tag = self.crypt(seq, from_host, &mut plain, None).expect("AES-GCM of a control message");
        let length = (4 + AES_GCM_TAG_LENGTH + plain.len()) as u16;
        let mut out = Vec::with_capacity(ENCRYPTED_HEADER_SIZE + AES_GCM_TAG_LENGTH + plain.len());
        out.extend_from_slice(&PTYPE_ENCRYPTED.to_le_bytes());
        out.extend_from_slice(&length.to_le_bytes());
        out.extend_from_slice(&seq.to_le_bytes());
        out.extend_from_slice(&tag);
        out.extend_from_slice(&plain);
        out
    }

    /// Opens one message: (seq, type, payload). decryptControlMessageToV1;
    /// the payload is what follows the inner header, cut to its length field
    /// when that is shorter.
    pub fn open(&self, from_host: bool, packet: &[u8]) -> Result<(u32, u16, Vec<u8>), CipherError> {
        if packet.len() < 2 {
            return Err(CipherError::Malformed);
        }
        if u16::from_le_bytes([packet[0], packet[1]]) != PTYPE_ENCRYPTED {
            return Err(CipherError::NotEncrypted);
        }
        if packet.len() < ENCRYPTED_HEADER_SIZE {
            return Err(CipherError::Malformed);
        }
        let length = u16::from_le_bytes([packet[2], packet[3]]) as usize;
        let seq = u32::from_le_bytes(packet[4..8].try_into().unwrap());
        // "Make sure the host isn't lying to us about the packet length"; "Received runt packet"
        if packet.len() < length + 4 || length < 4 + AES_GCM_TAG_LENGTH + 4 {
            return Err(CipherError::Malformed);
        }
        let tag: [u8; 16] = packet[8..24].try_into().unwrap();
        let mut plain = packet[24..length + 4].to_vec();
        self.crypt(seq, from_host, &mut plain, Some(&tag))?;
        let ptype = u16::from_le_bytes([plain[0], plain[1]]);
        let declared = u16::from_le_bytes([plain[2], plain[3]]) as usize;
        let mut payload = plain.split_off(4);
        payload.truncate(declared.min(payload.len()));
        Ok((seq, ptype, payload))
    }
}

/// ControlStream.c ignoreDisconnectIntercept: whether a datagram is ENet's
/// protocol header followed by exactly one DISCONNECT command.
fn is_lone_disconnect(datagram: &[u8]) -> bool {
    // ENetProtocolHeader: peerID (big-endian, the top bit says a sentTime
    // follows), then the commands. ENetProtocolDisconnect: command, channel,
    // reliable sequence number, data: 8 bytes.
    const HEADER_FLAG_SENT_TIME: u16 = 1 << 15;
    const COMMAND_MASK: u8 = 0x0F;
    const COMMAND_DISCONNECT: u8 = 4;
    if datagram.len() < 2 {
        return false;
    }
    let header = if u16::from_be_bytes([datagram[0], datagram[1]]) & HEADER_FLAG_SENT_TIME != 0 { 4 } else { 2 };
    datagram.len() == header + 8 && datagram[header] & COMMAND_MASK == COMMAND_DISCONNECT
}

/// `tokio`'s UDP socket in the shape `rusty_enet` asks for: nothing blocks.
///
/// With `hold_disconnect` it also does what the reference does with ENet's
/// intercept hook (ControlStream.c ignoreDisconnectIntercept: "drops
/// disconnect events to allow us to process pending receives first. It works
/// around what appears to be a bug in ENet where pending disconnects can
/// cause loss of unprocessed received data"): a datagram that is nothing but
/// the host's DISCONNECT is kept back until [`EnetSocket::release`], so that
/// a termination message sent just before it is still read.
pub(super) struct EnetSocket {
    socket: Arc<UdpSocket>,
    hold_disconnect: bool,
    held: Option<(Vec<u8>, SocketAddr)>,
    release: bool,
    round_trips: RoundTrips,
}

impl EnetSocket {
    pub(super) fn new(socket: Arc<UdpSocket>, hold_disconnect: bool) -> EnetSocket {
        EnetSocket { socket, hold_disconnect, held: None, release: false, round_trips: RoundTrips::default() }
    }

    /// A DISCONNECT of the host waits here.
    fn holds_disconnect(&self) -> bool {
        self.held.is_some()
    }

    /// Lets ENet have the DISCONNECT at its next look.
    fn release(&mut self) {
        self.release = true;
    }
}

impl enet::Socket for EnetSocket {
    type Address = SocketAddr;
    type Error = std::io::Error;

    fn init(&mut self, _options: enet::SocketOptions) -> Result<(), std::io::Error> {
        Ok(())
    }

    fn send(&mut self, address: SocketAddr, buffer: &[u8]) -> Result<usize, std::io::Error> {
        match self.socket.try_send_to(buffer, address) {
            Ok(n) => {
                self.round_trips.sent(buffer, Instant::now());
                Ok(n)
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(0),
            // An ICMP "port unreachable" for an earlier datagram: the peer's
            // time-out decides, not this (VideoStream.c VideoPingThreadProc
            // gives the same reason for not checking sendto).
            Err(e) if matches!(e.kind(), std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::ConnectionReset) => Ok(0),
            Err(e) => Err(e),
        }
    }

    fn receive(
        &mut self,
        buffer: &mut [u8; enet::MTU_MAX],
    ) -> Result<Option<(SocketAddr, enet::PacketReceived)>, std::io::Error> {
        if self.release {
            if let Some((datagram, from)) = self.held.take() {
                buffer[..datagram.len()].copy_from_slice(&datagram);
                return Ok(Some((from, enet::PacketReceived::Complete(datagram.len()))));
            }
        }
        loop {
            match self.socket.try_recv_from(buffer) {
                Ok((n, from)) => {
                    self.round_trips.received(&buffer[..n], Instant::now());
                    if self.hold_disconnect && self.held.is_none() && is_lone_disconnect(&buffer[..n]) {
                        log::debug!("sunshine: the host disconnects; reading what it sent before");
                        self.held = Some((buffer[..n].to_vec(), from));
                        continue;
                    }
                    return Ok(Some((from, enet::PacketReceived::Complete(n))));
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
                Err(e) if matches!(e.kind(), std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::ConnectionReset) => {
                    continue
                }
                Err(e) => return Err(e),
            }
        }
    }
}

/// What the control stream is asked to do.
enum Request {
    Idr,
    FecStatus([u8; 21]),
    Input { channel: u8, data: Vec<u8> },
    Close,
}

/// What the control stream tells its owner. After any of these it is over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlEvent {
    /// The host's termination message with its reason
    /// ([`termination_text`]).
    Terminated { code: u32 },
    /// ENet gave the peer up (no answer for 10 s) or the host disconnected
    /// without a word.
    Lost,
    /// The socket failed.
    Failed,
}

/// Counters for the end-of-session log line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ControlStats {
    pub pings_sent: u64,
    pub idr_requests_sent: u64,
    pub fec_status_sent: u64,
    pub input_messages_sent: u64,
    /// Messages of the host that were opened and understood or politely
    /// ignored (rumble, HDR, controller extensions).
    pub received: u64,
    /// Messages of the host that did not decrypt, came out of order, or
    /// were not encrypted.
    pub rejected: u64,
    /// ENet's smoothed round trip time of the acknowledged pings, in ms, as
    /// last seen while connected (it starts at ENet's 500 and settles within
    /// a few seconds).
    pub round_trip_ms: u32,
    /// The round trip measured on the datagrams (`RoundTrips`): the median
    /// of the last second's, in microseconds, as last known; 0 = none yet.
    pub round_trip_us: u32,
}

/// The round trip between this computer and the host to tell the viewer
/// now (`Event::Latency`), in tenths of a millisecond, given what was told
/// last; `None` when there is nothing new. The figure is `round_trip_us`
/// (`RoundTrips`); 0 there means that nothing was measured yet.
pub fn round_trip_to_tell(stats: &ControlStats, told: Option<u32>) -> Option<u32> {
    if stats.round_trip_us == 0 {
        return None;
    }
    let tenths = (stats.round_trip_us + 50) / 100;
    (told != Some(tenths)).then_some(tenths)
}

/// The round trips of the control stream, measured on its datagrams.
///
/// ENet measures them itself, and that is the figure Moonlight shows
/// (moonlight-common-c ControlStream.c `LiGetEstimatedRttInfo`:
/// `peer->roundTripTime`). But `rusty_enet` 0.4.1 gives only its smoothed
/// figure (`Peer::round_trip_time`), in whole milliseconds, which starts at
/// 500 ms and moves by `diff / 8` (c/protocol.rs
/// `enet_protocol_handle_acknowledge`): with whole numbers it stops up to
/// 7 ms away from what is measured (Moonlight's ENet rounds up,
/// `(diff + 7) / 8`). So the same measurement is made here, on what ENet
/// itself puts on the wire, with this computer's clock:
///
/// - A datagram that carries a reliable command has ENet's time of sending
///   in its header (`ENetProtocolHeader.sentTime`, the flag
///   `ENET_PROTOCOL_HEADER_FLAG_SENT_TIME`; c/protocol.rs
///   `enet_protocol_send_outgoing_commands`).
/// - The host acknowledges each reliable command with an ACKNOWLEDGE that
///   repeats that time (`ENetProtocolAcknowledge.receivedSentTime`;
///   `enet_protocol_handle_incoming_commands` queues it with the header's
///   `sentTime`, `enet_protocol_send_acknowledgements` writes it).
/// - ENet's own sample is `serviceTime - receivedSentTime`
///   (`enet_protocol_handle_acknowledge`), in milliseconds.
///
/// Here the moment each such datagram left is kept by its `sentTime`, and
/// the sample is the time from then to the acknowledgement's arrival, in
/// microseconds. A command sent again leaves in a datagram with a new
/// `sentTime`, so a sample never mixes two sendings. The periodic ping goes
/// out reliable every 100 ms ("to ensure the RTT is recomputed regularly",
/// ControlStream.c lossStatsThreadFunc): about ten samples a second.
///
/// Told is the median of the samples of the last second (Lunaris's own
/// choice: one late acknowledgement, or one lucky one, does not move it).
/// Like ENet's figure it holds the time the host takes to read the
/// datagram and send the acknowledgement.
#[derive(Default)]
pub(super) struct RoundTrips {
    /// (`sentTime` of a datagram, when it left, whether a sample was taken of it)
    sent: VecDeque<(u16, Instant, bool)>,
    /// (when the acknowledgement came, the round trip in microseconds)
    samples: VecDeque<(Instant, u32)>,
}

const ENET_HEADER_FLAG_SENT_TIME: u16 = 1 << 15;
const ENET_HEADER_FLAG_COMPRESSED: u16 = 1 << 14;
const ENET_COMMAND_MASK: u8 = 0x0F;
const ENET_COMMAND_ACKNOWLEDGE: u8 = 1;
/// Datagrams whose time of leaving is kept (25 s of pings).
const ROUND_TRIP_SENT_KEPT: usize = 256;
/// The samples that count: those of the last second.
const ROUND_TRIP_WINDOW: Duration = Duration::from_secs(1);

/// The size of an ENet command without its data, and where its 16-bit data
/// length is, if it has data (rusty_enet c/protocol.rs: the
/// `ENetProtocol*` structs and `COMMAND_SIZES`). `None`: not a command.
fn enet_command_layout(command: u8) -> Option<(usize, Option<usize>)> {
    Some(match command & ENET_COMMAND_MASK {
        1 => (8, None),      // ACKNOWLEDGE
        2 => (48, None),     // CONNECT
        3 => (44, None),     // VERIFY_CONNECT
        4 => (8, None),      // DISCONNECT
        5 => (4, None),      // PING
        6 => (6, Some(4)),   // SEND_RELIABLE
        7 => (8, Some(6)),   // SEND_UNRELIABLE
        8 => (24, Some(6)),  // SEND_FRAGMENT
        9 => (8, Some(6)),   // SEND_UNSEQUENCED
        10 => (12, None),    // BANDWIDTH_LIMIT
        11 => (16, None),    // THROTTLE_CONFIGURE
        12 => (24, Some(6)), // SEND_UNRELIABLE_FRAGMENT
        _ => return None,
    })
}

impl RoundTrips {
    /// A datagram of this side left at `now`.
    pub(super) fn sent(&mut self, datagram: &[u8], now: Instant) {
        if datagram.len() < 4 || u16::from_be_bytes([datagram[0], datagram[1]]) & ENET_HEADER_FLAG_SENT_TIME == 0 {
            return;
        }
        let sent_time = u16::from_be_bytes([datagram[2], datagram[3]]);
        // Two datagrams within one of ENet's milliseconds: the first stands for both.
        if self.sent.back().is_some_and(|(last, _, _)| *last == sent_time) {
            return;
        }
        if self.sent.len() >= ROUND_TRIP_SENT_KEPT {
            self.sent.pop_front();
        }
        self.sent.push_back((sent_time, now, false));
    }

    /// A datagram of the host came at `now`: its acknowledgements are samples.
    pub(super) fn received(&mut self, datagram: &[u8], now: Instant) {
        if datagram.len() < 2 {
            return;
        }
        let flags = u16::from_be_bytes([datagram[0], datagram[1]]);
        if flags & ENET_HEADER_FLAG_COMPRESSED != 0 {
            return; // not used by either side here
        }
        let mut offset = if flags & ENET_HEADER_FLAG_SENT_TIME != 0 { 4 } else { 2 };
        while offset + 4 <= datagram.len() {
            let Some((size, data_length_at)) = enet_command_layout(datagram[offset]) else { return };
            if offset + size > datagram.len() {
                return;
            }
            if datagram[offset] & ENET_COMMAND_MASK == ENET_COMMAND_ACKNOWLEDGE {
                let received_sent_time = u16::from_be_bytes([datagram[offset + 6], datagram[offset + 7]]);
                // The newest datagram with that time (the 16 bits come round every 65 s).
                if let Some(entry) = self.sent.iter_mut().rev().find(|(sent_time, _, _)| *sent_time == received_sent_time) {
                    if !std::mem::replace(&mut entry.2, true) {
                        let micros = now.saturating_duration_since(entry.1).as_micros().min(u32::MAX as u128) as u32;
                        self.samples.push_back((now, micros));
                    }
                }
            }
            let data = data_length_at.map_or(0, |at| u16::from_be_bytes([datagram[offset + at], datagram[offset + at + 1]]) as usize);
            offset += size + data;
        }
    }

    /// The median of the samples of the last second before `now`, in
    /// microseconds; `None` when there is none.
    pub(super) fn median_us(&mut self, now: Instant) -> Option<u32> {
        while self.samples.front().is_some_and(|(at, _)| now.saturating_duration_since(*at) > ROUND_TRIP_WINDOW) {
            self.samples.pop_front();
        }
        if self.samples.is_empty() {
            return None;
        }
        let mut values: Vec<u32> = self.samples.iter().map(|(_, micros)| *micros).collect();
        values.sort_unstable();
        Some(values[values.len() / 2])
    }
}

pub struct ControlParams {
    /// The host's address with the port of the control SETUP.
    pub remote: SocketAddr,
    /// `X-SS-Connect-Data`.
    pub connect_data: u32,
    /// The launch's `rikey`.
    pub key: [u8; 16],
    /// `SS_ENC_CONTROL_V2` was enabled in the ANNOUNCE.
    pub encryption_v2: bool,
}

/// A running control stream. Dropping it ends it (the host is told, as far
/// as two seconds allow).
pub struct Control {
    requests: mpsc::UnboundedSender<Request>,
    events: mpsc::UnboundedReceiver<ControlEvent>,
    stats: Arc<std::sync::Mutex<ControlStats>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Control {
    /// startControlStream: connects, sets the peer's time-out, sends
    /// "start A" and "start B", and leaves a task that pings every 100 ms.
    pub async fn connect(params: ControlParams) -> Result<Control, Error> {
        let bind: SocketAddr = if params.remote.is_ipv4() { "0.0.0.0:0".parse().unwrap() } else { "[::]:0".parse().unwrap() };
        let socket = Arc::new(UdpSocket::bind(bind).await.map_err(|_| Error::Internal("no UDP socket for the control stream"))?);
        // ENet's sends go through `try_send_to` ([`EnetSocket`]), which gives up
        // without trying while the runtime has not yet learnt that the new
        // socket can be written to: the CONNECT would be dropped unseen and
        // sent again only after ENet's first time-out, half a second later.
        socket.writable().await.map_err(|_| Error::Internal("no UDP socket for the control stream"))?;
        // enet_host_create(family, local, 1, CTRL_CHANNEL_COUNT, 0, 0)
        let mut host = enet::Host::new(
            EnetSocket::new(socket.clone(), true),
            enet::HostSettings { peer_limit: 1, channel_limit: CTRL_CHANNEL_COUNT, ..Default::default() },
        )
        .map_err(|_| Error::Internal("ENet host"))?;
        let _ = host.set_mtu(ENET_MTU);
        // enet_host_connect(client, &remoteAddress, CTRL_CHANNEL_COUNT, ControlConnectData)
        let peer = host
            .connect(params.remote, CTRL_CHANNEL_COUNT, params.connect_data)
            .map_err(|_| Error::Internal("ENet peer"))?
            .id();

        // "Wait for the connect to complete"
        let deadline = tokio::time::Instant::now() + CONTROL_STREAM_TIMEOUT;
        'connect: loop {
            loop {
                match host.service() {
                    Ok(Some(enet::Event::Connect { .. })) => break 'connect,
                    Ok(Some(enet::Event::Disconnect { .. })) => {
                        log::info!("sunshine: the host refused the control connection");
                        return Err(Error::Unreachable);
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(_) => return Err(Error::Unreachable),
                }
            }
            if tokio::time::Instant::now() >= deadline {
                log::info!("sunshine: no answer on the control port (UDP {})", params.remote.port());
                return Err(Error::Timeout);
            }
            let _ = tokio::time::timeout(SERVICE_INTERVAL, socket.readable()).await;
        }
        // "Ensure the connect verify ACK is sent immediately";
        // "Set the peer timeout to 10 seconds and limit backoff to 2x RTT"
        host.flush();
        host.peer_mut(peer).set_timeout(2, 10000, 10000);

        let (requests, request_rx) = mpsc::unbounded_channel();
        let (event_tx, events) = mpsc::unbounded_channel();
        let stats = Arc::new(std::sync::Mutex::new(ControlStats::default()));
        let mut link = Link {
            host,
            peer,
            cipher: ControlCipher::new(params.key, params.encryption_v2),
            v2: params.encryption_v2,
            send_seq: 0,
            last_received_seq: None,
            stats: ControlStats::default(),
        };
        // "Send START A" (for the encrypted generation 7 the table has the IDR
        // request at that place), "Send START B".
        link.send(PTYPE_REQUEST_IDR_FRAME, &REQUEST_IDR_PAYLOAD, CTRL_CHANNEL_GENERIC, enet::PacketKind::Reliable);
        link.send(PTYPE_START_B, &START_B_PAYLOAD, CTRL_CHANNEL_GENERIC, enet::PacketKind::Reliable);
        link.host.flush();

        let shared = stats.clone();
        let task = tokio::spawn(async move { link.run(socket, request_rx, event_tx, shared).await });
        Ok(Control { requests, events, stats, task: Some(task) })
    }

    /// requestIdrFrame: the host answers with a key frame.
    pub fn request_idr(&self) {
        let _ = self.requests.send(Request::Idr);
    }

    /// connectionSendFrameFecStatus: [`super::video::FrameFecStatus::to_bytes`];
    /// best effort, for the host's log.
    pub fn send_fec_status(&self, status: [u8; 21]) {
        let _ = self.requests.send(Request::FecStatus(status));
    }

    /// sendInputPacketOnControlStream: one input packet (input.rs) on one of
    /// the `CTRL_CHANNEL_*`, reliable.
    pub fn send_input(&self, channel: u8, data: Vec<u8>) {
        let _ = self.requests.send(Request::Input { channel, data });
    }

    /// The end of the stream, when the host or the network ends it.
    pub async fn event(&mut self) -> Option<ControlEvent> {
        self.events.recv().await
    }

    pub fn stats(&self) -> ControlStats {
        *self.stats.lock().unwrap()
    }

    /// stopControlStream: lets ENet deliver what is still queued and tells
    /// the host that this side leaves (at most two seconds).
    pub async fn close(mut self) -> ControlStats {
        let _ = self.requests.send(Request::Close);
        if let Some(task) = self.task.take() {
            let _ = tokio::time::timeout(CONTROL_STREAM_LINGER_TIMEOUT + Duration::from_millis(500), task).await;
        }
        self.stats()
    }
}

struct Link {
    host: enet::Host<EnetSocket>,
    peer: enet::PeerID,
    cipher: ControlCipher,
    v2: bool,
    /// currentEnetSequenceNumber
    send_seq: u32,
    last_received_seq: Option<u32>,
    stats: ControlStats,
}

enum Incoming {
    Nothing,
    Ended(ControlEvent),
}

impl Link {
    /// sendMessageEnet for an encrypted stream.
    fn send(&mut self, ptype: u16, payload: &[u8], channel: u8, kind: enet::PacketKind) {
        let message = self.cipher.seal(self.send_seq, false, ptype, payload);
        self.send_seq = self.send_seq.wrapping_add(1);
        let peer = self.host.peer_mut(self.peer);
        // "if the requested channel exceeds the peer's supported channel count" use channel 0
        let channel = if (channel as usize) < peer.channel_count() { channel } else { 0 };
        let _ = peer.send(channel, &enet::Packet::new(message.as_slice(), kind));
    }

    /// controlReceiveThreadFunc for one received packet.
    fn received(&mut self, data: &[u8]) -> Incoming {
        let (seq, ptype, payload) = match self.cipher.open(true, data) {
            Ok(opened) => opened,
            Err(e) => {
                // "Discarding unencrypted packet on encrypted control stream" / "Failed to decrypt control packet"
                log::debug!("sunshine: control message dropped: {e:?}");
                self.stats.rejected += 1;
                return Incoming::Nothing;
            }
        };
        // PlatformCrypto.c isReplayedMessage keeps a window of sequence
        // numbers seen per channel (new-style encryption only:
        // CIPHER_FLAG_NO_REPLAY_CHECK for NVIDIA's one-byte IV, which repeats
        // by design). Stricter here: the host sends everything on one
        // reliable channel (stream.cpp control_server_t::send), so its
        // numbers only grow.
        if self.v2 {
            if self.last_received_seq.is_some_and(|last| seq <= last) {
                self.stats.rejected += 1;
                return Incoming::Nothing;
            }
            self.last_received_seq = Some(seq);
        }
        self.stats.received += 1;
        match ptype {
            PTYPE_TERMINATION | PTYPE_TERMINATION_SHORT => {
                // "the extended termination message which contains a full HRESULT" (big-endian),
                // or "the short termination message" (little-endian, 16 bits)
                let code = if payload.len() >= 4 {
                    u32::from_be_bytes(payload[..4].try_into().unwrap())
                } else if payload.len() >= 2 {
                    u16::from_le_bytes([payload[0], payload[1]]) as u32
                } else {
                    0
                };
                log::info!("sunshine: the host ends the session, reason {code:#010x}");
                // "The server will also not acknowledge our disconnect message
                // once it sends this message, so we mark the peer as fully
                // disconnected now"
                self.host.peer_mut(self.peer).disconnect_now(0);
                self.host.flush();
                Incoming::Ended(ControlEvent::Terminated { code })
            }
            // Rumble, HDR mode and metadata, trigger rumble, motion sensors,
            // controller LEDs, adaptive triggers, player LEDs: nothing here
            // has a game controller or an HDR picture.
            _ => Incoming::Nothing,
        }
    }

    /// Lets ENet work until it has nothing more to say.
    fn service(&mut self) -> Incoming {
        loop {
            let data = match self.host.service() {
                Ok(Some(enet::Event::Receive { packet, .. })) => packet.data().to_vec(),
                Ok(Some(enet::Event::Disconnect { .. })) => {
                    // "Control stream received unexpected disconnect event"
                    return Incoming::Ended(ControlEvent::Lost);
                }
                Ok(Some(enet::Event::Connect { .. })) => continue,
                Ok(None) => return Incoming::Nothing,
                Err(e) => {
                    log::info!("sunshine: control socket failed: {}", e.kind());
                    return Incoming::Ended(ControlEvent::Failed);
                }
            };
            if let Incoming::Ended(event) = self.received(&data) {
                return Incoming::Ended(event);
            }
        }
    }

    /// One request of the owner; true when it asks for the end.
    fn request(&mut self, request: Request, fec_status: &mut VecDeque<[u8; 21]>) -> bool {
        match request {
            Request::Idr => {
                // requestIdrFrame: CTRL_CHANNEL_URGENT, reliable
                self.send(PTYPE_REQUEST_IDR_FRAME, &REQUEST_IDR_PAYLOAD, CTRL_CHANNEL_URGENT, enet::PacketKind::Reliable);
                self.stats.idr_requests_sent += 1;
            }
            Request::FecStatus(status) => {
                // A queue of 8: more than that per ping interval is not sent.
                if fec_status.len() < FEC_STATUS_QUEUE {
                    fec_status.push_back(status);
                }
            }
            Request::Input { channel, data } => {
                self.send(PTYPE_INPUT_DATA, &data, channel, enet::PacketKind::Reliable);
                self.stats.input_messages_sent += 1;
            }
            Request::Close => return true,
        }
        false
    }

    fn publish(&mut self, shared: &std::sync::Mutex<ControlStats>) {
        // Only while connected: ENet puts the figure back to its starting
        // value of 500 ms when the peer is reset at the disconnect.
        let peer = self.host.peer(self.peer);
        if peer.connected() {
            self.stats.round_trip_ms = peer.round_trip_time().as_millis() as u32;
        }
        if let Some(micros) = self.host.socket_mut().round_trips.median_us(Instant::now()) {
            // Never 0: that is "none yet".
            self.stats.round_trip_us = micros.max(1);
        }
        *shared.lock().unwrap() = self.stats;
    }

    async fn run(
        mut self,
        socket: Arc<UdpSocket>,
        mut requests: mpsc::UnboundedReceiver<Request>,
        events: mpsc::UnboundedSender<ControlEvent>,
        shared: Arc<std::sync::Mutex<ControlStats>>,
    ) {
        let mut fec_status: VecDeque<[u8; 21]> = VecDeque::new();
        let mut ping = tokio::time::interval(PERIODIC_PING_INTERVAL);
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // When the host's DISCONNECT was first seen (and kept back).
        let mut disconnect_seen: Option<tokio::time::Instant> = None;
        loop {
            if let Incoming::Ended(event) = self.service() {
                self.publish(&shared);
                let _ = events.send(event);
                return;
            }
            // "Now that no pending receive events remain, we can remove our
            // intercept hook and allow the server's disconnect to be processed"
            if self.host.socket().holds_disconnect() {
                let seen = *disconnect_seen.get_or_insert_with(tokio::time::Instant::now);
                if seen.elapsed() >= DISCONNECT_GRACE {
                    self.host.socket_mut().release();
                    continue;
                }
            }
            tokio::select! {
                _ = socket.readable() => {}
                _ = tokio::time::sleep(SERVICE_INTERVAL) => {}
                _ = ping.tick() => {
                    // lossStatsThreadFunc: "For Sunshine servers, send the more
                    // detailed per-frame FEC messages" as unsequenced packets,
                    // then the ping, reliable "to ensure the RTT is recomputed regularly".
                    while let Some(status) = fec_status.pop_front() {
                        self.send(SS_FRAME_FEC_PTYPE, &status, CTRL_CHANNEL_GENERIC, enet::PacketKind::Unreliable { sequenced: false });
                        self.stats.fec_status_sent += 1;
                    }
                    self.send(PTYPE_PERIODIC_PING, &PERIODIC_PING_PAYLOAD, CTRL_CHANNEL_GENERIC, enet::PacketKind::Reliable);
                    self.stats.pings_sent += 1;
                    self.host.flush();
                    self.publish(&shared);
                }
                request = requests.recv() => {
                    // What else is waiting goes out in the same datagram
                    // (sendMessageEnet's `moreData`).
                    let mut next = request;
                    let mut stop = false;
                    loop {
                        match next {
                            Some(request) => {
                                if self.request(request, &mut fec_status) {
                                    stop = true;
                                    break;
                                }
                            }
                            None => {
                                stop = true;
                                break;
                            }
                        }
                        match requests.try_recv() {
                            Ok(request) => next = Some(request),
                            Err(mpsc::error::TryRecvError::Empty) => break,
                            Err(mpsc::error::TryRecvError::Disconnected) => next = None,
                        }
                    }
                    self.host.flush();
                    if stop {
                        break;
                    }
                }
            }
        }

        self.publish(&shared);
        // Misc.c gracefullyDisconnectEnetPeer: "Begin the disconnection
        // process. We'll get ENET_EVENT_TYPE_DISCONNECT once the peer acks all
        // outstanding reliable sends."
        self.host.peer_mut(self.peer).disconnect_later(0);
        self.host.flush();
        let deadline = tokio::time::Instant::now() + CONTROL_STREAM_LINGER_TIMEOUT;
        'linger: while tokio::time::Instant::now() < deadline {
            loop {
                match self.host.service() {
                    Ok(Some(enet::Event::Disconnect { .. })) | Err(_) => break 'linger,
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                }
            }
            let _ = tokio::time::timeout(SERVICE_INTERVAL, socket.readable()).await;
        }
        self.publish(&shared);
    }
}

#[cfg(test)]
#[path = "control_tests.rs"]
mod tests;
