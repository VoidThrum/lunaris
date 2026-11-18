//! Video tests, in process: the HOST's side of packetizing is written here
//! from Sunshine `src/stream.cpp` (`videoBroadcastThread`,
//! `concat_and_insert`, `fec::encode`; v2026.914, the same on master), with
//! the same Reed-Solomon code and the same order of steps (parity first,
//! then the RTP header and FEC fields into every shard, then encryption).

use super::*;
use aes_gcm::{AeadInOut, Aes128Gcm, KeyInit};

pub(crate) struct Lcg(pub(crate) u32);
impl Lcg {
    fn next(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(1103515245).wrapping_add(12345);
        self.0 >> 16
    }
    fn byte(&mut self) -> u8 {
        self.next() as u8
    }
    /// Bytes without zeros: no start code can appear by accident.
    fn nonzero(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.byte() | 1).collect()
    }
}

const KEY: [u8; 16] = *b"0123456789abcdef";

/// stream.cpp: the state of `session->video` and the config a client set.
/// (Also the video sender of `fake_host.rs`.)
pub(crate) struct Host {
    packet_size: usize,
    pub(crate) fec_percentage: usize,
    /// `x-nv-vqos[0].fec.minRequiredFecPackets` (Moonlight sends 2)
    min_required_fec_packets: usize,
    lowseq: u32,
    cipher: Option<Aes128Gcm>,
    gcm_iv_counter: u64,
    frame_index: u32,
    timestamp: u32,
}

/// One frame: its FEC blocks, each a list of datagrams (data shards first,
/// then parity), and how many of them are data.
pub(crate) struct Sent {
    pub(crate) blocks: Vec<(usize, Vec<Vec<u8>>)>,
}

impl Sent {
    pub(crate) fn all(&self) -> Vec<Vec<u8>> {
        self.blocks.iter().flat_map(|b| b.1.iter().cloned()).collect()
    }
}

impl Host {
    fn new(packet_size: usize, encrypted: bool) -> Self {
        Self::with_key(packet_size, encrypted.then_some(KEY), 2)
    }

    /// `key`: the session's key when the client enabled video encryption;
    /// `min_required_fec_packets`: what its ANNOUNCE asked for.
    pub(crate) fn with_key(packet_size: usize, key: Option<[u8; 16]>, min_required_fec_packets: usize) -> Self {
        Self {
            packet_size,
            fec_percentage: 20,
            min_required_fec_packets,
            lowseq: 0,
            cipher: key.map(|key| Aes128Gcm::new(&key.into())),
            gcm_iv_counter: 0,
            frame_index: 0,
            timestamp: 9000,
        }
    }

    /// stream.cpp: `videoBroadcastThread`, one loop iteration.
    pub(crate) fn send(&mut self, frame: &[u8], idr: bool) -> Sent {
        self.frame_index += 1;
        self.timestamp = self.timestamp.wrapping_add(1500);
        let frame_index = self.frame_index;

        // video_short_frame_header_t
        let mut frame_header = [0u8; 8];
        frame_header[0] = 0x01;
        frame_header[1..3].copy_from_slice(&37u16.to_le_bytes()); // frame_processing_latency
        frame_header[3] = if idr { 2 } else { 1 };
        let mut last_payload_len = (frame.len() + 8) % (self.packet_size - NV_VIDEO_PACKET_SIZE);
        if last_payload_len == 0 {
            last_payload_len = self.packet_size - NV_VIDEO_PACKET_SIZE;
        }
        frame_header[4..6].copy_from_slice(&(last_payload_len as u16).to_le_bytes());

        let mut fec_percentage = self.fec_percentage;
        let blocksize = self.packet_size + 16; // MAX_RTP_HEADER_SIZE
        const RAW: usize = 32; // sizeof(video_packet_raw_t)
        let payload_blocksize = blocksize - RAW;

        // concat_and_insert(sizeof(video_packet_raw_t), payload_blocksize, frame_header, payload)
        let mut data = frame_header.to_vec();
        data.extend_from_slice(frame);
        let mut payload = Vec::new();
        for slice in data.chunks(payload_blocksize) {
            payload.extend_from_slice(&[0u8; RAW]);
            payload.extend_from_slice(slice);
        }

        const MAX_FEC_BLOCKS: usize = 4;
        let max_data_shards_per_fec_block = (255 * 100) / (100 + fec_percentage);
        let max_data_per_fec_block = max_data_shards_per_fec_block * blocksize;
        let mut fec_blocks_needed = (payload.len() + (max_data_per_fec_block - 1)) / max_data_per_fec_block;
        if fec_blocks_needed > MAX_FEC_BLOCKS {
            // "Skipping FEC for abnormally large encoded frame"
            fec_percentage = 0;
            fec_blocks_needed = MAX_FEC_BLOCKS;
        }
        let unaligned_size = payload.len() / fec_blocks_needed;
        let aligned_size = ((unaligned_size + (blocksize - 1)) / blocksize) * blocksize;

        let mut sent = Sent { blocks: Vec::new() };
        for block_index in 0..fec_blocks_needed {
            let begin = (block_index * aligned_size).min(payload.len());
            let end = if block_index == fec_blocks_needed - 1 { payload.len() } else { (begin + aligned_size).min(payload.len()) };
            let current_payload = &mut payload[begin..end];
            let packets = (current_payload.len() + (blocksize - 1)) / blocksize;
            let multi_fec_blocks = ((block_index << 4) | ((fec_blocks_needed - 1) << 6)) as u8;

            for x in 0..packets {
                let p = &mut current_payload[x * blocksize + 16..];
                p[0..4].copy_from_slice(&((self.lowseq.wrapping_add(x as u32)) << 8).to_le_bytes()); // streamPacketIndex
                p[4..8].copy_from_slice(&frame_index.to_le_bytes());
                p[10] = 0x10; // multiFecFlags
                p[11] = multi_fec_blocks;
                p[8] = FLAG_CONTAINS_PIC_DATA;
                if x == 0 {
                    p[8] |= FLAG_SOF;
                }
                if x == packets - 1 {
                    p[8] |= FLAG_EOF;
                }
            }

            // fec::encode(current_payload, blocksize, fecPercentage, minRequiredFecPackets, prefix)
            let data_shards = packets;
            let mut parity_shards = (data_shards * fec_percentage + 99) / 100;
            let mut percentage = fec_percentage;
            if parity_shards < self.min_required_fec_packets && fec_percentage != 0 {
                parity_shards = self.min_required_fec_packets;
                percentage = (100 * parity_shards) / data_shards;
            }
            let mut shards: Vec<Vec<u8>> = current_payload
                .chunks(blocksize)
                .map(|c| {
                    let mut s = c.to_vec();
                    s.resize(blocksize, 0); // the last data shard is zero padded
                    s
                })
                .collect();
            shards.extend((0..parity_shards).map(|_| vec![0u8; blocksize]));
            if percentage != 0 {
                let rs = ReedSolomon::new(data_shards, parity_shards).unwrap();
                rs.encode(&mut shards, blocksize).unwrap();
            }

            // "set FEC info now that we know for sure what our percentage will be"
            let mut datagrams = Vec::new();
            for (x, shard) in shards.iter_mut().enumerate() {
                let fec_info = ((x << 12) | (data_shards << 22) | (percentage << 4)) as u32;
                shard[28..32].copy_from_slice(&fec_info.to_le_bytes());
                shard[0] = 0x80 | FLAG_EXTENSION;
                shard[2..4].copy_from_slice(&((self.lowseq.wrapping_add(x as u32)) as u16).to_be_bytes());
                shard[4..8].copy_from_slice(&self.timestamp.to_be_bytes());
                shard[27] = multi_fec_blocks;
                shard[20..24].copy_from_slice(&frame_index.to_le_bytes());

                if let Some(cipher) = &self.cipher {
                    let mut iv = [0u8; 12];
                    iv[..8].copy_from_slice(&self.gcm_iv_counter.to_le_bytes());
                    iv[11] = b'V';
                    self.gcm_iv_counter += 1;
                    let tag = cipher.encrypt_inout_detached(&iv.into(), &[], shard.as_mut_slice().into()).unwrap();
                    // video_packet_enc_prefix_t
                    let mut d = iv.to_vec();
                    d.extend_from_slice(&frame_index.to_le_bytes());
                    d.extend_from_slice(&tag);
                    d.extend_from_slice(shard);
                    datagrams.push(d);
                } else {
                    datagrams.push(shard.clone());
                }
            }
            self.lowseq = self.lowseq.wrapping_add(shards.len() as u32);
            sent.blocks.push((data_shards, datagrams));
        }
        sent
    }
}

