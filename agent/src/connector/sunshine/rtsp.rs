//! The RTSP exchange that sets a GameStream session up, as Moonlight does it
//! with a Sunshine host (generation 7, `appversion` 7.1.431 and later): over
//! TCP, one connection per request, encrypted when the launch answered with
//! `rtspenc://`.
//!
//! Ported from moonlight-common-c (GPLv3): `RtspConnection.c`
//! (`performRtspHandshake`, `sealRtspMessage`, `unsealRtspMessage`,
//! `transactRtspMessageTcp`, the request builders), `RtspParser.c`
//! (`serializeRtspMessage`) and `SdpGenerator.c` (`getAttributesList`,
//! `addGen5Options`, `fillSdpHeader`, `fillSdpTail`). The host's side was read
//! in Sunshine `src/rtsp.cpp` (tag v2026.914; master differs only by
//! `config::cap_stream_resolution` in `cmd_announce` and a fix of the
//! surround mapping in `cmd_describe`: nothing on the wire).
//!
//! Not here: RTSP over ENet (hosts of generation 5 to 7 before 7.1.404) and
//! the SDP of generations 3 to 6. Sunshine is neither.
//!
//! Nothing here logs a key; a request's text has none, but is not logged
//! either.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use aes_gcm::{AeadInOut, Aes128Gcm, KeyInit};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::http::Error;
use super::net::Reachability;
use super::video::{AppVersion, VideoCodec, ENC_VIDEO_HEADER_SIZE};

// RtspConnection.c
const RTSP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const RTSP_RECEIVE_TIMEOUT: Duration = Duration::from_secs(15);
const RTSP_RETRY_DELAY: Duration = Duration::from_millis(500);
const MAX_RTSP_RESPONSE_SIZE: usize = 1024 * 1024;
/// RtspConnection.c `ENCRYPTED_RTSP_BIT`.
const ENCRYPTED_RTSP_BIT: u32 = 0x8000_0000;
/// RtspConnection.c `sizeof(ENC_RTSP_HEADER)`: typeAndLength, sequenceNumber
/// (both big-endian), tag[16].
pub(super) const ENC_RTSP_HEADER_SIZE: usize = 24;
/// Sunshine rtsp.cpp `socket_t::msg_buf`: one request, header and payload,
/// must be shorter than this (and an encrypted one together with its header).
pub const HOST_REQUEST_BUFFER: usize = 2048;

/// Where RTSP is when the launch names no port (Connection.c LiStartConnection).
pub const DEFAULT_RTSP_PORT: u16 = 48010;
// RtspConnection.c performRtspHandshake: "Use the well known port if parsing fails".
const DEFAULT_AUDIO_PORT: u16 = 48000;
const DEFAULT_VIDEO_PORT: u16 = 47998;
const DEFAULT_CONTROL_PORT: u16 = 47999;

// Limelight-internal.h
pub const SS_ENC_CONTROL_V2: u32 = 0x01;
pub const SS_ENC_VIDEO: u32 = 0x02;
pub const SS_ENC_AUDIO: u32 = 0x04;
/// "Client sends SS_FRAME_FEC_STATUS for frame losses".
pub const ML_FF_FEC_STATUS: u32 = 0x01;
/// "Client supports X-SS-Ping-Payload and X-SS-Connect-Data".
pub const ML_FF_SESSION_ID_V1: u32 = 0x02;
/// moonlight-qt app/streaming/session.cpp: "Use 1392 byte video packets by
/// default"; here only for a host found on the local link (`net.rs`).
pub const PACKET_SIZE_LOCAL: usize = 1392;
/// Connection.c LiStartConnection: "Cap packet size at 1024 for remote IPv4
/// streaming to avoid fragmentation"; moonlight-qt: the same over a VPN.
/// Used here for every IPv4 host that is not known to be on the local link.
pub const PACKET_SIZE_IPV4: usize = 1024;
/// "IPv6 guarantees a minimum MTU of 1280 before fragmentation": 1184.
pub const PACKET_SIZE_IPV6: usize = 1184;
/// Video bitrate (kbit/s) from which Moonlight asks for high-quality audio.
pub const HIGH_AUDIO_BITRATE_THRESHOLD: u32 = 15000;

// Limelight.h
pub const ENCFLG_AUDIO: u32 = 0x1;
pub const ENCFLG_VIDEO: u32 = 0x2;
pub const ENCFLG_ALL: u32 = 0xFFFF_FFFF;

// SdpGenerator.c
const NVFF_BASE: u32 = 0x07;
const NVFF_AUDIO_ENCRYPTION: u32 = 0x20;
const NVFF_RI_ENCRYPTION: u32 = 0x80;

