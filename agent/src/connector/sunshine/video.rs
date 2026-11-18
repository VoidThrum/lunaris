//! The receiving side of the GameStream video stream: one UDP datagram in,
//! complete frames out. No sockets, no threads, no clock of its own.
//!
//! Ported from moonlight-common-c (GPLv3): `VideoStream.c`
//! (`VideoReceiveThreadProc`: decryption), `RtpVideoQueue.c` (the FEC block
//! queue) and `VideoDepacketizer.c` (packets to one frame). The host's side
//! was read in Sunshine `src/stream.cpp` (`videoBroadcastThread`,
//! `fec::encode`), tag v2026.914 and master: no difference on the wire.
//!
//! How the caller drives it:
//!
//! ```text
//! let mut rx = VideoReceiver::new(VideoParams { codec, packet_size, app_version, encryption })?;
//! loop {
//!     let n = socket.recv(&mut buf);              // buf >= rx.max_datagram_size()
//!     for out in rx.push(&buf[..n], Instant::now()) {
//!         match out {
//!             VideoOut::Frame { .. } => hand to the pipeline,
//!             VideoOut::NeedIdr { .. } => control stream: ask the host for an IDR frame,
//!             VideoOut::FecStatus(s) => optional: control stream message 0x5502 (s.to_bytes()),
//!         }
//!     }
//! }
//! ```
//!
//! - `packet_size` is the value sent as `x-nv-video[0].packetSize` in the
//!   ANNOUNCE (SdpGenerator.c: with video encryption the client subtracts
//!   the 32 bytes of the encryption header before sending it). The host may
//!   use a smaller size (Sunshine's `packetsize` limit, rtsp.cpp): harmless,
//!   every packet is then shorter than the maximum.
//! - `app_version` is the `appversion` of serverinfo (Sunshine: `7.1.431.-1`).
//! - `encryption` is `Some` when bit `SS_ENC_VIDEO` (0x02) was sent in
//!   `x-ss-general.encryptionEnabled`; the key is the `rikey` of the launch.
//! - The FEC percentage is not a parameter: each packet carries it.
//!
//! Output frames: H.264 and H.265 as Annex-B, one access unit, key frames
//! with the parameter sets in front as the host sent them (access unit
//! delimiters and SEI in front of a frame are removed, as the reference
//! does). AV1: the bytes of the host encoder's packet unchanged, cut to the
//! exact length: the AV1 low-overhead bitstream format (OBUs with size
//! fields, AV1 specification section 5), one temporal unit, a sequence
//! header OBU in key frames. Nothing of AV1 is parsed here.
//!
//! Not done (decision 1 of the stage): reference frame invalidation and the
//! speculative loss prediction of RtpVideoQueue.c (`reportedLostFrame`,
//! `receivedOosData`): every loss is answered with an IDR request, as the
//! reference does when the decoder cannot invalidate reference frames.

use std::time::Instant;

use aes_gcm::{AeadInOut, Aes128Gcm, KeyInit};
use bytes::Bytes;

use super::fec::ReedSolomon;

// Video.h
const FLAG_CONTAINS_PIC_DATA: u8 = 0x1;
const FLAG_EOF: u8 = 0x2;
const FLAG_SOF: u8 = 0x4;
const NV_VIDEO_PACKET_EXTRA_FLAG_LTR_FRAME: u8 = 0x1;
const FLAG_EXTENSION: u8 = 0x10;
const FIXED_RTP_HEADER_SIZE: usize = 12;
const MAX_RTP_HEADER_SIZE: usize = 16;
/// Video.h: `sizeof(NV_VIDEO_PACKET)`.
pub const NV_VIDEO_PACKET_SIZE: usize = 16;
/// Video.h: `sizeof(ENC_VIDEO_HEADER)`: iv[12], frameNumber (LE u32), tag[16].
pub const ENC_VIDEO_HEADER_SIZE: usize = 32;
/// Video.h: `SS_FRAME_FEC_PTYPE`, the control message that carries
/// [`FrameFecStatus::to_bytes`].
pub const SS_FRAME_FEC_PTYPE: u16 = 0x5502;

// VideoDepacketizer.c
const MAX_VIDEO_REASSEMBLED_FRAME_SIZE: usize = 32 * 1024 * 1024;
const MAX_VIDEO_REASSEMBLED_FRAGMENTS: u32 = 65536;
const CONSECUTIVE_DROP_LIMIT: u32 = 120;

// Limelight-internal.h: isBefore16 / isBefore24 / isBefore32
fn before16(x: u16, y: u16) -> bool {
    x.wrapping_sub(y) > u16::MAX / 2
}
fn before24(x: u32, y: u32) -> bool {
    (x.wrapping_sub(y) & 0xFF_FFFF) > 0xFF_FFFF / 2
}
fn before32(x: u32, y: u32) -> bool {
    x.wrapping_sub(y) > u32::MAX / 2
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    H264,
    H265,
    Av1,
}

impl VideoCodec {
    /// The codecs whose bitstream the depacketizer reads at the NAL level
    /// (VideoDepacketizer.c: `VIDEO_FORMAT_MASK_H264 | VIDEO_FORMAT_MASK_H265`).
    fn is_nal(self) -> bool {
        !matches!(self, VideoCodec::Av1)
    }
}

/// The host's `appversion` (serverinfo), e.g. `7.1.431.-1` for Sunshine.
/// Limelight-internal.h: `AppVersionQuad`, `APP_VERSION_AT_LEAST`, `IS_SUNSHINE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppVersion(pub [i32; 4]);

impl AppVersion {
    /// What Sunshine (v2026.914 and master) reports.
    pub const SUNSHINE: AppVersion = AppVersion([7, 1, 431, -1]);

    pub fn parse(s: &str) -> Option<Self> {
        let mut quad = [0i32; 4];
        let mut parts = s.trim().split('.');
        for q in quad.iter_mut() {
            *q = parts.next()?.trim().parse().ok()?;
        }
        Some(Self(quad))
    }

    pub fn at_least(&self, a: i32, b: i32, c: i32) -> bool {
        let q = &self.0;
        q[0] > a || (q[0] == a && q[1] > b) || (q[0] == a && q[1] == b && q[2] >= c)
    }

    pub fn is_sunshine(&self) -> bool {
        self.0[3] < 0
    }
}

/// Key material of the encrypted video stream (Sunshine's extension,
/// `SS_ENC_VIDEO`): AES-128-GCM with the launch's `rikey`. Every packet
/// brings its own 12-byte IV and tag (stream.cpp: the host forms the IV from
/// a 64-bit little-endian packet counter and the byte 'V' at index 11; the
/// client only uses it as sent), no associated data.
#[derive(Clone)]
pub struct VideoEncryption {
    pub key: [u8; 16],
}

