//! RTP payloading of VP9 (draft-ietf-payload-vp9), non-flexible mode, with
//! the P bit and the scalability structure taken from the frame itself.
//!
//! Ported from pion/rtp (MIT, https://github.com/pion/rtp):
//! `codecs/vp9_packet.go` `VP9Payloader.Payload` and `payloadNonFlexible`,
//! and of `codecs/vp9/header.go` (`Header.Unmarshal`, `Width`, `Height`)
//! what those need. rtc-rtp's own `Vp9Payloader` sends flexible mode without
//! the P bit, so a receiver takes every frame for a key frame.

use bytes::{BufMut, Bytes, BytesMut};
use rtc::rtp::packetizer::Payloader;

/// What the payloader needs of a VP9 frame's uncompressed header
/// (codecs/vp9/header.go `Header`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Vp9Header {
    pub non_key_frame: bool,
    /// The size, of a key frame.
    pub width: u16,
    pub height: u16,
}

struct Bits<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn read(&mut self, n: usize) -> Option<u32> {
        if self.pos + n > self.buf.len() * 8 {
            return None;
        }
        let mut value = 0u32;
        for _ in 0..n {
            let bit = (self.buf[self.pos / 8] >> (7 - self.pos % 8)) & 1;
            value = (value << 1) | bit as u32;
            self.pos += 1;
        }
        Some(value)
    }
}

impl Vp9Header {
    /// header.go `Header.Unmarshal`, as far as the frame size of a key
    /// frame. `None` for what is not a VP9 frame.
    pub fn parse(frame: &[u8]) -> Option<Vp9Header> {
        let mut bits = Bits { buf: frame, pos: 0 };
        if bits.read(2)? != 2 {
            return None; // frame_marker
        }
        let profile_low = bits.read(1)?;
        let profile_high = bits.read(1)?;
        let profile = (profile_high << 1) + profile_low;
        if profile == 3 {
            bits.read(1)?; // reserved_zero
        }
        // show_existing_frame: an earlier frame is shown again, which
        // depends on it. (pion leaves such a frame marked as a key frame;
        // here it is not one.)
        if bits.read(1)? == 1 {
            return Some(Vp9Header { non_key_frame: true, width: 0, height: 0 });
        }
        let non_key_frame = bits.read(1)? == 1;
        bits.read(2)?; // show_frame, error_resilient_mode
        if non_key_frame {
            return Some(Vp9Header { non_key_frame, width: 0, height: 0 });
        }
        // frame_sync_code
        if bits.read(8)? != 0x49 || bits.read(8)? != 0x83 || bits.read(8)? != 0x42 {
            return None;
        }
        // color_config (HeaderColorConfig.unmarshal)
        if profile >= 2 {
            bits.read(1)?; // ten_or_twelve_bit
        }
        let color_space = bits.read(3)?;
        if color_space != 7 {
            bits.read(1)?; // color_range
            if profile == 1 || profile == 3 {
                bits.read(3)?; // subsampling_x, subsampling_y, reserved_zero
            }
        } else if profile == 1 || profile == 3 {
            bits.read(1)?; // reserved_zero
        }
        // frame_size (HeaderFrameSize.unmarshal): both minus one.
        let width = bits.read(16)? as u16;
        let height = bits.read(16)? as u16;
        Some(Vp9Header { non_key_frame, width: width.wrapping_add(1), height: height.wrapping_add(1) })
    }
}

/// vp9_packet.go `VP9Payloader` with `FlexibleMode` false.
#[derive(Debug, Clone, Default)]
pub struct LunarisVp9Payloader {
    picture_id: u16,
    initialized: bool,
}

impl LunarisVp9Payloader {
    pub fn new() -> Self {
        Self::default()
    }

    /// With the picture id the first frame gets.
    #[cfg(test)]
    fn with_picture_id(picture_id: u16) -> Self {
        LunarisVp9Payloader { picture_id: picture_id & 0x7FFF, initialized: true }
    }
}

impl Payloader for LunarisVp9Payloader {
    fn payload(&mut self, mtu: usize, payload: &Bytes) -> Result<Vec<Bytes>, rtc::shared::error::Error> {
        // VP9Payloader.Payload: a random first picture id, then one more per frame.
        if !self.initialized {
            self.picture_id = (uuid::Uuid::new_v4().as_u128() & 0x7FFF) as u16;
            self.initialized = true;
        }
        let payloads = self.payload_non_flexible(mtu, payload);
        self.picture_id += 1;
        if self.picture_id >= 0x8000 {
            self.picture_id = 0;
        }
        Ok(payloads)
    }

    fn clone_to(&self) -> Box<dyn Payloader> {
        Box::new(self.clone())
    }
}