/// The 12-byte IV of an RTSP message: the sequence number little-endian in
/// the first four bytes, 'C' (client) or 'H' (host) and 'R' at the end
/// (RtspConnection.c sealRtspMessage / unsealRtspMessage; rtsp.cpp respond,
/// handle_read_encrypted_message).
fn rtsp_iv(seq: u32, from_host: bool) -> [u8; 12] {
    let mut iv = [0u8; 12];
    iv[..4].copy_from_slice(&seq.to_le_bytes());
    iv[10] = if from_host { b'H' } else { b'C' };
    iv[11] = b'R';
    iv
}

/// Encrypts one RTSP message (RtspConnection.c sealRtspMessage; the host's
/// answers are made the same way, rtsp.cpp respond).
pub(super) fn seal(key: &[u8; 16], seq: u32, from_host: bool, plaintext: &[u8]) -> Vec<u8> {
    let cipher = Aes128Gcm::new(key.into());
    let mut out = Vec::with_capacity(ENC_RTSP_HEADER_SIZE + plaintext.len());
    out.extend_from_slice(&(ENCRYPTED_RTSP_BIT | plaintext.len() as u32).to_be_bytes());
    out.extend_from_slice(&seq.to_be_bytes());
    out.extend_from_slice(&[0u8; 16]);
    out.extend_from_slice(plaintext);
    let iv = rtsp_iv(seq, from_host);
    let tag = cipher
        .encrypt_inout_detached(&iv.into(), &[], (&mut out[ENC_RTSP_HEADER_SIZE..]).into())
        .expect("AES-GCM of an RTSP message");
    out[8..24].copy_from_slice(&tag);
    out
}

/// Decrypts one whole RTSP message: its sequence number and its text
/// (RtspConnection.c unsealRtspMessage).
pub(super) fn unseal(key: &[u8; 16], from_host: bool, raw: &[u8]) -> Result<(u32, Vec<u8>), Error> {
    if raw.len() <= ENC_RTSP_HEADER_SIZE {
        return Err(Error::Protocol("RTSP encrypted header too small"));
    }
    let type_and_len = u32::from_be_bytes(raw[0..4].try_into().unwrap());
    if type_and_len & ENCRYPTED_RTSP_BIT == 0 {
        return Err(Error::Protocol("unencrypted RTSP message on an encrypted session"));
    }
    let len = (type_and_len & !ENCRYPTED_RTSP_BIT) as usize;
    if len + ENC_RTSP_HEADER_SIZE != raw.len() {
        return Err(Error::Protocol("encrypted RTSP message of the wrong length"));
    }
    let seq = u32::from_be_bytes(raw[4..8].try_into().unwrap());
    let tag: [u8; 16] = raw[8..24].try_into().unwrap();
    let mut plaintext = raw[ENC_RTSP_HEADER_SIZE..].to_vec();
    Aes128Gcm::new(key.into())
        .decrypt_inout_detached(&rtsp_iv(seq, from_host).into(), &[], plaintext.as_mut_slice().into(), &tag.into())
        .map_err(|_| Error::Protocol("RTSP message failed authentication"))?;
    Ok((seq, plaintext))
}

/// An RTSP answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtspResponse {
    pub status: u32,
    pub status_text: String,
    pub headers: Vec<(String, String)>,
    pub payload: Vec<u8>,
}

impl RtspResponse {
    /// RtspParser.c getOptionContent (names compare without case).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// Reads an answer: status line, options, then whatever follows the empty
/// line as payload (RtspParser.c parseRtspMessage).
pub(super) fn parse_response(raw: &[u8]) -> Result<RtspResponse, Error> {
    let (head_end, body_start) = match raw.windows(4).position(|w| w == b"\r\n\r\n") {
        Some(p) => (p, p + 4),
        None => match raw.windows(2).position(|w| w == b"\n\n") {
            Some(p) => (p, p + 2),
            None => (raw.len(), raw.len()),
        },
    };
    let head = std::str::from_utf8(&raw[..head_end]).map_err(|_| Error::Protocol("RTSP answer is not text"))?;
    let mut lines = head.lines();
    let mut words = lines.next().unwrap_or("").splitn(3, ' ');
    let protocol = words.next().unwrap_or("");
    if !protocol.starts_with("RTSP/") {
        return Err(Error::Protocol("not an RTSP answer"));
    }
    let status = words.next().and_then(|w| w.parse().ok()).ok_or(Error::Protocol("RTSP answer without a status"))?;
    let status_text = words.next().unwrap_or("").trim().to_string();
    let mut headers = Vec::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }
    }
    Ok(RtspResponse { status, status_text, headers, payload: raw[body_start..].to_vec() })
}