#[derive(Clone)]
pub struct VideoParams {
    pub codec: VideoCodec,
    /// `x-nv-video[0].packetSize` as sent to the host.
    pub packet_size: usize,
    pub app_version: AppVersion,
    pub encryption: Option<VideoEncryption>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdrReason {
    /// A frame (or more) was lost and could not be recovered; asked for at
    /// the first frame that arrives complete afterwards, so that the request
    /// is not sent into a network that is still losing packets.
    FrameLost,
    /// 120 frames in a row had to be dropped while waiting for a key frame.
    ConsecutiveDrops,
    /// A frame did not pass the depacketizer's checks.
    CorruptFrame,
    /// [`VideoReceiver::decoder_needs_idr`] was called.
    Decoder,
}

/// The state of a FEC block that needed recovery or was given up, as the
/// reference reports it to a Sunshine host (RtpVideoQueue.c:
/// `reportFinalFrameFecStatus`, Video.h: `SS_FRAME_FEC_STATUS`). Only for
/// the host's log; sending it is optional (and only right when the ANNOUNCE
/// declared `ML_FF_FEC_STATUS`, bit 0x01 of `x-ml-general.featureFlags`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameFecStatus {
    pub frame_index: u32,
    pub highest_received_sequence_number: u16,
    pub next_contiguous_sequence_number: u16,
    pub missing_packets_before_highest_received: u16,
    pub total_data_packets: u16,
    pub total_parity_packets: u16,
    pub received_data_packets: u16,
    pub received_parity_packets: u16,
    pub fec_percentage: u8,
    pub multi_fec_block_index: u8,
    pub multi_fec_block_count: u8,
}

impl FrameFecStatus {
    /// The payload of control message [`SS_FRAME_FEC_PTYPE`] (big-endian).
    pub fn to_bytes(&self) -> [u8; 21] {
        let mut b = [0u8; 21];
        b[0..4].copy_from_slice(&self.frame_index.to_be_bytes());
        b[4..6].copy_from_slice(&self.highest_received_sequence_number.to_be_bytes());
        b[6..8].copy_from_slice(&self.next_contiguous_sequence_number.to_be_bytes());
        b[8..10].copy_from_slice(&self.missing_packets_before_highest_received.to_be_bytes());
        b[10..12].copy_from_slice(&self.total_data_packets.to_be_bytes());
        b[12..14].copy_from_slice(&self.total_parity_packets.to_be_bytes());
        b[14..16].copy_from_slice(&self.received_data_packets.to_be_bytes());
        b[16..18].copy_from_slice(&self.received_parity_packets.to_be_bytes());
        b[18] = self.fec_percentage;
        b[19] = self.multi_fec_block_index;
        b[20] = self.multi_fec_block_count;
        b
    }
}

#[derive(Debug, Clone)]
pub enum VideoOut {
    Frame {
        /// The host's frame index (starts at 1).
        frame_number: u32,
        key: bool,
        /// RTP timestamp of the frame, 90 kHz, from the host's capture time
        /// (stream.cpp: relative to the start of the video thread).
        rtp_timestamp: u32,
        /// The same in microseconds; when the host sends no timestamp
        /// (0), the time since the first frame arrived instead.
        presentation_time_us: u64,
        /// Sunshine: capture to send in 1/10 ms, 0 when not known.
        host_processing_latency: u16,
        /// When the first packet of the frame's first FEC block arrived.
        receive_time: Instant,
        /// The frame carried `NV_VIDEO_PACKET_EXTRA_FLAG_LTR_FRAME`.
        long_term_reference: bool,
        data: Bytes,
    },
    /// The caller asks the host for an IDR frame (control stream). Frames are
    /// dropped here until a key frame arrives. May repeat; the caller should
    /// not send requests more often than it finds reasonable.
    NeedIdr { reason: IdrReason },
    FecStatus(FrameFecStatus),
}

/// Counters for a log line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VideoStats {
    /// Datagrams given to `push`.
    pub datagrams: u64,
    /// Too short or too long to be a video packet.
    pub invalid: u64,
    /// Encrypted stream: packets of frames already finished, dropped before decryption.
    pub old_before_decrypt: u64,
    /// Encrypted stream: authentication failed.
    pub decrypt_failed: u64,
    /// Packets the queue did not take (old frame or block, duplicate, out of range).
    pub rejected: u64,
    /// Of those: packets of a frame that was already handed out or given
    /// up. The ordinary case is parity that was not needed any more: every
    /// small frame brings two parity packets behind its data.
    pub rejected_finished_frame: u64,
    /// Of those: packets of a FEC block of the current frame that is done.
    pub rejected_finished_block: u64,
    /// Packets taken that arrived after one with a higher sequence number.
    pub out_of_order: u64,
    /// Data / parity packets taken into FEC blocks.
    pub data_packets: u64,
    pub parity_packets: u64,
    /// Data packets rebuilt by Reed-Solomon.
    pub recovered_packets: u64,
    /// FEC blocks completed with the help of parity.
    pub recovered_blocks: u64,
    /// FEC blocks given up with too few packets.
    pub unrecoverable_blocks: u64,
    /// Recovery that produced a packet failing the sanity checks.
    pub recovery_corrupt: u64,
    /// Frames handed out / of those key frames.
    pub frames: u64,
    pub key_frames: u64,
    /// Frames the stream skipped (never completed) as seen by the depacketizer.
    pub frames_lost: u64,
    /// Complete frames dropped while waiting for a key frame.
    pub frames_dropped_waiting_idr: u64,
    pub frames_corrupt: u64,
    pub idr_requests: u64,
}

/// The fixed RTP header (Video.h: `RTP_PACKET`), fields in host order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtpHeader {
    pub header: u8,
    pub packet_type: u8,
    pub sequence_number: u16,
    pub timestamp: u32,
    pub ssrc: u32,
}

impl RtpHeader {
    pub const SIZE: usize = FIXED_RTP_HEADER_SIZE;

    pub fn parse(b: &[u8]) -> Option<Self> {
        if b.len() < Self::SIZE {
            return None;
        }
        Some(Self {
            header: b[0],
            packet_type: b[1],
            sequence_number: u16::from_be_bytes([b[2], b[3]]),
            timestamp: u32::from_be_bytes([b[4], b[5], b[6], b[7]]),
            ssrc: u32::from_be_bytes([b[8], b[9], b[10], b[11]]),
        })
    }

    /// Where the NV_VIDEO_PACKET starts (RtpVideoQueue.c: `dataOffset`).
    pub fn video_data_offset(&self) -> usize {
        if self.header & FLAG_EXTENSION != 0 {
            FIXED_RTP_HEADER_SIZE + 4
        } else {
            FIXED_RTP_HEADER_SIZE
        }
    }
}

/// Video.h: `NV_VIDEO_PACKET` (little-endian on the wire).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NvVideoPacket {
    pub stream_packet_index: u32,
    pub frame_index: u32,
    pub flags: u8,
    pub extra_flags: u8,
    pub multi_fec_flags: u8,
    pub multi_fec_blocks: u8,
    pub fec_info: u32,
}

impl NvVideoPacket {
    pub fn parse(b: &[u8]) -> Option<Self> {
        if b.len() < NV_VIDEO_PACKET_SIZE {
            return None;
        }
        Some(Self {
            stream_packet_index: u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            frame_index: u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
            flags: b[8],
            extra_flags: b[9],
            multi_fec_flags: b[10],
            multi_fec_blocks: b[11],
            fec_info: u32::from_le_bytes([b[12], b[13], b[14], b[15]]),
        })
    }

    /// Index of the packet in its FEC block (data shards first, then parity).
    pub fn fec_index(&self) -> u32 {
        (self.fec_info & 0x3FF000) >> 12
    }
    /// Data shards of the FEC block.
    pub fn fec_data_packets(&self) -> u32 {
        (self.fec_info & 0xFFC0_0000) >> 22
    }
    /// Parity as a percentage of the data shards (rounded up).
    pub fn fec_percentage(&self) -> u32 {
        (self.fec_info & 0xFF0) >> 4
    }
    pub fn fec_current_block(&self) -> u8 {
        (self.multi_fec_blocks >> 4) & 0x3
    }
    pub fn fec_last_block(&self) -> u8 {
        (self.multi_fec_blocks >> 6) & 0x3
    }
}