fn params(codec: VideoCodec, packet_size: usize, encrypted: bool) -> VideoParams {
    VideoParams {
        codec,
        packet_size,
        app_version: AppVersion::SUNSHINE,
        encryption: encrypted.then(|| VideoEncryption { key: KEY }),
    }
}

fn receiver(codec: VideoCodec, packet_size: usize, encrypted: bool) -> VideoReceiver {
    VideoReceiver::new(params(codec, packet_size, encrypted)).unwrap()
}

fn push_all(rx: &mut VideoReceiver, datagrams: &[Vec<u8>]) -> Vec<VideoOut> {
    let now = Instant::now();
    datagrams.iter().flat_map(|d| rx.push(d, now)).collect()
}

fn frames(outs: &[VideoOut]) -> Vec<(u32, bool, Vec<u8>)> {
    outs.iter()
        .filter_map(|o| match o {
            VideoOut::Frame { frame_number, key, data, .. } => Some((*frame_number, *key, data.to_vec())),
            _ => None,
        })
        .collect()
}

fn idr_requests(outs: &[VideoOut]) -> Vec<IdrReason> {
    outs.iter()
        .filter_map(|o| match o {
            VideoOut::NeedIdr { reason } => Some(*reason),
            _ => None,
        })
        .collect()
}

/// A synthetic frame of exactly `len` bytes for the codec.
pub(crate) fn make_frame(codec: VideoCodec, key: bool, len: usize, rng: &mut Lcg) -> Vec<u8> {
    let mut f: Vec<u8> = Vec::new();
    match codec {
        VideoCodec::H264 => {
            if key {
                f.extend_from_slice(&[0, 0, 0, 1, 0x67, 0x64, 0x00, 0x28, 0xAC]);
                f.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xEE, 0x3C, 0x80]);
                f.extend_from_slice(&[0, 0, 0, 1, 0x65]);
            } else {
                f.extend_from_slice(&[0, 0, 0, 1, 0x41]);
            }
        }
        VideoCodec::H265 => {
            if key {
                f.extend_from_slice(&[0, 0, 0, 1, 0x40, 0x01, 0x0C, 0x01]);
                f.extend_from_slice(&[0, 0, 0, 1, 0x42, 0x01, 0x01, 0x01]);
                f.extend_from_slice(&[0, 0, 0, 1, 0x44, 0x01, 0xC1, 0x73]);
                f.extend_from_slice(&[0, 0, 0, 1, 0x26, 0x01]);
            } else {
                f.extend_from_slice(&[0, 0, 0, 1, 0x02, 0x01]);
            }
        }
        VideoCodec::Av1 => {
            // Not parsed: anything, zeros included (and at the end)
            let mut v: Vec<u8> = (0..len).map(|_| rng.byte()).collect();
            if len >= 3 {
                v[len - 1] = 0;
                v[len - 2] = 0;
            }
            return v;
        }
    }
    assert!(len >= f.len() + 1, "frame too small for the test's headers");
    let rest = len - f.len();
    f.extend(rng.nonzero(rest));
    f
}