/// A request as RtspParser.c serializeRtspMessage writes it.
pub(super) fn serialize_request(method: &str, target: &str, headers: &[(&str, String)], payload: &[u8]) -> Vec<u8> {
    let mut out = format!("{method} {target} RTSP/1.0\r\n").into_bytes();
    for (name, value) in headers {
        out.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(payload);
    out
}

/// What the answer to DESCRIBE tells (RtspConnection.c performRtspHandshake;
/// rtsp.cpp cmd_describe writes it).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Describe {
    /// `sprop-parameter-sets=AAAAAU`: the base64 start of an HEVC VPS, which
    /// is how a host says it can encode H.265.
    pub hevc: bool,
    /// `AV1/90000`.
    pub av1: bool,
    /// `x-nv-video[0].refPicInvalidation`: the encoder can invalidate
    /// reference frames (not used here: every loss asks for an IDR frame).
    pub reference_frame_invalidation: bool,
    /// `x-ss-general.featureFlags` (Sunshine: platf::get_capabilities(), pen
    /// and touch, controller touch pads; says nothing about the system).
    pub feature_flags: u32,
    /// `x-ss-general.encryptionSupported`: `SS_ENC_*`.
    pub encryption_supported: u32,
    /// `x-ss-general.encryptionRequested`: `SS_ENC_*`.
    pub encryption_requested: u32,
}

impl Describe {
    /// Whether the host says it can encode `codec` (H.264 always).
    pub fn offers(&self, codec: VideoCodec) -> bool {
        match codec {
            VideoCodec::H264 => true,
            VideoCodec::H265 => self.hevc,
            VideoCodec::Av1 => self.av1,
        }
    }
}

/// `strtoul(text, NULL, 0)` for what hosts write: decimal or `0x` hex, 0
/// when the text starts with no number.
fn c_strtoul(text: &str) -> u32 {
    let value = text.trim_start();
    let (digits, radix) = match value.strip_prefix("0x").or_else(|| value.strip_prefix("0X")) {
        Some(hex) => (hex, 16),
        None => (value, 10),
    };
    let end = digits.find(|c: char| !c.is_digit(radix)).unwrap_or(digits.len());
    u32::from_str_radix(&digits[..end], radix).unwrap_or(0)
}

/// RtspConnection.c parseSdpAttributeToUInt: the number after the first ':'
/// that follows the first place the name appears.
fn sdp_attribute_u32(payload: &str, name: &str) -> Option<u32> {
    let rest = &payload[payload.find(name)?..];
    Some(c_strtoul(&rest[rest.find(':')? + 1..]))
}

pub fn parse_describe(payload: &str) -> Describe {
    Describe {
        hevc: payload.contains("sprop-parameter-sets=AAAAAU"),
        av1: payload.contains("AV1/90000"),
        reference_frame_invalidation: payload.contains("x-nv-video[0].refPicInvalidation"),
        feature_flags: sdp_attribute_u32(payload, "x-ss-general.featureFlags").unwrap_or(0),
        encryption_supported: sdp_attribute_u32(payload, "x-ss-general.encryptionSupported").unwrap_or(0),
        encryption_requested: sdp_attribute_u32(payload, "x-ss-general.encryptionRequested").unwrap_or(0),
    }
}

/// PlatformSockets.c isPrivateNetworkAddress: what Moonlight takes for "the
/// host is on this network" (STREAM_CFG_AUTO in Connection.c
/// LiStartConnection). Loopback is not in the list there, and not here.
pub fn is_private_network_address(addr: &IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => {
            let a = u32::from(*v4);
            (a & 0xFF00_0000) == 0x0A00_0000 // 10.0.0.0/8
                || (a & 0xFFF0_0000) == 0xAC10_0000 // 172.16.0.0/12
                || (a & 0xFFFF_0000) == 0xC0A8_0000 // 192.168.0.0/16
                || (a & 0xFFFF_0000) == 0xA9FE_0000 // 169.254.0.0/16
        }
        IpAddr::V6(v6) => {
            let s = v6.segments()[0];
            (s & 0xFFC0) == 0xFE80 // fe80::/10
                || (s & 0xFFC0) == 0xFEC0 // fec0::/10
                || (s & 0xFE00) == 0xFC00 // fc00::/7
        }
    }
}

/// PlatformSockets.c addrToUrlSafeString: IPv6 in brackets.
pub(super) fn url_safe_address(addr: &IpAddr) -> String {
    match addr {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => format!("[{v6}]"),
    }
}

/// What the person and the viewer want from the stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamRequest {
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// The whole stream's bitrate in kbit/s (Limelight.h
    /// STREAM_CONFIGURATION.bitrate).
    pub bitrate_kbps: u32,
    /// `ENCFLG_*`: what this side would like encrypted beyond the control
    /// stream. The host's demands are followed in any case.
    pub encryption_flags: u32,
}

/// What both sides ended up with: the receivers and the control stream are
/// built from this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Negotiated {
    /// `x-ss-general.encryptionEnabled` as sent: `SS_ENC_*`.
    pub encryption_enabled: u32,
    /// The video stream is encrypted (`SS_ENC_VIDEO`).
    pub video_encrypted: bool,
    /// The audio stream is encrypted (SdpGenerator.c AudioEncryptionEnabled:
    /// asked by `ENCFLG_AUDIO` through `x-nv-general.featureFlags` bit 0x20,
    /// or agreed as `SS_ENC_AUDIO`).
    pub audio_encrypted: bool,
    /// `x-nv-video[0].packetSize` as sent (already without the 32 bytes of the
    /// video encryption header): the value `VideoParams::packet_size` wants.
    pub packet_size: usize,
    /// `x-nv-aqos.packetDuration` in milliseconds.
    pub audio_packet_duration_ms: u32,
    /// Moonlight's "streamingRemotely": the host is not on a private network.
    pub remote: bool,
}