/// RtpVideoQueue.h: `RTPV_QUEUE_ENTRY` with its packet.
struct Entry {
    seq: u16,
    is_parity: bool,
    rtp_header: u8,
    rtp_timestamp: u32,
    nv: NvVideoPacket,
    /// The whole packet from the RTP header on, zero padded to the FEC
    /// block size (`packet_size + MAX_RTP_HEADER_SIZE`).
    data: Vec<u8>,
    /// Bytes of `data` that are the packet.
    length: usize,
    receive_time: Instant,
}

/// RtpVideoQueue.h: `RTP_VIDEO_QUEUE`.
struct Queue {
    pending: Vec<Entry>,
    completed: Vec<Entry>,
    buffer_first_recv_time: Option<Instant>,
    buffer_lowest_sequence_number: u16,
    buffer_highest_sequence_number: u16,
    buffer_first_parity_sequence_number: u16,
    buffer_data_packets: u32,
    buffer_parity_packets: u32,
    received_data_packets: u32,
    received_parity_packets: u32,
    received_highest_sequence_number: u16,
    fec_percentage: u32,
    next_contiguous_sequence_number: u16,
    missing_packets: u32,
    use_fast_queue_path: bool,
    current_frame_number: u32,
    multi_fec_capable: bool,
    multi_fec_current_block_number: u8,
    multi_fec_last_block_number: u8,
    /// Not in the reference: a recovery of this block produced a packet that
    /// failed the checks; do not decode it again with every further packet.
    recovery_failed: bool,
    /// Not in the reference: a FEC block was begun at least once.
    started: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BufferType {
    PicData,
    Sps,
    Pps,
    Vps,
}

/// The state of VideoDepacketizer.c.
struct Depacketizer {
    codec: VideoCodec,
    app_version: AppVersion,
    /// The frame being put together (the reference's NAL chain, here one buffer).
    chain: Vec<u8>,
    chain_head_type: Option<BufferType>,
    chain_fragment_count: u32,
    next_frame_number: u32,
    start_frame_number: u32,
    waiting_for_next_successful_frame: bool,
    waiting_for_idr_frame: bool,
    last_packet_in_stream: u32,
    decoding_frame: bool,
    frame_is_idr: bool,
    last_packet_payload_length: u16,
    synthetic_pts_base: Option<Instant>,
    frame_host_processing_latency: u16,
    first_packet_receive_time: Option<Instant>,
    first_packet_presentation_time: u64,
    first_packet_rtp_timestamp: u32,
    drop_state_pending: bool,
    consecutive_frame_drops: u32,
    last_good_frame: u32,
}

pub struct VideoReceiver {
    params: VideoParams,
    cipher: Option<Aes128Gcm>,
    q: Queue,
    d: Depacketizer,
    stats: VideoStats,
    last_seen_frame: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoParamsError {
    /// The packet size cannot hold the headers, or is beyond a datagram.
    PacketSize,
}

impl VideoReceiver {
    /// VideoStream.c: `initializeVideoStream`.
    pub fn new(params: VideoParams) -> Result<Self, VideoParamsError> {
        if params.packet_size <= NV_VIDEO_PACKET_SIZE || params.packet_size > 65000 {
            return Err(VideoParamsError::PacketSize);
        }
        let cipher = params.encryption.as_ref().map(|e| Aes128Gcm::new(&e.key.into()));
        let q = Queue {
            pending: Vec::new(),
            completed: Vec::new(),
            buffer_first_recv_time: None,
            buffer_lowest_sequence_number: 0,
            buffer_highest_sequence_number: 0,
            buffer_first_parity_sequence_number: 0,
            buffer_data_packets: 0,
            buffer_parity_packets: 0,
            received_data_packets: 0,
            received_parity_packets: 0,
            received_highest_sequence_number: 0,
            fec_percentage: 0,
            next_contiguous_sequence_number: 0,
            missing_packets: 0,
            use_fast_queue_path: false,
            // RtpVideoQueue.c: RtpvInitializeQueue
            current_frame_number: 1,
            multi_fec_capable: params.app_version.at_least(7, 1, 431),
            multi_fec_current_block_number: 0,
            multi_fec_last_block_number: 0,
            recovery_failed: false,
            started: false,
        };
        // VideoDepacketizer.c: initializeVideoDepacketizer
        let d = Depacketizer {
            codec: params.codec,
            app_version: params.app_version,
            chain: Vec::new(),
            chain_head_type: None,
            chain_fragment_count: 0,
            next_frame_number: 1,
            start_frame_number: 0,
            waiting_for_next_successful_frame: false,
            waiting_for_idr_frame: true,
            last_packet_in_stream: u32::MAX,
            decoding_frame: false,
            frame_is_idr: false,
            last_packet_payload_length: 0,
            synthetic_pts_base: None,
            frame_host_processing_latency: 0,
            first_packet_receive_time: None,
            first_packet_presentation_time: 0,
            first_packet_rtp_timestamp: 0,
            drop_state_pending: false,
            consecutive_frame_drops: 0,
            last_good_frame: 0,
        };
        Ok(Self { params, cipher, q, d, stats: VideoStats::default(), last_seen_frame: 0 })
    }

    /// The largest datagram the host sends: the receive buffer must hold it.
    /// VideoStream.c: `receiveSize`.
    pub fn max_datagram_size(&self) -> usize {
        self.receive_size() + if self.cipher.is_some() { ENC_VIDEO_HEADER_SIZE } else { 0 }
    }

    /// The FEC block size: a packet from its RTP header on.
    /// VideoStream.c: `decryptedSize`; stream.cpp: `blocksize`.
    fn receive_size(&self) -> usize {
        self.params.packet_size + MAX_RTP_HEADER_SIZE
    }

    pub fn stats(&self) -> VideoStats {
        self.stats
    }

    /// The last frame handed out (ControlStream.c: `lastGoodFrame`, set by
    /// `connectionReceivedCompleteFrame`; goes into the loss statistics of
    /// hosts older than Sunshine).
    pub fn last_good_frame(&self) -> u32 {
        self.d.last_good_frame
    }

    /// The newest frame of which a packet was seen (ControlStream.c:
    /// `lastSeenFrame`, set by `connectionSawFrame`).
    pub fn last_seen_frame(&self) -> u32 {
        self.last_seen_frame
    }

    /// The frame the FEC queue is collecting (RtpVideoQueue.c:
    /// `RtpvGetCurrentFrameNumber`).
    pub fn current_frame_number(&self) -> u32 {
        self.q.current_frame_number
    }

    /// First frame after the last one handed out: where a reference frame
    /// invalidation range would start (VideoDepacketizer.c:
    /// `startFrameNumber`). Not used while every loss asks for an IDR frame.
    pub fn invalidation_start_frame(&self) -> u32 {
        self.d.start_frame_number
    }

    /// For hosts whose control stream has no IDR request message (the
    /// unencrypted control stream of generation 7, generation 5): the range
    /// of the "invalidate reference frames" message that stands in for it
    /// (ControlStream.c: `requestIdrFrame`).
    pub fn idr_fallback_range(&self) -> (u32, u32) {
        if self.last_seen_frame < 0x20 {
            (0, self.last_seen_frame)
        } else {
            (self.last_seen_frame - 0x20, self.last_seen_frame)
        }
    }