const CODECS: [VideoCodec; 3] = [VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1];

#[test]
fn frames_of_many_sizes_round_trip() {
    for codec in CODECS {
        for encrypted in [false, true] {
            for packet_size in [216usize, 1024, 1392] {
                let mut host = Host::new(packet_size, encrypted);
                let mut rx = receiver(codec, packet_size, encrypted);
                let mut rng = Lcg(packet_size as u32 + encrypted as u32);
                let payload = packet_size - 16;
                let mut sizes = vec![40, 41, 64, 100, payload - 9, payload - 8, payload - 7, payload, payload + 1];
                sizes.extend([2 * payload - 8, 2 * payload - 7, 3 * payload, 10 * payload - 8, 10 * payload - 7, 25_000, 60_001]);
                for (n, &size) in sizes.iter().enumerate() {
                    let key = n % 3 == 0;
                    let frame = make_frame(codec, key, size, &mut rng);
                    let sent = host.send(&frame, key);
                    for d in sent.all() {
                        assert!(d.len() <= rx.max_datagram_size());
                    }
                    let outs = push_all(&mut rx, &sent.all());
                    let got = frames(&outs);
                    assert_eq!(got.len(), 1, "{codec:?} enc {encrypted} ps {packet_size} size {size}");
                    assert_eq!(got[0].0, host.frame_index);
                    assert_eq!(got[0].1, key, "{codec:?} size {size}");
                    assert_eq!(got[0].2, frame, "{codec:?} enc {encrypted} ps {packet_size} size {size}");
                    assert!(idr_requests(&outs).is_empty());
                }
                let st = rx.stats();
                assert_eq!(st.frames, sizes.len() as u64);
                assert_eq!(st.recovered_packets, 0);
                assert_eq!(st.frames_lost + st.frames_corrupt + st.decrypt_failed + st.invalid, 0);
                assert_eq!(rx.last_good_frame(), host.frame_index);
                assert_eq!(rx.last_seen_frame(), host.frame_index);
            }
        }
    }
}

#[test]
fn frame_fields_from_the_headers() {
    let mut host = Host::new(1024, false);
    let mut rx = receiver(VideoCodec::H264, 1024, false);
    let mut rng = Lcg(1);
    let frame = make_frame(VideoCodec::H264, true, 5000, &mut rng);
    let outs = push_all(&mut rx, &host.send(&frame, true).all());
    match &outs[0] {
        VideoOut::Frame { frame_number, key, rtp_timestamp, presentation_time_us, host_processing_latency, long_term_reference, .. } => {
            assert_eq!(*frame_number, 1);
            assert!(*key);
            assert_eq!(*rtp_timestamp, 10500);
            assert_eq!(*presentation_time_us, 10500 * 1000 / 90);
            assert_eq!(*host_processing_latency, 37);
            assert!(!*long_term_reference);
        }
        other => panic!("{other:?}"),
    }
}

/// A receiver that already has a key frame (frame 1), and the host that sent it.
fn started(codec: VideoCodec, packet_size: usize, encrypted: bool, rng: &mut Lcg) -> (Host, VideoReceiver) {
    let mut host = Host::new(packet_size, encrypted);
    let mut rx = receiver(codec, packet_size, encrypted);
    let key = make_frame(codec, true, 3000, rng);
    let outs = push_all(&mut rx, &host.send(&key, true).all());
    assert_eq!(frames(&outs).len(), 1);
    (host, rx)
}

/// All subsets of `0..n` with exactly `k` elements.
fn subsets(n: usize, k: usize) -> Vec<Vec<usize>> {
    fn go(start: usize, n: usize, k: usize, cur: &mut Vec<usize>, out: &mut Vec<Vec<usize>>) {
        if cur.len() == k {
            out.push(cur.clone());
            return;
        }
        for i in start..n {
            cur.push(i);
            go(i + 1, n, k, cur, out);
            cur.pop();
        }
    }
    let mut out = Vec::new();
    go(0, n, k, &mut Vec::new(), &mut out);
    out
}

fn without(datagrams: &[Vec<u8>], drop: &[usize]) -> Vec<Vec<u8>> {
    datagrams.iter().enumerate().filter(|(i, _)| !drop.contains(i)).map(|(_, d)| d.clone()).collect()
}

#[test]
fn loss_up_to_the_parity_count_in_every_position_is_recovered() {
    // packet size 216: 200 bytes of frame a packet; 1990 + 8 bytes = 10 data
    // packets, 20 % = 2 parity packets.
    for codec in CODECS {
        for encrypted in [false, true] {
            for size in [1990usize, 1801] {
                let mut rng = Lcg(77);
                for k in 1..=2 {
                    for drop in subsets(12, k) {
                        let (mut host, mut rx) = started(codec, 216, encrypted, &mut rng);
                        let frame = make_frame(codec, false, size, &mut rng);
                        let sent = host.send(&frame, false);
                        assert_eq!(sent.blocks.len(), 1);
                        assert_eq!(sent.blocks[0].0, 10);
                        assert_eq!(sent.blocks[0].1.len(), 12);
                        let outs = push_all(&mut rx, &without(&sent.all(), &drop));
                        let got = frames(&outs);
                        assert_eq!(got.len(), 1, "{codec:?} enc {encrypted} drop {drop:?}");
                        assert_eq!(got[0].2, frame, "{codec:?} enc {encrypted} drop {drop:?}");
                        assert!(idr_requests(&outs).is_empty());
                        let lost_data = drop.iter().filter(|&&i| i < 10).count() as u64;
                        assert_eq!(rx.stats().recovered_packets, lost_data, "drop {drop:?}");
                        assert_eq!(rx.stats().recovered_blocks, (lost_data > 0) as u64);
                        // A FEC status report goes with every recovery
                        let reports = outs.iter().filter(|o| matches!(o, VideoOut::FecStatus(_))).count();
                        assert_eq!(reports, (lost_data > 0) as usize);

                        // and the stream goes on
                        let next = make_frame(codec, false, 700, &mut rng);
                        let outs = push_all(&mut rx, &host.send(&next, false).all());
                        assert_eq!(frames(&outs), vec![(3, false, next)]);
                    }
                }
            }
        }
    }
}