impl LunarisVp9Payloader {
    /// vp9_packet.go `payloadNonFlexible`.
    fn payload_non_flexible(&self, mtu: usize, payload: &Bytes) -> Vec<Bytes> {
        let Some(header) = Vp9Header::parse(payload) else { return Vec::new() };
        let mut remaining = payload.len();
        let mut index = 0;
        let mut payloads = Vec::new();
        while remaining > 0 {
            // The first packet of a key frame carries the scalability structure.
            let with_structure = !header.non_key_frame && index == 0;
            let header_size = if with_structure { 3 + 8 } else { 3 };
            if mtu <= header_size {
                return Vec::new();
            }
            let fragment = (mtu - header_size).min(remaining);
            let mut out = BytesMut::with_capacity(header_size + fragment);
            let mut first = 0x80 | 0x01; // I=1, Z=1
            if header.non_key_frame {
                first |= 0x40; // P=1
            }
            if index == 0 {
                first |= 0x08; // B=1
            }
            if remaining == fragment {
                first |= 0x04; // E=1
            }
            if with_structure {
                first |= 0x02; // V=1
            }
            out.put_u8(first);
            out.put_u8((self.picture_id >> 8) as u8 | 0x80);
            out.put_u8(self.picture_id as u8);
            if with_structure {
                out.put_u8(0x10 | 0x08); // N_S=0, Y=1, G=1
                out.put_u16(header.width);
                out.put_u16(header.height);
                out.put_u8(0x01); // N_G=1
                out.put_u8(1 << 4 | 1 << 2); // TID=0, U=1, R=1
                out.put_u8(0x01); // P_DIFF=1
            }
            out.extend_from_slice(&payload[index..index + fragment]);
            payloads.push(out.freeze());
            remaining -= fragment;
            index += fragment;
        }
        payloads
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The start of a profile 0 key frame of 1280 x 720: frame marker,
    /// profile 0, not show_existing, key frame, shown; the sync code; colour
    /// space 2 (BT.709), studio range; the size minus one.
    fn key_frame(extra: usize) -> Bytes {
        // bits: 10 0 0 0 0 1 0 | 0x49 0x83 0x42 | 010 0 + width-1 (16) + height-1 (16)
        let mut bits: Vec<u8> = vec![1, 0, 0, 0, 0, 0, 1, 0];
        for byte in [0x49u8, 0x83, 0x42] {
            bits.extend((0..8).rev().map(|i| (byte >> i) & 1));
        }
        bits.extend([0, 1, 0, 0]);
        for value in [1279u16, 719] {
            bits.extend((0..16).rev().map(|i| ((value >> i) & 1) as u8));
        }
        while bits.len() % 8 != 0 {
            bits.push(0);
        }
        let mut frame: Vec<u8> = bits.chunks(8).map(|c| c.iter().fold(0, |acc, bit| (acc << 1) | bit)).collect();
        frame.extend(std::iter::repeat(0xAB).take(extra));
        Bytes::from(frame)
    }

    /// An inter frame: frame marker, profile 0, not show_existing, non-key.
    fn inter_frame(extra: usize) -> Bytes {
        let mut frame = vec![0b1000_0110u8];
        frame.extend(std::iter::repeat(0xCD).take(extra));
        Bytes::from(frame)
    }

    #[test]
    fn the_header_says_which_frames_are_key_frames_and_their_size() {
        assert_eq!(Vp9Header::parse(&key_frame(10)), Some(Vp9Header { non_key_frame: false, width: 1280, height: 720 }));
        assert_eq!(Vp9Header::parse(&inter_frame(10)), Some(Vp9Header { non_key_frame: true, width: 0, height: 0 }));
        // show_existing_frame depends on an earlier frame.
        assert!(Vp9Header::parse(&[0b1000_1000, 0]).unwrap().non_key_frame);
        // Not VP9 (wrong frame marker), a wrong sync code, and too short.
        assert_eq!(Vp9Header::parse(&[0x00, 0x00]), None);
        assert_eq!(Vp9Header::parse(&[0b1000_0010, 0x49, 0x83, 0x00, 0, 0, 0, 0, 0]), None);
        assert_eq!(Vp9Header::parse(&[]), None);
    }

    #[test]
    fn a_key_frame_is_marked_as_one_and_an_inter_frame_is_not() {
        let mut payloader = LunarisVp9Payloader::with_picture_id(0x1234);
        // A key frame over three packets of at most 100 bytes.
        let frame = key_frame(200);
        let packets = payloader.payload(100, &frame).unwrap();
        assert_eq!(packets.len(), 3);
        // First: I, B, V, Z set; P clear (a key frame); F clear (non-flexible).
        assert_eq!(packets[0][0], 0x80 | 0x08 | 0x02 | 0x01);
        assert_eq!(&packets[0][1..3], &[0x92, 0x34]);
        // The scalability structure: one layer with its size, one picture group.
        assert_eq!(&packets[0][3..11], &[0x18, 0x05, 0x00, 0x02, 0xD0, 0x01, 0x14, 0x01]);
        assert_eq!(packets[1][0], 0x80 | 0x01);
        assert_eq!(packets[2][0], 0x80 | 0x04 | 0x01);
        assert!(packets.iter().all(|p| p.len() <= 100));
        // The frame is whole again from the packets.
        let mut whole = packets[0][11..].to_vec();
        whole.extend_from_slice(&packets[1][3..]);
        whole.extend_from_slice(&packets[2][3..]);
        assert_eq!(whole, frame.to_vec());

        // The next frame: an inter frame in one packet. P set, the picture id one more.
        let packets = payloader.payload(100, &inter_frame(50)).unwrap();
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0][0], 0x80 | 0x40 | 0x08 | 0x04 | 0x01);
        assert_eq!(&packets[0][1..3], &[0x92, 0x35]);
        assert_eq!(packets[0].len(), 3 + 51);

        // What is not VP9 yields nothing; the picture id wraps at 15 bits.
        assert!(payloader.payload(100, &Bytes::from_static(&[0, 0, 0])).unwrap().is_empty());
        let mut last = LunarisVp9Payloader::with_picture_id(0x7FFF);
        last.payload(100, &inter_frame(1)).unwrap();
        assert_eq!(&last.payload(100, &inter_frame(1)).unwrap()[0][1..3], &[0x80, 0x00]);
    }
}