    /// True while frames are dropped because a key frame has to come first.
    pub fn waiting_for_key_frame(&self) -> bool {
        self.d.waiting_for_idr_frame
    }

    /// The consumer of the frames can no longer use what follows (its
    /// decoder failed, pictures were thrown away): everything is dropped
    /// until the next key frame. Returns the request the caller sends.
    /// VideoDepacketizer.c: `requestDecoderRefresh`.
    pub fn decoder_needs_idr(&mut self) -> VideoOut {
        self.d.waiting_for_idr_frame = true;
        self.d.drop_state_pending = self.d.decoding_frame;
        self.stats.idr_requests += 1;
        VideoOut::NeedIdr { reason: IdrReason::Decoder }
    }

    /// One UDP datagram of the video port.
    /// VideoStream.c: `VideoReceiveThreadProc` (the part after `recvUdpSocket`).
    pub fn push(&mut self, datagram: &[u8], now: Instant) -> Vec<VideoOut> {
        let mut out = Vec::new();
        self.stats.datagrams += 1;

        let enc = if self.cipher.is_some() { ENC_VIDEO_HEADER_SIZE } else { 0 };
        // Runt packet; one longer than the host can send is not ours either
        // (the reference's receive buffer would cut it).
        if datagram.len() < FIXED_RTP_HEADER_SIZE + enc || datagram.len() > self.receive_size() + enc {
            self.stats.invalid += 1;
            return out;
        }

        // Room for the whole FEC block size at once: the queue pads every
        // packet to it (one allocation a packet instead of two).
        let mut packet = Vec::with_capacity(self.receive_size());
        if let Some(cipher) = &self.cipher {
            let iv: [u8; 12] = datagram[0..12].try_into().unwrap();
            let frame_number = u32::from_le_bytes(datagram[12..16].try_into().unwrap());
            let tag: [u8; 16] = datagram[16..32].try_into().unwrap();
            // Not trusted yet: only used to save the decryption of shards of
            // frames already finished.
            if frame_number != 0 && frame_number < self.q.current_frame_number {
                self.stats.old_before_decrypt += 1;
                return out;
            }
            packet.extend_from_slice(&datagram[ENC_VIDEO_HEADER_SIZE..]);
            if cipher.decrypt_inout_detached(&iv.into(), &[], packet.as_mut_slice().into(), &tag.into()).is_err() {
                self.stats.decrypt_failed += 1;
                return out;
            }
        } else {
            packet.extend_from_slice(datagram);
        }

        if !self.add_packet(packet, now, &mut out) {
            self.stats.rejected += 1;
        }
        out
    }

    fn fec_status(&self) -> VideoOut {
        let q = &self.q;
        VideoOut::FecStatus(FrameFecStatus {
            frame_index: q.current_frame_number,
            highest_received_sequence_number: q.received_highest_sequence_number,
            next_contiguous_sequence_number: q.next_contiguous_sequence_number,
            missing_packets_before_highest_received: q.missing_packets as u16,
            total_data_packets: q.buffer_data_packets as u16,
            total_parity_packets: q.buffer_parity_packets as u16,
            received_data_packets: q.received_data_packets as u16,
            received_parity_packets: q.received_parity_packets as u16,
            fec_percentage: q.fec_percentage as u8,
            multi_fec_block_index: q.multi_fec_current_block_number,
            multi_fec_block_count: q.multi_fec_last_block_number + 1,
        })
    }

    /// RtpVideoQueue.c: `RtpvAddPacket`. True when the packet was queued.
    fn add_packet(&mut self, mut packet: Vec<u8>, now: Instant, out: &mut Vec<VideoOut>) -> bool {
        let Some(rtp) = RtpHeader::parse(&packet) else { return false };
        let seq = rtp.sequence_number;

        // Behind the current buffer window. (Not in the reference: before the
        // first block there is no window; a receiver that starts listening
        // when the host's sequence number is past 32767 would otherwise
        // reject everything until the number wraps.)
        if self.q.started && before16(seq, self.q.next_contiguous_sequence_number) {
            return false;
        }

        let data_offset = rtp.video_data_offset();
        let length = packet.len();
        if length < data_offset + NV_VIDEO_PACKET_SIZE {
            return false;
        }
        let Some(mut nv) = NvVideoPacket::parse(&packet[data_offset..]) else { return false };

        // Hosts before multi-FEC: make the packet look like a single block.
        if !self.q.multi_fec_capable {
            nv.multi_fec_flags = 0x10;
            nv.multi_fec_blocks = 0x00;
        }

        if before32(nv.frame_index, self.q.current_frame_number) {
            // A frame behind the current one
            self.stats.rejected_finished_frame += 1;
            return false;
        }

        let fec_index = nv.fec_index();
        let fec_current_block_number = nv.fec_current_block();

        if nv.frame_index == self.q.current_frame_number && fec_current_block_number < self.q.multi_fec_current_block_number {
            // A FEC block behind the current one
            self.stats.rejected_finished_block += 1;
            return false;
        }

        // Start over when the queue is empty after a delivery, or when a
        // frame (or block) could not be finished before the next one came.
        if self.q.pending.is_empty()
            || self.q.current_frame_number != nv.frame_index
            || self.q.multi_fec_current_block_number != fec_current_block_number
        {
            if !self.q.pending.is_empty() {
                // The block in the queue is given up
                out.push(self.fec_status());
                self.stats.unrecoverable_blocks += 1;
                log::debug!(
                    "Sunshine video: unrecoverable frame {} (block {} of {}): {}+{}={} received < {} needed",
                    self.q.current_frame_number,
                    self.q.multi_fec_current_block_number + 1,
                    self.q.multi_fec_last_block_number + 1,
                    self.q.received_data_packets,
                    self.q.received_parity_packets,
                    self.q.pending.len(),
                    self.q.buffer_data_packets
                );

                // Only a block of this frame was missed, not the whole frame:
                // the rest of the frame is of no use, go on to the next one.
                if self.q.multi_fec_last_block_number != 0 && self.q.current_frame_number == nv.frame_index {
                    self.q.pending.clear();
                    self.q.completed.clear();
                    self.d.notify_frame_lost(&mut self.stats, out);
                    self.q.current_frame_number = self.q.current_frame_number.wrapping_add(1);
                    self.q.multi_fec_current_block_number = 0;
                    return false;
                }
            }

            // It must be the current block of the current frame, or block 0
            // of a new frame.
            let expected_fec_block_number =
                if self.q.current_frame_number == nv.frame_index { self.q.multi_fec_current_block_number } else { 0 };
            if fec_current_block_number != expected_fec_block_number {
                out.push(self.fec_status());
                log::debug!(
                    "Sunshine video: unrecoverable frame {}: lost FEC blocks {} to {}",
                    nv.frame_index,
                    expected_fec_block_number + 1,
                    fec_current_block_number
                );
                self.q.pending.clear();
                self.q.completed.clear();
                self.d.notify_frame_lost(&mut self.stats, out);
                // A block of this frame is gone: skip to the next frame.
                self.q.current_frame_number = nv.frame_index.wrapping_add(1);
                self.q.multi_fec_current_block_number = 0;
                return false;
            }

            // What is left of the previous FEC block
            self.q.pending.clear();

            if self.q.current_frame_number != nv.frame_index {
                // Completed blocks of the previous frame
                self.q.completed.clear();
                // Frame numbers not contiguous: the network dropped a whole
                // frame (the number was advanced after the last delivery).
                self.d.notify_frame_lost(&mut self.stats, out);
            }

            self.q.current_frame_number = nv.frame_index;

            // ControlStream.c: connectionSawFrame
            self.last_seen_frame = nv.frame_index;

            let q = &mut self.q;
            q.started = true;
            q.buffer_first_recv_time = Some(now);
            q.buffer_lowest_sequence_number = seq.wrapping_sub(fec_index as u16);
            q.next_contiguous_sequence_number = q.buffer_lowest_sequence_number;
            q.received_data_packets = 0;
            q.received_parity_packets = 0;
            q.received_highest_sequence_number = 0;
            q.missing_packets = 0;
            q.use_fast_queue_path = true;
            q.recovery_failed = false;
            q.buffer_data_packets = nv.fec_data_packets();
            q.fec_percentage = nv.fec_percentage();
            q.buffer_parity_packets = (q.buffer_data_packets * q.fec_percentage + 99) / 100;
            q.buffer_first_parity_sequence_number = q.buffer_lowest_sequence_number.wrapping_add(q.buffer_data_packets as u16);
            q.buffer_highest_sequence_number =
                q.buffer_first_parity_sequence_number.wrapping_add(q.buffer_parity_packets as u16).wrapping_sub(1);
            q.multi_fec_current_block_number = fec_current_block_number;
            q.multi_fec_last_block_number = nv.fec_last_block();
        }

        // Above the sequence numbers of this FEC block
        if before16(self.q.buffer_highest_sequence_number, seq) {
            return false;
        }

        let is_parity = !before16(seq, self.q.buffer_first_parity_sequence_number);
        packet.resize(self.receive_size(), 0);
        let entry = Entry {
            seq,
            is_parity,
            rtp_header: rtp.header,
            rtp_timestamp: rtp.timestamp,
            nv,
            data: packet,
            length,
            receive_time: now,
        };
        if !self.queue_packet(entry, false) {
            return false;
        }

        // Count of the packets missing below the highest one received
        let q = &mut self.q;
        if q.pending.len() == 1 {
            q.missing_packets += seq.wrapping_sub(q.buffer_lowest_sequence_number) as u32;
            q.received_highest_sequence_number = seq;
        } else if before16(q.received_highest_sequence_number, seq) {
            q.missing_packets += seq.wrapping_sub(q.received_highest_sequence_number).wrapping_sub(1) as u32;
            q.received_highest_sequence_number = seq;
        } else {
            // Behind the highest but accepted: a hole was filled
            q.missing_packets = q.missing_packets.saturating_sub(1);
        }

        if is_parity {
            q.received_parity_packets += 1;
            self.stats.parity_packets += 1;
        } else {
            q.received_data_packets += 1;
            self.stats.data_packets += 1;
        }

        // Fails while too few packets are there: keep waiting.
        if self.reconstruct_frame(out) {
            self.stage_complete_fec_block();

            if self.q.multi_fec_current_block_number < self.q.multi_fec_last_block_number {
                // On to the next FEC block of this frame
                self.q.multi_fec_current_block_number += 1;
            } else {
                // All FEC blocks to the depacketizer
                self.submit_completed_frame(out);
                self.q.current_frame_number = self.q.current_frame_number.wrapping_add(1);
                self.q.multi_fec_current_block_number = 0;
            }
        }

        true
    }