#[test]
fn one_loss_more_than_parity_asks_for_idr_and_recovers_at_the_key_frame() {
    for codec in CODECS {
        let mut rng = Lcg(5);
        for drop in subsets(12, 3) {
            let (mut host, mut rx) = started(codec, 216, true, &mut rng);
            let frame = make_frame(codec, false, 1990, &mut rng);
            let sent = host.send(&frame, false);
            let outs = push_all(&mut rx, &without(&sent.all(), &drop));
            assert!(frames(&outs).is_empty(), "drop {drop:?}");
            // Nothing is asked for while the loss is not known to be over
            assert!(idr_requests(&outs).is_empty());

            // The next frame arrives complete: it cannot be shown, and the
            // IDR frame is asked for.
            let p = make_frame(codec, false, 900, &mut rng);
            let outs = push_all(&mut rx, &host.send(&p, false).all());
            assert!(frames(&outs).is_empty());
            assert_eq!(idr_requests(&outs), vec![IdrReason::FrameLost], "drop {drop:?}");
            assert!(rx.waiting_for_key_frame());

            // Further frames before the key frame: dropped, not asked again
            let p = make_frame(codec, false, 900, &mut rng);
            let outs = push_all(&mut rx, &host.send(&p, false).all());
            assert!(frames(&outs).is_empty());
            assert!(idr_requests(&outs).is_empty());

            // The key frame, and what follows it
            let k = make_frame(codec, true, 2500, &mut rng);
            let outs = push_all(&mut rx, &host.send(&k, true).all());
            assert_eq!(frames(&outs), vec![(5, true, k)]);
            assert!(!rx.waiting_for_key_frame());
            let p = make_frame(codec, false, 900, &mut rng);
            let outs = push_all(&mut rx, &host.send(&p, false).all());
            assert_eq!(frames(&outs), vec![(6, false, p)]);

            let st = rx.stats();
            assert_eq!(st.frames_lost, 1);
            assert_eq!(st.frames_dropped_waiting_idr, 2);
            assert_eq!(st.unrecoverable_blocks, 1);
            assert_eq!(st.idr_requests, 1);
        }
    }
}

#[test]
fn key_frame_right_after_a_loss_needs_no_request() {
    let mut rng = Lcg(9);
    let (mut host, mut rx) = started(VideoCodec::H264, 216, false, &mut rng);
    let lost = make_frame(VideoCodec::H264, false, 1990, &mut rng);
    let _never_sent = host.send(&lost, false);
    let k = make_frame(VideoCodec::H264, true, 2500, &mut rng);
    let outs = push_all(&mut rx, &host.send(&k, true).all());
    assert_eq!(frames(&outs), vec![(3, true, k)]);
    assert!(idr_requests(&outs).is_empty());
    assert_eq!(rx.stats().frames_lost, 1);
}

#[test]
fn reordering_and_duplicates() {
    for codec in CODECS {
        for encrypted in [false, true] {
            let mut rng = Lcg(31);
            let (mut host, mut rx) = started(codec, 216, encrypted, &mut rng);
            for round in 0..40 {
                let key = round % 7 == 3;
                let frame = make_frame(codec, key, 500 + round * 173, &mut rng);
                let mut d = host.send(&frame, key).all();
                match round % 4 {
                    0 => d.reverse(),
                    1 => {
                        // shuffle
                        for i in (1..d.len()).rev() {
                            d.swap(i, rng.next() as usize % (i + 1));
                        }
                    }
                    2 => {
                        // every packet twice, the copies later
                        let copy = d.clone();
                        d.extend(copy);
                    }
                    _ => {
                        // duplicates next to each other, one packet moved to the end
                        let mut e = Vec::new();
                        for p in &d {
                            e.push(p.clone());
                            e.push(p.clone());
                        }
                        let first = e.remove(0);
                        e.push(first);
                        d = e;
                    }
                }
                let outs = push_all(&mut rx, &d);
                let got = frames(&outs);
                assert_eq!(got.len(), 1, "{codec:?} round {round}");
                assert_eq!(got[0].1, key);
                assert_eq!(got[0].2, frame, "{codec:?} enc {encrypted} round {round}");
                assert!(idr_requests(&outs).is_empty());
            }
            assert_eq!(rx.stats().frames, 41);
            assert!(rx.stats().out_of_order > 0);
            assert!(rx.stats().rejected + rx.stats().old_before_decrypt > 0);
        }
    }
}

#[test]
fn reordering_together_with_loss() {
    let mut rng = Lcg(8);
    let (mut host, mut rx) = started(VideoCodec::H265, 216, true, &mut rng);
    for round in 0..30usize {
        let frame = make_frame(VideoCodec::H265, false, 1990, &mut rng);
        let mut d = host.send(&frame, false).all();
        d.remove(round % 12);
        d.remove((round * 5) % 11);
        for i in (1..d.len()).rev() {
            d.swap(i, rng.next() as usize % (i + 1));
        }
        let outs = push_all(&mut rx, &d);
        assert_eq!(frames(&outs), vec![(round as u32 + 2, false, frame)]);
    }
}