/// Limelight.h COLORSPACE_REC_709 << 1 | COLOR_RANGE_LIMITED: what
/// `x-nv-video[0].encoderCscMode` asks for. Moonlight sends what its decoder
/// prefers; the viewers here are browsers and desktop decoders that take an
/// HD picture as Rec. 709 limited range, and the host writes its choice into
/// the bitstream's colour description as well (Sunshine video.cpp).
const ENCODER_CSC_MODE: u32 = (1 << 1) | 0;

/// The SDP of the ANNOUNCE for a generation-7 Sunshine host, attribute by
/// attribute in the order of SdpGenerator.c getAttributesList, and what it
/// settles.
///
/// Choices where the reference asks its caller: stereo, 8 bit 4:2:0 SDR, one
/// slice per frame, one reference frame (no reference frame invalidation),
/// 5 ms audio packets.
pub fn build_sdp(
    request: &StreamRequest,
    describe: &Describe,
    app_version: AppVersion,
    remote_addr: &IpAddr,
    reach: Reachability,
    video_port: u16,
) -> (String, Negotiated) {
    // moonlight-qt session.cpp: on the local link STREAM_CFG_LOCAL "even if
    // it's not in RFC 1918 space", over a VPN STREAM_CFG_REMOTE "even if the
    // target address is in RFC 1918 address space"; otherwise
    // STREAM_CFG_AUTO, which Connection.c settles by the address.
    let remote = match reach {
        Reachability::Lan => false,
        Reachability::Vpn => true,
        Reachability::Unknown => !is_private_network_address(remote_addr),
    };
    let mut attributes: Vec<(&str, String)> = Vec::new();
    let mut add = |name: &'static str, value: String| attributes.push((name, value));

    // getAttributesList: "Send client feature flags to Sunshine hosts"
    add("x-ml-general.featureFlags", (ML_FF_FEC_STATUS | ML_FF_SESSION_ID_V1).to_string());

    // "New-style control stream encryption is low overhead, so we enable it any time it is supported"
    let mut enabled = 0u32;
    if describe.encryption_supported & SS_ENC_CONTROL_V2 != 0 {
        enabled |= SS_ENC_CONTROL_V2;
    }
    // "If video encryption is supported by the host and desired by the client, use it";
    // "explicitly requested by the host but *not* by the client, we'll encrypt anyway"
    if (describe.encryption_supported & SS_ENC_VIDEO != 0 && request.encryption_flags & ENCFLG_VIDEO != 0)
        || (describe.encryption_requested & SS_ENC_VIDEO != 0 && request.encryption_flags & ENCFLG_VIDEO == 0)
    {
        enabled |= SS_ENC_VIDEO;
    }
    if (describe.encryption_supported & SS_ENC_AUDIO != 0 && request.encryption_flags & ENCFLG_AUDIO != 0)
        || (describe.encryption_requested & SS_ENC_AUDIO != 0 && request.encryption_flags & ENCFLG_AUDIO == 0)
    {
        enabled |= SS_ENC_AUDIO;
    }
    add("x-ss-general.encryptionEnabled", enabled.to_string());
    add("x-ss-video[0].chromaSamplingType", "0".into()); // 4:2:0
    add("x-ss-video[0].intraRefresh", "0".into());

    add("x-nv-video[0].clientViewportWd", request.width.to_string());
    add("x-nv-video[0].clientViewportHt", request.height.to_string());
    add("x-nv-video[0].maxFPS", request.fps.to_string());

    // The packet size: Connection.c LiStartConnection keeps the caller's
    // size when the host counts as local and caps it at 1024 (IPv4) or 1184
    // (IPv6) "to avoid fragmentation" when it is remote; moonlight-qt
    // (app/streaming/session.cpp) asks for 1392 ("Use 1392 byte video packets
    // by default") on a link it found to be the local network, and for 1024
    // over what looks like a VPN. A private address alone says nothing of
    // the path: the first real host was 192.168.x.x behind a WireGuard
    // tunnel with an MTU of 1420, and 1392-byte packets (1436 bytes of IP
    // packet) never arrived there. So where the reference lets the address
    // decide (reachability unknown, STREAM_CFG_AUTO) the capped size is
    // asked for whatever the address looks like: Limelight.h says of this
    // value "use 1024 if unsure". A multiple of 16. getAttributesList:
    // "Adjust the video packet size to account for encryption overhead".
    let mut packet_size: usize = match (reach, remote_addr) {
        (Reachability::Lan, _) => PACKET_SIZE_LOCAL,
        (Reachability::Vpn, _) => PACKET_SIZE_IPV4,
        (Reachability::Unknown, IpAddr::V4(_)) => PACKET_SIZE_IPV4,
        (Reachability::Unknown, IpAddr::V6(_)) => PACKET_SIZE_IPV6,
    };
    packet_size -= packet_size % 16;
    if enabled & SS_ENC_VIDEO != 0 {
        packet_size -= ENC_VIDEO_HEADER_SIZE;
    }
    add("x-nv-video[0].packetSize", packet_size.to_string());
    add("x-nv-video[0].rateControlMode", "4".into());
    add("x-nv-video[0].timeoutLengthMs", "7000".into());
    add("x-nv-video[0].framesWithInvalidRefThreshold", "0".into());

    // "20% of the video bitrate will added to the user-specified bitrate for FEC";
    // remote: "Subtract 500 Kbps to leave room for audio and control"; the cap of 100 Mbps.
    let mut adjusted = (request.bitrate_kbps as f64 * 0.80) as i64;
    if remote && adjusted > 500 {
        adjusted -= 500;
    }
    let adjusted = adjusted.min(100_000);
    add("x-nv-video[0].initialBitrateKbps", adjusted.to_string());
    add("x-nv-video[0].initialPeakBitrateKbps", adjusted.to_string());
    add("x-nv-vqos[0].bw.minimumBitrateKbps", adjusted.to_string());
    add("x-nv-vqos[0].bw.maximumBitrateKbps", adjusted.to_string());
    // "Send the configured bitrate to Sunshine hosts, so they can adjust for dynamic FEC percentage"
    add("x-ml-video.configuredBitrateKbps", request.bitrate_kbps.to_string());

    add("x-nv-vqos[0].fec.enable", "1".into());
    add("x-nv-vqos[0].videoQualityScoreUpdateTime", "5000".into());
    // "If the remote host is local (RFC 1918), enable QoS tagging for our traffic"
    add("x-nv-vqos[0].qosTrafficType", if remote { "0" } else { "5" }.into());
    add("x-nv-aqos.qosTrafficType", if remote { "0" } else { "4" }.into());

    // addGen5Options, APP_VERSION_AT_LEAST(7, 1, 431): "RI encryption is always enabled";
    // "Enable audio encryption if the client opted in or the host required it"
    let audio_encrypted = request.encryption_flags & ENCFLG_AUDIO != 0 || enabled & SS_ENC_AUDIO != 0;
    let mut feature_flags = NVFF_BASE | NVFF_RI_ENCRYPTION;
    if audio_encrypted {
        feature_flags |= NVFF_AUDIO_ENCRYPTION;
    }
    add("x-nv-general.featureFlags", feature_flags.to_string());
    // "Ask for the encrypted control protocol to ensure remote input will be encrypted."
    add("x-nv-general.useReliableUdp", "13".into());
    add("x-nv-vqos[0].fec.minRequiredFecPackets", "2".into());
    add("x-nv-vqos[0].bllFec.enable", "0".into());
    if app_version.at_least(7, 1, 446) && (request.width < 720 || request.height < 540) {
        add("x-nv-vqos[0].drc.enable", "1".into());
        add("x-nv-vqos[0].drc.tableType", "2".into());
    } else {
        add("x-nv-vqos[0].drc.enable", "0".into());
    }
    add("x-nv-general.enableRecoveryMode", "0".into());

    // back in getAttributesList, `AppVersionQuad[0] >= 4`
    add("x-nv-video[0].videoEncoderSlicesPerFrame", "1".into());
    match request.codec {
        VideoCodec::Av1 => add("x-nv-vqos[0].bitStreamFormat", "2".into()),
        VideoCodec::H265 => {
            add("x-nv-clientSupportHevc", "1".into());
            add("x-nv-vqos[0].bitStreamFormat", "1".into());
        }
        VideoCodec::H264 => {
            add("x-nv-clientSupportHevc", "0".into());
            add("x-nv-vqos[0].bitStreamFormat", "0".into());
        }
    }
    add("x-nv-video[0].dynamicRangeMode", "0".into()); // SDR
    // "Restrict the video stream to 1 reference frame if we're not using reference frame invalidation."
    add("x-nv-video[0].maxNumReferenceFrames", "1".into());
    // The client's display rate in the reference; here the rate asked for
    // (Sunshine drops a value that is not within 1 % of maxFPS, rtsp.cpp cmd_announce).
    add("x-nv-video[0].clientRefreshRateX100", (request.fps * 100).to_string());
    add("x-nv-audio.surround.numChannels", "2".into());
    add("x-nv-audio.surround.channelMask", "3".into());
    add("x-nv-audio.surround.enable", "0".into());
    add("x-nv-audio.surround.AudioQuality", "0".into());
    let audio_packet_duration_ms = 5;
    add("x-nv-aqos.packetDuration", audio_packet_duration_ms.to_string());
    add("x-nv-video[0].encoderCscMode", ENCODER_CSC_MODE.to_string());

    // fillSdpHeader, fillSerializedAttributeList ("a=%s:" value " \r\n"), fillSdpTail
    let mut sdp = format!(
        "v=0\r\no=android 0 14 IN {} {}\r\ns=NVIDIA Streaming Client\r\n",
        if remote_addr.is_ipv4() { "IPv4" } else { "IPv6" },
        url_safe_address(remote_addr)
    );
    for (name, value) in &attributes {
        sdp.push_str(&format!("a={name}:{value} \r\n"));
    }
    sdp.push_str(&format!("t=0 0\r\nm=video {video_port}  \r\n"));

    let negotiated = Negotiated {
        encryption_enabled: enabled,
        video_encrypted: enabled & SS_ENC_VIDEO != 0,
        audio_encrypted,
        packet_size,
        audio_packet_duration_ms,
        remote,
    };
    (sdp, negotiated)
}