    /// RtpVideoQueue.c: `queuePacket`. False for a duplicate.
    fn queue_packet(&mut self, entry: Entry, is_fec_recovery: bool) -> bool {
        let q = &mut self.q;
        let mut out_of_sequence = false;
        // In order: no need to look through the list. Once a packet came out
        // of order the list is searched for the rest of the block, because
        // the next contiguous number is no longer kept up.
        if q.use_fast_queue_path && entry.seq == q.next_contiguous_sequence_number {
            q.next_contiguous_sequence_number = entry.seq.wrapping_add(1);
        } else {
            for e in &q.pending {
                if e.seq == entry.seq {
                    return false;
                } else if before16(entry.seq, e.seq) {
                    out_of_sequence = true;
                }
            }
            q.use_fast_queue_path = false;
        }
        if out_of_sequence && !is_fec_recovery {
            self.stats.out_of_order += 1;
        }
        q.pending.push(entry);
        true
    }

    /// RtpVideoQueue.c: `reconstructFrame`. True when all data packets of the
    /// block are in the pending list (received or rebuilt).
    fn reconstruct_frame(&mut self, out: &mut Vec<VideoOut>) -> bool {
        let total_packets = (self.q.buffer_data_packets + self.q.buffer_parity_packets) as usize;
        let data_packets = self.q.buffer_data_packets as usize;

        if self.q.pending.len() < data_packets {
            // Not enough to recover yet
            return false;
        }
        if self.q.received_data_packets == self.q.buffer_data_packets {
            // A full block, no need for FEC
            return true;
        }
        if self.params.app_version.0[0] < 5 || self.q.recovery_failed {
            // The reference: "FEC recovery not supported on Gen 4 servers"
            return false;
        }

        let Ok(rs) = ReedSolomon::new(data_packets, self.q.buffer_parity_packets as usize) else {
            // The FEC data of the packets is bogus
            self.q.recovery_failed = true;
            return false;
        };

        let receive_size = self.receive_size();
        let lowest = self.q.buffer_lowest_sequence_number;
        let mut shards: Vec<Vec<u8>> = vec![Vec::new(); total_packets];
        let mut marks = vec![true; total_packets];
        for e in self.q.pending.iter_mut() {
            let index = e.seq.wrapping_sub(lowest) as usize;
            if index < total_packets && marks[index] {
                // Already zero padded to the block size when it was queued
                shards[index] = std::mem::take(&mut e.data);
                marks[index] = false;
            }
        }
        for (i, s) in shards.iter_mut().enumerate() {
            if marks[i] {
                s.resize(receive_size, 0);
            }
        }

        let decoded = rs.decode(&mut shards, &marks, receive_size).is_ok();

        // The packets taken out go back to their entries
        for e in self.q.pending.iter_mut() {
            let index = e.seq.wrapping_sub(lowest) as usize;
            if index < total_packets && !marks[index] && e.data.is_empty() {
                e.data = std::mem::take(&mut shards[index]);
            }
        }
        if !decoded {
            self.q.recovery_failed = true;
            return false;
        }

        // The final state of a block that needed recovery
        out.push(self.fec_status());

        // The recovered packets get their RTP header from a received one:
        // the host computes the parity before it fills in the RTP header and
        // the FEC fields, and writes those into the parity shards as well
        // (stream.cpp: videoBroadcastThread), so these bytes do not survive.
        let (head_header, head_timestamp) = {
            let head = &self.q.pending[0];
            (head.rtp_header, head.rtp_timestamp)
        };
        let data_offset = if head_header & FLAG_EXTENSION != 0 { FIXED_RTP_HEADER_SIZE + 4 } else { FIXED_RTP_HEADER_SIZE };
        let receive_time = self.q.buffer_first_recv_time.unwrap_or_else(Instant::now);

        let mut recovered = Vec::new();
        for i in 0..data_packets {
            if !marks[i] {
                continue;
            }
            let data = std::mem::take(&mut shards[i]);
            let Some(mut nv) = NvVideoPacket::parse(&data[data_offset..]) else {
                self.q.recovery_failed = true;
                return false;
            };
            nv.frame_index = self.q.current_frame_number;
            nv.multi_fec_blocks = ((self.q.multi_fec_last_block_number << 2) | self.q.multi_fec_current_block_number) << 4;
            nv.fec_info = 0;

            // Some checks that the recovered packet is sane, so that a
            // corrupt one reaches neither the depacketizer nor the decoder.
            let corrupt = (i == 0 && nv.flags & FLAG_SOF == 0)
                || (i == data_packets - 1 && nv.flags & FLAG_EOF == 0)
                || (i > 0 && i < data_packets - 1 && nv.flags & FLAG_CONTAINS_PIC_DATA == 0)
                || (nv.flags & !(FLAG_SOF | FLAG_EOF | FLAG_CONTAINS_PIC_DATA) != 0);
            if corrupt {
                log::debug!(
                    "Sunshine video: FEC recovery returned a corrupt packet {} (frame {})",
                    lowest.wrapping_add(i as u16),
                    self.q.current_frame_number
                );
                self.stats.recovery_corrupt += 1;
                self.q.recovery_failed = true;
                return false;
            }

            recovered.push(Entry {
                seq: lowest.wrapping_add(i as u16),
                is_parity: false,
                rtp_header: head_header,
                rtp_timestamp: head_timestamp,
                nv,
                data,
                // A recovered packet may have zero padding at its end.
                length: self.params.packet_size + data_offset,
                receive_time,
            });
        }

        self.stats.recovered_blocks += 1;
        for e in recovered {
            if self.queue_packet(e, true) {
                self.stats.recovered_packets += 1;
            }
        }
        true
    }