#[test]
fn old_frames_and_late_packets_are_rejected() {
    let mut rng = Lcg(3);
    let (mut host, mut rx) = started(VideoCodec::H264, 216, false, &mut rng);
    let f2 = make_frame(VideoCodec::H264, false, 1990, &mut rng);
    let s2 = host.send(&f2, false).all();
    let f3 = make_frame(VideoCodec::H264, false, 1990, &mut rng);
    let s3 = host.send(&f3, false).all();

    // Frame 2 without two packets (recovered), then frame 3, then all of
    // frame 2 again (late copies), then frame 3 again.
    let outs = push_all(&mut rx, &without(&s2, &[4, 5]));
    assert_eq!(frames(&outs), vec![(2, false, f2)]);
    let before = rx.stats().rejected;
    let outs = push_all(&mut rx, &s2);
    assert!(outs.is_empty());
    assert_eq!(rx.stats().rejected, before + 12);
    let outs = push_all(&mut rx, &s3[..6]);
    assert!(outs.is_empty());
    let outs = push_all(&mut rx, &s2);
    assert!(outs.is_empty());
    let outs = push_all(&mut rx, &s3[6..]);
    assert_eq!(frames(&outs), vec![(3, false, f3)]);
    let outs = push_all(&mut rx, &s3);
    assert!(outs.is_empty());
    assert_eq!(rx.stats().frames, 3);
    assert_eq!(rx.stats().frames_lost, 0);
}

#[test]
fn multi_fec_block_frames() {
    // packet size 128: FEC blocks of 144 bytes, at most 212 data shards a
    // block at 20 %: frames beyond 30528 bytes of shards take several blocks.
    for codec in CODECS {
        for encrypted in [false, true] {
            let mut rng = Lcg(41);
            let (mut host, mut rx) = started(codec, 128, encrypted, &mut rng);
            let mut expect_blocks = Vec::new();
            for (n, size) in [30_000usize, 47_000, 70_000, 94_000, 23_700, 23_800].into_iter().enumerate() {
                let key = n == 2;
                let frame = make_frame(codec, key, size, &mut rng);
                let sent = host.send(&frame, key);
                expect_blocks.push(sent.blocks.len());

                // In every block: the first data packet, one in the middle
                // and as many more as there is parity, minus two, are lost;
                // the packets of each block arrive in reverse.
                let mut datagrams = Vec::new();
                for (data, block) in &sent.blocks {
                    let parity = block.len() - data;
                    assert!(parity >= 2);
                    let mut drop = vec![0, data / 2];
                    drop.extend((0..parity - 2).map(|i| 1 + i * 3));
                    let mut kept = without(block, &drop);
                    kept.reverse();
                    datagrams.extend(kept);
                }
                let outs = push_all(&mut rx, &datagrams);
                let got = frames(&outs);
                assert_eq!(got.len(), 1, "{codec:?} enc {encrypted} size {size}");
                assert_eq!(got[0].1, key);
                assert!(got[0].2 == frame, "{codec:?} enc {encrypted} size {size}");
                assert!(idr_requests(&outs).is_empty());
            }
            assert_eq!(expect_blocks, vec![2, 2, 3, 4, 1, 2]);
        }
    }
}

#[test]
fn a_lost_block_of_a_multi_block_frame_loses_the_frame() {
    for lost_block in 0..3 {
        let mut rng = Lcg(42);
        let (mut host, mut rx) = started(VideoCodec::H264, 128, false, &mut rng);
        let frame = make_frame(VideoCodec::H264, false, 70_000, &mut rng);
        let sent = host.send(&frame, false);
        assert_eq!(sent.blocks.len(), 3);
        let mut datagrams = Vec::new();
        for (i, (data, block)) in sent.blocks.iter().enumerate() {
            if i == lost_block {
                // one packet too few
                let parity = block.len() - data;
                datagrams.extend(block[parity + 1..].iter().cloned());
            } else {
                datagrams.extend(block.iter().cloned());
            }
        }
        let outs = push_all(&mut rx, &datagrams);
        assert!(frames(&outs).is_empty(), "block {lost_block}");

        let p = make_frame(VideoCodec::H264, false, 900, &mut rng);
        let outs = push_all(&mut rx, &host.send(&p, false).all());
        assert!(frames(&outs).is_empty());
        assert_eq!(idr_requests(&outs), vec![IdrReason::FrameLost], "block {lost_block}");

        let k = make_frame(VideoCodec::H264, true, 40_000, &mut rng);
        let outs = push_all(&mut rx, &host.send(&k, true).all());
        assert_eq!(frames(&outs).len(), 1);
        assert!(frames(&outs)[0].2 == k);
        assert_eq!(rx.stats().frames_lost, 1);
    }
}

#[test]
fn a_frame_too_large_for_fec_is_sent_without_and_arrives() {
    // More than 4 FEC blocks' worth: the host turns FEC off for the frame.
    for codec in CODECS {
        let mut rng = Lcg(43);
        let (mut host, mut rx) = started(codec, 128, true, &mut rng);
        let frame = make_frame(codec, true, 130_000, &mut rng);
        let sent = host.send(&frame, true);
        assert_eq!(sent.blocks.len(), 4);
        for (data, block) in &sent.blocks {
            assert_eq!(*data, block.len(), "no parity");
            assert!(*data > 255);
        }
        let outs = push_all(&mut rx, &sent.all());
        let got = frames(&outs);
        assert_eq!(got.len(), 1);
        assert!(got[0].2 == frame);

        // and one lost packet of such a frame cannot be recovered
        let frame = make_frame(codec, false, 130_000, &mut rng);
        let mut d = host.send(&frame, false).all();
        d.remove(500);
        let outs = push_all(&mut rx, &d);
        assert!(frames(&outs).is_empty());
        let p = make_frame(codec, false, 500, &mut rng);
        let outs = push_all(&mut rx, &host.send(&p, false).all());
        assert_eq!(idr_requests(&outs), vec![IdrReason::FrameLost]);
    }
}