/// What one SETUP answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupAnswer {
    /// `Transport: ... server_port=N`.
    pub port: Option<u16>,
    /// `X-SS-Ping-Payload`: 16 characters to send in every UDP ping.
    pub ping_payload: Option<[u8; 16]>,
    /// `X-SS-Connect-Data`: the number to give ENet when connecting.
    pub connect_data: u32,
}

/// RtspConnection.c parseServerPortFromTransport.
fn server_port(response: &RtspResponse) -> Option<u16> {
    let transport = response.header("Transport")?;
    let rest = &transport[transport.find("server_port=")? + "server_port=".len()..];
    let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    rest[..end].parse::<u16>().ok().filter(|p| *p != 0)
}

/// The port after the last colon of a session URL (Connection.c
/// parseRtspPortNumberFromUrl).
pub fn rtsp_port_from_url(session_url: Option<&str>) -> u16 {
    session_url
        .and_then(|url| url.rsplit_once(':'))
        .and_then(|(_, port)| {
            let end = port.find(|c: char| !c.is_ascii_digit()).unwrap_or(port.len());
            port[..end].parse::<u16>().ok()
        })
        .filter(|p| *p != 0)
        .unwrap_or(DEFAULT_RTSP_PORT)
}

/// The host part of a session URL, IPv6 with its brackets (RtspConnection.c
/// parseUrlAddrFromRtspUrlString).
fn url_host(url: &str) -> Option<String> {
    let rest = &url[url.find("//")? + 2..];
    let host = if let Some(end) = rest.find(']') {
        &rest[..=end]
    } else if let Some(colon) = rest.rfind(':') {
        &rest[..colon]
    } else if let Some(slash) = rest.find('/') {
        &rest[..slash]
    } else {
        rest
    };
    (!host.is_empty()).then(|| host.to_string())
}