    /// RtpVideoQueue.c: `stageCompleteFecBlock`: the data packets of the
    /// block in sequence order to the completed list, parity dropped.
    fn stage_complete_fec_block(&mut self) {
        let lowest = self.q.buffer_lowest_sequence_number;
        let first = self.q.buffer_first_recv_time;
        let mut block: Vec<Entry> = self.q.pending.drain(..).filter(|e| !e.is_parity).collect();
        block.sort_by_key(|e| e.seq.wrapping_sub(lowest));
        for mut e in block {
            // The first packet's receive time for all of them
            if let Some(t) = first {
                e.receive_time = t;
            }
            self.q.completed.push(e);
        }
    }

    /// RtpVideoQueue.c: `submitCompletedFrame` -> VideoDepacketizer.c: `queueRtpPacket`.
    fn submit_completed_frame(&mut self, out: &mut Vec<VideoOut>) {
        let completed = std::mem::take(&mut self.q.completed);
        for e in completed {
            let data_offset =
                if e.rtp_header & FLAG_EXTENSION != 0 { FIXED_RTP_HEADER_SIZE + 4 } else { FIXED_RTP_HEADER_SIZE };
            let start = data_offset + NV_VIDEO_PACKET_SIZE;
            if e.length < start || e.length > e.data.len() {
                continue;
            }
            self.d.process_rtp_payload(&e.nv, &e.data[start..e.length], e.receive_time, e.rtp_timestamp, &mut self.stats, out);
        }
    }
}

// ---- VideoDepacketizer.c ----

/// `getAnnexBStartSequence`: the length of the start code at the beginning
/// of `b` (3 or 4), only when a byte for the NAL unit type follows.
fn annex_b_start_sequence(b: &[u8]) -> Option<usize> {
    if b.len() <= 3 {
        return None;
    }
    if b[0] == 0 && b[1] == 0 {
        if b[2] == 0 {
            if b.len() > 4 && b[3] == 1 {
                return Some(4);
            }
        } else if b[2] == 1 {
            return Some(3);
        }
    }
    None
}

/// `skipToNextNalOrEnd`
fn skip_to_next_nal_or_end(b: &mut &[u8]) {
    if let Some(n) = annex_b_start_sequence(b) {
        *b = &b[n..];
    }
    while annex_b_start_sequence(b).is_none() {
        if b.is_empty() {
            return;
        }
        *b = &b[1..];
    }
}

impl Depacketizer {
    /// The NAL unit type at a start code (`H264_NAL_TYPE` / `HEVC_NAL_TYPE`).
    fn nal_type(&self, b: &[u8]) -> Option<u8> {
        let n = annex_b_start_sequence(b)?;
        Some(match self.codec {
            VideoCodec::H264 => b[n] & 0x1F,
            _ => (b[n] & 0x7E) >> 1,
        })
    }

    /// `isSeqReferenceFrameStart`: an IDR slice (H.264: 5; H.265: 16..=21).
    fn is_seq_reference_frame_start(&self, b: &[u8]) -> bool {
        match (self.codec, self.nal_type(b)) {
            (VideoCodec::H264, Some(t)) => t == 5,
            (VideoCodec::H265, Some(t)) => (16..=21).contains(&t),
            _ => false,
        }
    }

    /// `isAccessUnitDelimiter` (H.264: 9; H.265: 35)
    fn is_access_unit_delimiter(&self, b: &[u8]) -> bool {
        match (self.codec, self.nal_type(b)) {
            (VideoCodec::H264, Some(t)) => t == 9,
            (VideoCodec::H265, Some(t)) => t == 35,
            _ => false,
        }
    }

    /// `isSeiNal` (H.264: 6; H.265: 39)
    fn is_sei_nal(&self, b: &[u8]) -> bool {
        match (self.codec, self.nal_type(b)) {
            (VideoCodec::H264, Some(t)) => t == 6,
            (VideoCodec::H265, Some(t)) => t == 39,
            _ => false,
        }
    }

    /// `isPictureParameterSetNal` (H.264: 8; H.265: 34)
    fn is_picture_parameter_set_nal(&self, b: &[u8]) -> bool {
        match (self.codec, self.nal_type(b)) {
            (VideoCodec::H264, Some(t)) => t == 8,
            (VideoCodec::H265, Some(t)) => t == 34,
            _ => false,
        }
    }

    /// `isIdrFrameStart`: a key frame starts with the SPS (H.264: 7) or the
    /// VPS (H.265: 32).
    fn is_idr_frame_start(&self, b: &[u8]) -> bool {
        match (self.codec, self.nal_type(b)) {
            (VideoCodec::H264, Some(t)) => t == 7,
            (VideoCodec::H265, Some(t)) => t == 32,
            _ => false,
        }
    }

    /// `getBufferFlags`
    fn buffer_type(&self, b: &[u8]) -> BufferType {
        match (self.codec, self.nal_type(b)) {
            (VideoCodec::H264, Some(7)) => BufferType::Sps,
            (VideoCodec::H264, Some(8)) => BufferType::Pps,
            (VideoCodec::H265, Some(33)) => BufferType::Sps,
            (VideoCodec::H265, Some(34)) => BufferType::Pps,
            (VideoCodec::H265, Some(32)) => BufferType::Vps,
            _ => BufferType::PicData,
        }
    }

    /// `cleanupFrameState`
    fn cleanup_frame_state(&mut self) {
        self.chain = Vec::new();
        self.chain_head_type = None;
        self.chain_fragment_count = 0;
    }

    /// `dropFrameState`, for a client without reference frame invalidation
    /// (`strictIdrFrameWait`): an IDR frame is needed from here on.
    fn drop_frame_state(&mut self, stats: &mut VideoStats, out: &mut Vec<VideoOut>) {
        self.drop_state_pending = false;
        self.waiting_for_idr_frame = true;

        self.consecutive_frame_drops += 1;
        if self.consecutive_frame_drops == CONSECUTIVE_DROP_LIMIT {
            self.consecutive_frame_drops = 0;
            self.waiting_for_idr_frame = true;
            stats.idr_requests += 1;
            out.push(VideoOut::NeedIdr { reason: IdrReason::ConsecutiveDrops });
        }

        self.cleanup_frame_state();
    }