#[test]
fn sequence_numbers_wrap() {
    for start in [65_000u32, 0x00FF_FF00] {
        let mut rng = Lcg(6);
        let mut host = Host::new(216, false);
        let mut rx = receiver(VideoCodec::H264, 216, false);
        // The receiver takes sequence numbers from 0 on, as the host starts
        // there; bring both close to the wrap of the 16-bit sequence number
        // (and of the 24-bit stream packet index).
        let k = make_frame(VideoCodec::H264, true, 1000, &mut rng);
        assert_eq!(frames(&push_all(&mut rx, &host.send(&k, true).all())).len(), 1);
        // Walk up in steps below half the 16-bit range
        while host.lowseq + 20_000 < start {
            host.lowseq += 20_000;
            let p = make_frame(VideoCodec::H264, false, 300, &mut rng);
            assert_eq!(frames(&push_all(&mut rx, &host.send(&p, false).all())).len(), 1);
        }
        host.lowseq = start;
        for round in 0..120usize {
            let frame = make_frame(VideoCodec::H264, false, 1990, &mut rng);
            let mut d = host.send(&frame, false).all();
            d.remove(round % 12);
            if round % 2 == 0 {
                d.reverse();
            }
            let outs = push_all(&mut rx, &d);
            assert_eq!(frames(&outs).len(), 1, "start {start} round {round}");
            assert_eq!(frames(&outs)[0].2, frame);
        }
    }
}

#[test]
fn aud_and_sei_in_front_are_removed_and_parameter_sets_kept() {
    let mut rng = Lcg(2);
    // H.264: AUD, SPS, PPS, SEI, IDR slice (as x264 and others order them)
    let mut host = Host::new(1024, false);
    let mut rx = receiver(VideoCodec::H264, 1024, false);
    let sps = [0, 0, 0, 1, 0x67, 0x64, 0x00, 0x28, 0xAC];
    let pps = [0, 0, 0, 1, 0x68, 0xEE, 0x3C, 0x80];
    let mut slice = vec![0, 0, 0, 1, 0x65];
    slice.extend(rng.nonzero(4000));
    let mut sent = vec![0, 0, 0, 1, 0x09, 0x10];
    sent.extend_from_slice(&sps);
    sent.extend_from_slice(&pps);
    sent.extend_from_slice(&[0, 0, 0, 1, 0x06, 0x05, 0x04, 0x80]);
    sent.extend_from_slice(&slice);
    let expected: Vec<u8> = [&sps[..], &pps[..], &slice[..]].concat();
    let outs = push_all(&mut rx, &host.send(&sent, true).all());
    assert_eq!(frames(&outs), vec![(1, true, expected)]);

    // P frame: AUD and SEI in front, 3-byte start code on the slice
    let mut slice = vec![0, 0, 1, 0x41];
    slice.extend(rng.nonzero(2000));
    let mut sent = vec![0, 0, 0, 1, 0x09, 0x30, 0, 0, 0, 1, 0x06, 0x01, 0x80];
    sent.extend_from_slice(&slice);
    let outs = push_all(&mut rx, &host.send(&sent, false).all());
    assert_eq!(frames(&outs), vec![(2, false, slice)]);

    // H.265: AUD, VPS, SPS, PPS, prefix SEI, IDR_W_RADL
    let mut host = Host::new(1024, false);
    let mut rx = receiver(VideoCodec::H265, 1024, false);
    let vps = [0, 0, 0, 1, 0x40, 0x01, 0x0C, 0x01];
    let sps = [0, 0, 0, 1, 0x42, 0x01, 0x01, 0x01];
    let pps = [0, 0, 0, 1, 0x44, 0x01, 0xC1, 0x73];
    let mut slice = vec![0, 0, 0, 1, 0x26, 0x01];
    slice.extend(rng.nonzero(4000));
    let mut sent = vec![0, 0, 0, 1, 0x46, 0x01, 0x10];
    sent.extend_from_slice(&vps);
    sent.extend_from_slice(&sps);
    sent.extend_from_slice(&pps);
    sent.extend_from_slice(&[0, 0, 0, 1, 0x4E, 0x01, 0x05, 0x80]);
    sent.extend_from_slice(&slice);
    let expected: Vec<u8> = [&vps[..], &sps[..], &pps[..], &slice[..]].concat();
    let outs = push_all(&mut rx, &host.send(&sent, true).all());
    assert_eq!(frames(&outs), vec![(1, true, expected)]);
}

#[test]
fn a_stream_that_does_not_start_with_a_key_frame_waits() {
    for codec in CODECS {
        let mut rng = Lcg(4);
        let mut host = Host::new(216, false);
        let mut rx = receiver(codec, 216, false);
        // 119 frames dropped silently, the 120th asks (CONSECUTIVE_DROP_LIMIT)
        for n in 1..=120 {
            let p = make_frame(codec, false, 600, &mut rng);
            let outs = push_all(&mut rx, &host.send(&p, false).all());
            assert!(frames(&outs).is_empty());
            if n < 120 {
                assert!(idr_requests(&outs).is_empty(), "frame {n}");
            } else {
                assert_eq!(idr_requests(&outs), vec![IdrReason::ConsecutiveDrops]);
            }
        }
        let k = make_frame(codec, true, 600, &mut rng);
        let outs = push_all(&mut rx, &host.send(&k, true).all());
        assert_eq!(frames(&outs), vec![(121, true, k)]);
    }
}