/// The requests of one RTSP handshake, in the order of RtspConnection.c
/// performRtspHandshake: [`options`](Self::options),
/// [`describe`](Self::describe), [`setup_audio`](Self::setup_audio) (the
/// caller starts its audio pings then, as notifyAudioPortNegotiationComplete
/// does), [`setup_video`](Self::setup_video),
/// [`setup_control`](Self::setup_control), [`announce`](Self::announce),
/// [`play`](Self::play).
pub struct RtspClient {
    addr: SocketAddr,
    /// What goes after the method (rtspTargetUrl).
    target_url: String,
    /// The `Host` option (urlAddr).
    host_header: String,
    /// The session's key when RTSP is encrypted.
    key: Option<[u8; 16]>,
    /// CSeq of the next request (currentSeqNumber).
    cseq: u32,
    /// encryptionSequenceNumber: counted up before each message.
    encryption_seq: u32,
    /// The sequence number of the host's last answer (it starts at 1).
    last_host_seq: u32,
    session_id: Option<String>,
    app_version: AppVersion,
}

impl std::fmt::Debug for RtspClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RtspClient {{ encrypted: {}, cseq: {} }}", self.key.is_some(), self.cseq)
    }
}

impl RtspClient {
    /// `session_url`: `sessionUrl0` of the launch answer. `remote`: the host's
    /// address as the person gave it, resolved (the reference connects there
    /// and not to the URL's address). `rikey`: the launch's key.
    /// `high_quality_audio`: RtspConnection.c performRtspHandshake names the
    /// host by its real address only then, and `0.0.0.0` otherwise; a
    /// Sunshine host reads the `Host` option of the ANNOUNCE for exactly that
    /// (rtsp.cpp cmd_announce: 96 or 256 kbit/s per channel).
    pub fn new(
        session_url: Option<&str>,
        remote: IpAddr,
        rikey: &[u8; 16],
        app_version: AppVersion,
        high_quality_audio: bool,
    ) -> RtspClient {
        let port = rtsp_port_from_url(session_url);
        let encrypted = session_url.is_some_and(|url| url.contains("rtspenc://"));
        let (host_header, target_url) = if high_quality_audio {
            match session_url.and_then(|url| url_host(url).map(|host| (host, url.to_string()))) {
                Some(pair) => pair,
                None => {
                    let host = url_safe_address(&remote);
                    let target = format!("rtsp://{host}:{port}");
                    (host, target)
                }
            }
        } else {
            ("0.0.0.0".to_string(), format!("rtsp://0.0.0.0:{port}"))
        };
        RtspClient {
            addr: SocketAddr::new(remote, port),
            target_url,
            host_header,
            key: encrypted.then_some(*rikey),
            cseq: 1,
            encryption_seq: 0,
            last_host_seq: 0,
            session_id: None,
            app_version,
        }
    }