    /// `notifyFrameLost` (called by the FEC queue). Without reference frame
    /// invalidation it only drops the state; the IDR request follows at the
    /// first frame that arrives complete (`waitingForNextSuccessfulFrame`).
    fn notify_frame_lost(&mut self, stats: &mut VideoStats, out: &mut Vec<VideoOut>) {
        self.drop_frame_state(stats, out);
    }

    /// `queueFragment`. False when the frame grows beyond the limits.
    fn queue_fragment(&mut self, data: &[u8]) -> bool {
        if data.is_empty() {
            return true;
        }
        // canAppendVideoFrameFragment
        if self.chain.len() > MAX_VIDEO_REASSEMBLED_FRAME_SIZE
            || data.len() > MAX_VIDEO_REASSEMBLED_FRAME_SIZE - self.chain.len()
            || self.chain_fragment_count >= MAX_VIDEO_REASSEMBLED_FRAGMENTS
        {
            return false;
        }
        if self.chain_head_type.is_none() {
            self.chain_head_type = Some(if self.codec.is_nal() { self.buffer_type(data) } else { BufferType::PicData });
        }
        self.chain.extend_from_slice(data);
        self.chain_fragment_count += 1;
        true
    }

    /// `processAvcHevcRtpPayloadSlow`: the first packet of a key frame, with
    /// several NAL units (and possibly padding between them).
    fn process_avc_hevc_rtp_payload_slow(&mut self, mut cur: &[u8]) -> bool {
        while !cur.is_empty() {
            // Padding bytes
            if annex_b_start_sequence(cur).is_none() {
                skip_to_next_nal_or_end(&mut cur);
            }
            // AUD or SEI NAL units in front
            while self.is_access_unit_delimiter(cur) || self.is_sei_nal(cur) {
                skip_to_next_nal_or_end(&mut cur);
            }

            let start = cur;
            let mut contains_pic_data = false;
            if self.is_seq_reference_frame_start(cur) {
                // No longer waiting for an IDR frame
                self.waiting_for_idr_frame = false;
                // Cancel any pending IDR frame request
                self.waiting_for_next_successful_frame = false;
                contains_pic_data = true;
                self.frame_is_idr = true;
            }

            skip_to_next_nal_or_end(&mut cur);

            // The picture data extends to the end of the packet
            if contains_pic_data {
                while !cur.is_empty() {
                    skip_to_next_nal_or_end(&mut cur);
                }
            }

            if !self.queue_fragment(&start[..start.len() - cur.len()]) {
                return false;
            }
        }
        true
    }

    /// `isFirstPacket`
    fn is_first_packet(flags: u8, fec_block_number: u8) -> bool {
        let flags = flags & !FLAG_CONTAINS_PIC_DATA;
        (flags == (FLAG_SOF | FLAG_EOF) || flags == FLAG_SOF) && fec_block_number == 0
    }

    /// `discardCorruptFrame`
    fn discard_corrupt_frame(&mut self, frame_index: u32, stats: &mut VideoStats, out: &mut Vec<VideoOut>) {
        self.decoding_frame = false;
        self.next_frame_number = frame_index.wrapping_add(1);
        stats.frames_corrupt += 1;
        self.drop_frame_state(stats, out);
        stats.idr_requests += 1;
        out.push(VideoOut::NeedIdr { reason: IdrReason::CorruptFrame });
    }