#[test]
fn joining_a_running_stream() {
    // The first packets seen belong to frame 50.
    let mut rng = Lcg(12);
    let mut host = Host::new(216, false);
    host.lowseq = 40_000;
    for _ in 0..49 {
        let p = make_frame(VideoCodec::H264, false, 600, &mut rng);
        host.send(&p, false);
    }
    let mut rx = receiver(VideoCodec::H264, 216, false);
    let p = make_frame(VideoCodec::H264, false, 600, &mut rng);
    let outs = push_all(&mut rx, &host.send(&p, false).all());
    assert!(frames(&outs).is_empty());
    assert_eq!(idr_requests(&outs), vec![IdrReason::FrameLost]);
    assert_eq!(rx.idr_fallback_range(), (50 - 0x20, 50));
    let k = make_frame(VideoCodec::H264, true, 600, &mut rng);
    let outs = push_all(&mut rx, &host.send(&k, true).all());
    assert_eq!(frames(&outs), vec![(51, true, k)]);
}

#[test]
fn decoder_refresh_drops_until_the_key_frame() {
    let mut rng = Lcg(13);
    let (mut host, mut rx) = started(VideoCodec::Av1, 216, false, &mut rng);
    assert!(matches!(rx.decoder_needs_idr(), VideoOut::NeedIdr { reason: IdrReason::Decoder }));
    let p = make_frame(VideoCodec::Av1, false, 600, &mut rng);
    assert!(frames(&push_all(&mut rx, &host.send(&p, false).all())).is_empty());
    let k = make_frame(VideoCodec::Av1, true, 600, &mut rng);
    assert_eq!(frames(&push_all(&mut rx, &host.send(&k, true).all())), vec![(3, true, k)]);
}

#[test]
fn encrypted_packets_are_authenticated() {
    let mut rng = Lcg(14);
    let (mut host, mut rx) = started(VideoCodec::H264, 216, true, &mut rng);
    let frame = make_frame(VideoCodec::H264, false, 1990, &mut rng);
    let mut d = host.send(&frame, false).all();
    // Two packets tampered with (one in the ciphertext, one in the tag): they
    // count as lost, and the parity makes up for them.
    d[3][60] ^= 0x01;
    d[7][20] ^= 0x80;
    let outs = push_all(&mut rx, &d);
    assert_eq!(frames(&outs), vec![(2, false, frame)]);
    assert_eq!(rx.stats().decrypt_failed, 2);
    assert_eq!(rx.stats().recovered_packets, 2);

    // The wrong key: nothing gets through
    let mut other = VideoReceiver::new(VideoParams {
        codec: VideoCodec::H264,
        packet_size: 216,
        app_version: AppVersion::SUNSHINE,
        encryption: Some(VideoEncryption { key: [7; 16] }),
    })
    .unwrap();
    let mut host = Host::new(216, true);
    let k = make_frame(VideoCodec::H264, true, 1990, &mut rng);
    let d = host.send(&k, true).all();
    assert!(push_all(&mut other, &d).is_empty());
    assert_eq!(other.stats().decrypt_failed, d.len() as u64);

    // An unencrypted receiver and encrypted packets, and the reverse: no frames, no panic
    let mut plain = receiver(VideoCodec::H264, 216, false);
    assert!(frames(&push_all(&mut plain, &d)).is_empty());
    let mut host = Host::new(216, false);
    let d = host.send(&k, true).all();
    let mut enc = receiver(VideoCodec::H264, 216, true);
    assert!(push_all(&mut enc, &d).is_empty());
}

#[test]
fn garbage_does_not_panic() {
    for codec in CODECS {
        for encrypted in [false, true] {
            let mut rng = Lcg(99);
            let (mut host, mut rx) = started(codec, 216, encrypted, &mut rng);
            let now = Instant::now();
            for len in [0usize, 1, 11, 12, 16, 27, 28, 31, 32, 44, 60, 232, 264, 265, 2000] {
                let junk: Vec<u8> = (0..len).map(|_| rng.byte()).collect();
                rx.push(&junk, now);
                rx.push(&vec![0u8; len], now);
                rx.push(&vec![0xFFu8; len], now);
            }
            // Valid packets with single bytes of the headers changed
            // (unencrypted only: the encrypted ones would just fail to decrypt)
            for round in 0..300 {
                let frame = make_frame(codec, round % 9 == 0, 300 + (rng.next() as usize % 3000), &mut rng);
                let mut d = host.send(&frame, round % 9 == 0).all();
                for p in d.iter_mut() {
                    if rng.next() % 4 == 0 {
                        let at = rng.next() as usize % 44.min(p.len());
                        p[at] = rng.byte();
                    }
                    if rng.next() % 16 == 0 {
                        p.truncate(rng.next() as usize % p.len());
                    }
                }
                push_all(&mut rx, &d);
            }
        }
    }
}

#[test]
fn recovered_frames_survive_wrong_bytes_only_where_the_host_overwrites() {
    // What the recovery relies on: the host computes parity over shards whose
    // RTP header and fecInfo are still zero and overwrites those fields (and
    // frameIndex, multiFecBlocks) in the parity shards afterwards. The payload
    // must come back whole all the same.
    let mut rng = Lcg(21);
    let (mut host, mut rx) = started(VideoCodec::Av1, 1392, false, &mut rng);
    for round in 0..50usize {
        let frame = make_frame(VideoCodec::Av1, false, 40_000 + round, &mut rng);
        let sent = host.send(&frame, false);
        let (data, block) = &sent.blocks[0];
        let parity = block.len() - data;
        // lose as many data packets as there is parity, spread out
        let drop: Vec<usize> = (0..parity).map(|i| (round + i * 5) % data).collect();
        let mut drop_sorted = drop.clone();
        drop_sorted.sort();
        drop_sorted.dedup();
        let outs = push_all(&mut rx, &without(block, &drop_sorted));
        assert_eq!(frames(&outs).len(), 1, "round {round}");
        assert!(frames(&outs)[0].2 == frame, "round {round}");
    }
}