    pub fn is_encrypted(&self) -> bool {
        self.key.is_some()
    }

    pub fn address(&self) -> SocketAddr {
        self.addr
    }

    /// RtspConnection.c transactRtspMessageTcp: connect (again for 10 s while
    /// the host refuses: it may not listen yet right after the launch), send,
    /// read until the host closes.
    async fn transact(
        &mut self,
        method: &str,
        target: &str,
        extra: Vec<(&str, String)>,
        payload: &[u8],
    ) -> Result<RtspResponse, Error> {
        // initializeRtspRequest: CSeq, X-GS-ClientVersion (14 for generation 7), Host.
        let mut headers: Vec<(&str, String)> = vec![
            ("CSeq", self.cseq.to_string()),
            ("X-GS-ClientVersion", "14".to_string()),
            ("Host", self.host_header.clone()),
        ];
        self.cseq += 1;
        headers.extend(extra);
        let plain = serialize_request(method, target, &headers, payload);
        let message = match &self.key {
            Some(key) => {
                self.encryption_seq += 1;
                seal(key, self.encryption_seq, false, &plain)
            }
            None => plain,
        };

        let mut retries = 0u32;
        let mut stream = loop {
            match tokio::time::timeout(RTSP_CONNECT_TIMEOUT, TcpStream::connect(self.addr)).await {
                Ok(Ok(stream)) => break stream,
                Ok(Err(e)) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                    retries += 1;
                    if retries > (RTSP_CONNECT_TIMEOUT.as_millis() / RTSP_RETRY_DELAY.as_millis()) as u32 {
                        return Err(Error::Unreachable);
                    }
                    tokio::time::sleep(RTSP_RETRY_DELAY).await;
                }
                Ok(Err(_)) => return Err(Error::Unreachable),
                Err(_) => return Err(Error::Timeout),
            }
        };
        let _ = stream.set_nodelay(true);
        // One time limit for the whole exchange on this connection, not one
        // per read: a host (or whoever answers in its place) that sends a
        // byte now and then does not hold the connecting up for ever.
        let deadline = tokio::time::Instant::now() + RTSP_RECEIVE_TIMEOUT;
        match tokio::time::timeout_at(deadline, stream.write_all(&message)).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => return Err(Error::ConnectionLost),
            Err(_) => return Err(Error::Timeout),
        }

        let mut raw = Vec::new();
        let mut chunk = [0u8; 16384];
        loop {
            let n = match tokio::time::timeout_at(deadline, stream.read(&mut chunk)).await {
                Ok(Ok(n)) => n,
                // A host that resets instead of closing after its answer.
                Ok(Err(_)) if !raw.is_empty() => 0,
                Ok(Err(_)) => return Err(Error::ConnectionLost),
                Err(_) => return Err(Error::Timeout),
            };
            if n == 0 {
                break;
            }
            raw.extend_from_slice(&chunk[..n]);
            if raw.len() > MAX_RTSP_RESPONSE_SIZE {
                return Err(Error::Protocol("RTSP answer too large"));
            }
        }
        if raw.is_empty() {
            // unsealRtspMessage: "the server just closed the connection
            // without responding" (Sunshine: no launch is waiting for RTSP).
            return Err(Error::ConnectionLost);
        }
        let plain = match &self.key {
            Some(key) => {
                let (seq, plain) = unseal(key, true, &raw)?;
                // unsealRtspMessage gives the number to PltDecryptMessage,
                // which refuses one it has seen (PlatformCrypto.c
                // isReplayedMessage). The host counts up by one per answer
                // (rtsp.cpp respond), so an answer must be newer than the last.
                if seq <= self.last_host_seq {
                    return Err(Error::Protocol("RTSP answer repeats an old one"));
                }
                self.last_host_seq = seq;
                plain
            }
            None => raw,
        };
        let response = parse_response(&plain)?;
        if response.status != 200 {
            log::info!("sunshine: RTSP {method} was answered with status {}", response.status);
            return Err(match response.status {
                // rtsp.cpp cmd_announce: "Rejecting client that cannot comply with mandatory encryption requirement"
                403 => Error::EncryptionRequired,
                code => Error::Host { code, message: format!("RTSP {method}: {}", response.status_text) },
            });
        }
        Ok(response)
    }

    /// requestOptions
    pub async fn options(&mut self) -> Result<(), Error> {
        let target = self.target_url.clone();
        self.transact("OPTIONS", &target, Vec::new(), &[]).await.map(|_| ())
    }

    /// requestDescribe and what performRtspHandshake reads from the answer.
    pub async fn describe(&mut self) -> Result<Describe, Error> {
        let target = self.target_url.clone();
        let response = self
            .transact(
                "DESCRIBE",
                &target,
                vec![
                    ("Accept", "application/sdp".to_string()),
                    ("If-Modified-Since", "Thu, 01 Jan 1970 00:00:00 GMT".to_string()),
                ],
                &[],
            )
            .await?;
        if response.payload.is_empty() {
            return Err(Error::Protocol("RTSP DESCRIBE without content"));
        }
        Ok(parse_describe(&String::from_utf8_lossy(&response.payload)))
    }

    /// setupStream
    async fn setup(&mut self, target: &str) -> Result<SetupAnswer, Error> {
        let mut headers = Vec::new();
        if let Some(id) = &self.session_id {
            headers.push(("Session", id.clone()));
        }
        headers.push(("Transport", "unicast;X-GS-ClientPort=50000-50001".to_string()));
        headers.push(("If-Modified-Since", "Thu, 01 Jan 1970 00:00:00 GMT".to_string()));
        let response = self.transact("SETUP", target, headers, &[]).await?;
        if self.session_id.is_none() {
            // "get the first token of the session until ';'" ("DEADBEEFCAFE;timeout = 90")
            let id = response.header("Session").and_then(|s| s.split(';').next()).map(str::trim).unwrap_or("");
            if id.is_empty() {
                return Err(Error::Protocol("RTSP SETUP without a session"));
            }
            self.session_id = Some(id.to_string());
        }
        // "Parse the Sunshine ping payload protocol extension if present": only a value of exactly 16 characters counts.
        let ping_payload = response.header("X-SS-Ping-Payload").and_then(|p| <[u8; 16]>::try_from(p.as_bytes()).ok());
        // strtoul(connectData, NULL, 0)
        let connect_data = response.header("X-SS-Connect-Data").map(c_strtoul).unwrap_or(0);
        Ok(SetupAnswer { port: server_port(&response), ping_payload, connect_data })
    }

    /// SETUP streamid=audio/0/0: (port, ping payload).
    pub async fn setup_audio(&mut self) -> Result<(u16, Option<[u8; 16]>), Error> {
        let answer = self.setup("streamid=audio/0/0").await?;
        Ok((answer.port.unwrap_or(DEFAULT_AUDIO_PORT), answer.ping_payload))
    }

    /// SETUP streamid=video/0/0: (port, ping payload).
    pub async fn setup_video(&mut self) -> Result<(u16, Option<[u8; 16]>), Error> {
        let answer = self.setup("streamid=video/0/0").await?;
        Ok((answer.port.unwrap_or(DEFAULT_VIDEO_PORT), answer.ping_payload))
    }

    /// controlStreamId: `streamid=control/13/0` from 7.1.431 on.
    fn control_stream_id(&self) -> &'static str {
        if self.app_version.at_least(7, 1, 431) {
            "streamid=control/13/0"
        } else {
            "streamid=control/1/0"
        }
    }

    /// SETUP of the control stream: (port, connect data).
    pub async fn setup_control(&mut self) -> Result<(u16, u32), Error> {
        let answer = self.setup(self.control_stream_id()).await?;
        Ok((answer.port.unwrap_or(DEFAULT_CONTROL_PORT), answer.connect_data))
    }

    /// sendVideoAnnounce: the SDP of [`build_sdp`]. This is the request that
    /// makes a Sunshine host start the session (rtsp.cpp cmd_announce): from
    /// here on it waits for the UDP pings and the control connection.
    pub async fn announce(&mut self, sdp: &str) -> Result<(), Error> {
        let session = self.session_id.clone().ok_or(Error::Internal("RTSP ANNOUNCE before SETUP"))?;
        let target = if self.app_version.at_least(7, 1, 431) { self.control_stream_id() } else { "streamid=video" };
        self.transact(
            "ANNOUNCE",
            target,
            vec![
                ("Session", session),
                ("Content-type", "application/sdp".to_string()),
                ("Content-length", sdp.len().to_string()),
            ],
            sdp.as_bytes(),
        )
        .await
        .map(|_| ())
    }

    /// playStream: one PLAY of "/" ("GFE 3.22 uses a single PLAY message").
    pub async fn play(&mut self) -> Result<(), Error> {
        let session = self.session_id.clone().ok_or(Error::Internal("RTSP PLAY before SETUP"))?;
        self.transact("PLAY", "/", vec![("Session", session)], &[]).await.map(|_| ())
    }
}

#[cfg(test)]
#[path = "rtsp_tests.rs"]
mod tests;