    /// `processRtpPayloadLocked`: `payload` is what follows the
    /// NV_VIDEO_PACKET in one data packet.
    fn process_rtp_payload(
        &mut self,
        video_packet: &NvVideoPacket,
        payload: &[u8],
        receive_time: Instant,
        rtp_timestamp: u32,
        stats: &mut VideoStats,
        out: &mut Vec<VideoOut>,
    ) {
        // RtpVideoQueue.c: queuePacket (`PTS_DIVISOR`): 90 kHz to microseconds
        let presentation_time_us = (rtp_timestamp as u64 * 1000) / 90;

        // The top 8 bits of the stream packet index are not part of it
        let stream_packet_index = (video_packet.stream_packet_index >> 8) & 0xFF_FFFF;
        let mut cur = payload;

        let fec_current_block_number = video_packet.fec_current_block();
        let fec_last_block_number = video_packet.fec_last_block();
        let frame_index = video_packet.frame_index;
        let flags = video_packet.flags;
        let first_packet = Self::is_first_packet(flags, fec_current_block_number);
        let last_packet = flags & FLAG_EOF != 0 && fec_current_block_number == fec_last_block_number;

        // Packets of a frame given up as corrupt
        if before32(frame_index, self.next_frame_number) {
            return;
        }

        // The FEC queue can in rare cases hand on a corrupt frame; the stream
        // packet index not following on finds nearly all of the rest.
        let expected = self.last_packet_in_stream.wrapping_add(1) & 0xFF_FFFF;
        if before24(stream_packet_index, expected) || (flags & FLAG_SOF == 0 && stream_packet_index != expected) {
            log::debug!("Sunshine video: depacketizer detected corrupt frame {frame_index}");
            self.discard_corrupt_frame(frame_index, stats, out);
            return;
        }

        // isValidVideoFramePacket: a continuation belongs to a frame whose
        // start was seen; a start while a frame is unfinished is invalid too.
        let valid = if first_packet {
            !self.decoding_frame
        } else {
            self.decoding_frame && frame_index == self.next_frame_number
        };
        if !valid {
            log::debug!("Sunshine video: depacketizer detected invalid frame start {frame_index}");
            self.discard_corrupt_frame(frame_index, stats, out);
            return;
        }

        if first_packet {
            // Is this the next consecutive frame?
            if before32(self.next_frame_number, frame_index) {
                stats.frames_lost += frame_index.wrapping_sub(self.next_frame_number) as u64;
                log::debug!(
                    "Sunshine video: network dropped {} frame(s) ({} to {})",
                    frame_index.wrapping_sub(self.next_frame_number),
                    self.next_frame_number,
                    frame_index.wrapping_sub(1)
                );
                self.next_frame_number = frame_index;
                // Wait until the next complete frame
                self.waiting_for_next_successful_frame = true;
                self.drop_frame_state(stats, out);
            }

            self.decoding_frame = true;
            self.frame_is_idr = false;
            self.first_packet_receive_time = Some(receive_time);

            // Some versions of Sunshine send no valid timestamp: one is
            // made from the receive time.
            let base = *self.synthetic_pts_base.get_or_insert(receive_time);
            self.first_packet_presentation_time = if presentation_time_us == 0 && frame_index > 0 {
                receive_time.saturating_duration_since(base).as_micros() as u64
            } else {
                presentation_time_us
            };
            self.first_packet_rtp_timestamp = rtp_timestamp;
        }

        self.last_packet_in_stream = stream_packet_index;

        // The first packet starts with the frame header (if the host has one)
        let mut frame_header_size = 0usize;
        if first_packet && !cur.is_empty() {
            // The frame type
            if self.app_version.at_least(7, 1, 350) && cur.len() >= 4 {
                match cur[3] {
                    // IDR frame: for codecs that are not parsed at the NAL
                    // level the frame header is trusted.
                    2 if !self.codec.is_nal() => {
                        self.waiting_for_idr_frame = false;
                        self.waiting_for_next_successful_frame = false;
                        self.frame_is_idr = true;
                    }
                    // 1 P-frame, 2 IDR frame, 4 intra refresh, 5 P-frame
                    // after reference frame invalidation, 104 an old
                    // Sunshine's fixed header
                    1 | 2 | 4 | 5 | 104 => {}
                    other => log::debug!("Sunshine video: unrecognized frame type {other}"),
                }
            }

            // Sunshine: host processing latency of the frame
            if self.app_version.is_sunshine() && cur.len() >= 3 {
                self.frame_host_processing_latency = u16::from_le_bytes([cur[1], cur[2]]);
            }

            // The exact length of the last packet's payload. The reference
            // reads it for the codecs that cannot stand zero padding (AV1);
            // Sunshine fills it in for every codec (stream.cpp:
            // `video_short_frame_header_t::lastPayloadLen`), so it is read
            // for all of them from a Sunshine host and 0 (unknown) otherwise.
            // (An old Sunshine's fixed header, frame type 104, has other
            // bytes there: not trusted for H.264/H.265.)
            if cur.len() >= 6 && (!self.codec.is_nal() || (self.app_version.is_sunshine() && cur[3] != 104)) {
                self.last_packet_payload_length = u16::from_le_bytes([cur[4], cur[5]]);
            } else {
                self.last_packet_payload_length = 0;
            }

            let v = &self.app_version;
            frame_header_size = if v.at_least(7, 1, 450) {
                // 0x01: 8 bytes, 0x81: 44 bytes
                if cur[0] == 0x01 { 8 } else { 44 }
            } else if v.at_least(7, 1, 446) {
                // [7.1.446, 7.1.450): 0x01: 8 bytes, 0x81: 41 bytes
                if cur[0] == 0x01 { 8 } else { 41 }
            } else if v.at_least(7, 1, 415) {
                // [7.1.415, 7.1.446): 0x01: 8 bytes, 0x81: 24 bytes
                // (Sunshine, 7.1.431: always 0x01, `video_short_frame_header_t`)
                if cur[0] == 0x01 { 8 } else { 24 }
            } else if v.at_least(7, 1, 350) {
                8
            } else if v.at_least(7, 1, 320) {
                12
            } else if v.at_least(5, 0, 0) {
                8
            } else {
                0
            };

            if cur.len() >= frame_header_size {
                cur = &cur[frame_header_size..];
            }
        }

        // The last packet is cut to its real length.
        if last_packet {
            let last_len = self.last_packet_payload_length as usize;
            if !self.codec.is_nal() {
                // The length includes the frame header, and cannot be more
                // than what was received. If it is not valid the frame is
                // given up.
                if last_len > frame_header_size && last_len - frame_header_size <= cur.len() {
                    cur = &cur[..last_len - frame_header_size];
                } else {
                    log::debug!("Sunshine video: invalid last payload length {last_len} on frame {frame_index}");
                    self.discard_corrupt_frame(frame_index, stats, out);
                    return;
                }
            } else if last_len != 0 {
                // Not in the reference, which leaves the zero padding of the
                // last packet at the end of an H.264/H.265 frame (legal in
                // Annex-B). Here the frames go on to RTP packetizers that
                // would carry the zeros along, so they are removed when the
                // length is known and plausible, and left otherwise.
                let already = payload.len() - cur.len();
                if last_len > already && last_len <= payload.len() {
                    cur = &cur[..last_len - already];
                }
            }
        }

        if first_packet && self.codec.is_nal() {
            // The Annex-B start code must be next; if not, search for one as
            // most decoders would.
            if annex_b_start_sequence(cur).is_none() {
                skip_to_next_nal_or_end(&mut cur);
            }
            // An access unit delimiter in front is removed: the rest of this
            // code is not prepared for it.
            if self.is_access_unit_delimiter(cur) {
                skip_to_next_nal_or_end(&mut cur);
            }
            // SEI NAL units after the (optional) AUD
            while self.is_sei_nal(cur) {
                skip_to_next_nal_or_end(&mut cur);
            }
        }

        let queued = if self.codec.is_nal() {
            if first_packet && self.is_idr_frame_start(cur) {
                // Parameter sets, possibly padded: the slow path
                self.process_avc_hevc_rtp_payload_slow(cur)
            } else {
                // Intel's H.264 Media Foundation encoder puts a PPS in front
                // of each P-frame: skipped.
                if first_packet && self.is_picture_parameter_set_nal(cur) {
                    skip_to_next_nal_or_end(&mut cur);
                }
                self.queue_fragment(cur)
            }
        } else {
            // Other codecs are passed through as they are.
            self.queue_fragment(cur)
        };
        if !queued {
            log::debug!("Sunshine video: depacketizer rejected oversized frame {frame_index}");
            self.discard_corrupt_frame(frame_index, stats, out);
            return;
        }

        if last_packet {
            // On to the next frame
            self.decoding_frame = false;
            self.next_frame_number = frame_index.wrapping_add(1);

            // A complete IDR frame satisfies a refresh asked for meanwhile.
            if self.drop_state_pending && !self.chain.is_empty() && self.frame_is_idr {
                self.drop_state_pending = false;
                self.waiting_for_idr_frame = false;
            }

            if self.waiting_for_idr_frame {
                // The first frame received completely after a loss stands for
                // the network having recovered: asking for an IDR frame while
                // it is still losing packets only adds to the congestion.
                if self.waiting_for_next_successful_frame {
                    stats.idr_requests += 1;
                    out.push(VideoOut::NeedIdr { reason: IdrReason::FrameLost });
                }
                self.waiting_for_next_successful_frame = false;
                stats.frames_dropped_waiting_idr += 1;
                self.drop_frame_state(stats, out);
                return;
            }

            if self.drop_state_pending {
                stats.frames_dropped_waiting_idr += 1;
                self.drop_frame_state(stats, out);
                return;
            }

            self.reassemble_frame(frame_index, video_packet.extra_flags & NV_VIDEO_PACKET_EXTRA_FLAG_LTR_FRAME != 0, stats, out);
        }
    }

    /// `reassembleFrame`
    fn reassemble_frame(&mut self, frame_number: u32, long_term_reference: bool, stats: &mut VideoStats, out: &mut Vec<VideoOut>) {
        if self.chain.is_empty() {
            return;
        }
        // A frame that starts with parameter sets is a key frame as well.
        let key = self.chain_head_type != Some(BufferType::PicData) || self.frame_is_idr;
        let data = Bytes::from(std::mem::take(&mut self.chain));
        self.chain_head_type = None;
        self.chain_fragment_count = 0;

        // Clear frame drops and advance the invalidation window
        self.consecutive_frame_drops = 0;
        self.start_frame_number = self.next_frame_number;

        stats.frames += 1;
        stats.key_frames += key as u64;
        // ControlStream.c: connectionReceivedCompleteFrame
        self.last_good_frame = frame_number;

        out.push(VideoOut::Frame {
            frame_number,
            key,
            rtp_timestamp: self.first_packet_rtp_timestamp,
            presentation_time_us: self.first_packet_presentation_time,
            host_processing_latency: self.frame_host_processing_latency,
            receive_time: self.first_packet_receive_time.unwrap_or_else(Instant::now),
            long_term_reference,
            data,
        });
    }
}

#[cfg(test)]
#[path = "video_tests.rs"]
pub(crate) mod tests;