#[test]
fn app_version_and_report_helpers() {
    assert_eq!(AppVersion::parse("7.1.431.-1"), Some(AppVersion::SUNSHINE));
    assert!(AppVersion::SUNSHINE.is_sunshine());
    assert!(AppVersion::SUNSHINE.at_least(7, 1, 415));
    assert!(!AppVersion::SUNSHINE.at_least(7, 1, 446));
    assert_eq!(AppVersion::parse("7.1.450.0"), Some(AppVersion([7, 1, 450, 0])));
    assert!(!AppVersion([7, 1, 450, 0]).is_sunshine());
    assert_eq!(AppVersion::parse("7.1"), None);
    assert_eq!(AppVersion::parse("a.b.c.d"), None);

    let s = FrameFecStatus {
        frame_index: 0x01020304,
        highest_received_sequence_number: 0x0506,
        next_contiguous_sequence_number: 0x0708,
        missing_packets_before_highest_received: 0x090A,
        total_data_packets: 0x0B0C,
        total_parity_packets: 0x0D0E,
        received_data_packets: 0x0F10,
        received_parity_packets: 0x1112,
        fec_percentage: 0x13,
        multi_fec_block_index: 0x14,
        multi_fec_block_count: 0x15,
    };
    let expect: Vec<u8> = (1..=0x15).collect();
    assert_eq!(s.to_bytes().to_vec(), expect);

    assert!(VideoReceiver::new(params(VideoCodec::H264, 16, false)).is_err());
    let rx = receiver(VideoCodec::H264, 1392, true);
    assert_eq!(rx.max_datagram_size(), 1392 + 16 + 32);
    assert_eq!(rx.idr_fallback_range(), (0, 0));
}

#[test]
fn fec_status_of_a_recovered_block() {
    let mut rng = Lcg(15);
    let (mut host, mut rx) = started(VideoCodec::H264, 216, false, &mut rng);
    let first_seq = host.lowseq as u16;
    let frame = make_frame(VideoCodec::H264, false, 1990, &mut rng);
    let d = host.send(&frame, false).all();
    let outs = push_all(&mut rx, &without(&d, &[2, 6]));
    let status = outs.iter().find_map(|o| if let VideoOut::FecStatus(s) = o { Some(*s) } else { None }).unwrap();
    assert_eq!(
        status,
        FrameFecStatus {
            frame_index: 2,
            highest_received_sequence_number: first_seq + 11,
            next_contiguous_sequence_number: first_seq + 2,
            missing_packets_before_highest_received: 2,
            total_data_packets: 10,
            total_parity_packets: 2,
            received_data_packets: 8,
            received_parity_packets: 2,
            fec_percentage: 20,
            multi_fec_block_index: 0,
            multi_fec_block_count: 1,
        }
    );
}

/// Not a check: prints what the receiver costs per datagram and per frame
/// for ten seconds of a 20 Mbit/s, 60 frames/s stream. Run with
/// `--ignored --nocapture` (an optimized build says more than a debug one).
#[test]
#[ignore]
fn bench_receive_cost() {
    for (label, sent_size, encrypted, lose_every) in [
        ("plain, 1024-byte packets", 1024usize, false, 0usize),
        ("plain, 1392-byte packets", 1392, false, 0),
        ("encrypted, 992-byte packets", 992, true, 0),
        ("plain, 1024-byte packets, 1 of 50 lost", 1024, false, 50),
    ] {
        let mut rng = Lcg(11);
        let mut host = Host::new(sent_size, encrypted);
        let mut datagrams: Vec<Vec<u8>> = Vec::new();
        let mut bytes = 0usize;
        for i in 0..600 {
            let key = i % 120 == 0;
            let frame = make_frame(VideoCodec::H264, key, if key { 200_000 } else { 40_300 }, &mut rng);
            bytes += frame.len();
            datagrams.extend(host.send(&frame, key).all());
        }
        if lose_every > 0 {
            let mut n = 0usize;
            datagrams.retain(|_| {
                n += 1;
                n % lose_every != 0
            });
        }
        let mut best = std::time::Duration::MAX;
        let (mut frames_out, mut bytes_out) = (0usize, 0usize);
        for _ in 0..5 {
            let mut rx = receiver(VideoCodec::H264, sent_size, encrypted);
            (frames_out, bytes_out) = (0, 0);
            let now = Instant::now();
            let started = Instant::now();
            for d in &datagrams {
                for out in rx.push(d, now) {
                    if let VideoOut::Frame { data, .. } = out {
                        frames_out += 1;
                        bytes_out += data.len();
                    }
                }
            }
            best = best.min(started.elapsed());
        }
        println!(
            "BENCH video receive [{label}]: {} datagrams, {frames_out} of 600 frames ({bytes_out} of {bytes} bytes) in {:.2} ms = {:.0} ns a datagram, {:.1} us a frame, {:.3} % of one core at 60 frames/s",
            datagrams.len(),
            best.as_secs_f64() * 1e3,
            best.as_nanos() as f64 / datagrams.len() as f64,
            best.as_secs_f64() * 1e6 / 600.0,
            best.as_secs_f64() / 10.0 * 100.0,
        );
    }
}
