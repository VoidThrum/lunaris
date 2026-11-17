//! RustDesk's stream framing: every protobuf `Message` travels in one frame
//! whose header carries the payload length.
//!
//! Ported from hbb_common/src/bytes_codec.rs (`BytesCodec`): the low two bits
//! of the first header byte say how long the header is (1 to 4 bytes), the
//! rest of the header, little endian, is the payload length shifted left by
//! two.

use bytes::{Buf, BufMut, BytesMut};
use std::io;

/// hbb_common/src/bytes_codec.rs `MAX_FRAME_LENGTH`: the largest payload the
/// four-byte header can describe.
pub const MAX_FRAME_LENGTH: usize = 0x3FFF_FFFF;

/// hbb_common/src/bytes_codec.rs `MAX_PREALLOCATED_PAYLOAD_LEN`: never
/// reserve more than this on the word of a header alone.
const MAX_PREALLOCATED_PAYLOAD_LEN: usize = 256 * 1024;

/// Appends the header of a `len`-byte payload to `buf`
/// (hbb_common/src/bytes_codec.rs `Encoder::encode`).
pub fn put_header(len: usize, buf: &mut BytesMut) -> io::Result<()> {
    if len <= 0x3F {
        buf.put_u8((len << 2) as u8);
    } else if len <= 0x3FFF {
        buf.put_u16_le((len << 2) as u16 | 0x1);
    } else if len <= 0x3F_FFFF {
        let h = (len << 2) as u32 | 0x2;
        buf.put_u16_le((h & 0xFFFF) as u16);
        buf.put_u8((h >> 16) as u8);
    } else if len <= MAX_FRAME_LENGTH {
        buf.put_u32_le((len << 2) as u32 | 0x3);
    } else {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Overflow"));
    }
    Ok(())
}

/// Splits frames off a receive buffer
/// (hbb_common/src/bytes_codec.rs `Decoder::decode`, `decode_head`,
/// `decode_data`).
pub struct Decoder {
    /// Payload length of the frame whose header was already consumed.
    pending: Option<usize>,
    /// hbb_common/src/bytes_codec.rs `set_max_packet_length`.
    max_packet_length: usize,
}

impl Decoder {
    pub fn new(max_packet_length: usize) -> Self {
        Decoder { pending: None, max_packet_length }
    }

    pub fn set_max_packet_length(&mut self, n: usize) {
        self.max_packet_length = n;
    }

    /// The next complete payload in `src`, `None` when more bytes are needed.
    pub fn decode(&mut self, src: &mut BytesMut) -> io::Result<Option<BytesMut>> {
        let n = match self.pending {
            Some(n) => n,
            None => {
                if src.is_empty() {
                    return Ok(None);
                }
                let head_len = ((src[0] & 0x3) + 1) as usize;
                if src.len() < head_len {
                    return Ok(None);
                }
                let mut n = src[0] as usize;
                if head_len > 1 {
                    n |= (src[1] as usize) << 8;
                }
                if head_len > 2 {
                    n |= (src[2] as usize) << 16;
                }
                if head_len > 3 {
                    n |= (src[3] as usize) << 24;
                }
                n >>= 2;
                if n > self.max_packet_length {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "Too big packet"));
                }
                src.advance(head_len);
                src.reserve(n.saturating_sub(src.len()).min(MAX_PREALLOCATED_PAYLOAD_LEN));
                self.pending = Some(n);
                n
            }
        };
        if src.len() < n {
            return Ok(None);
        }
        self.pending = None;
        Ok(Some(src.split_to(n)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(len: usize) -> Vec<u8> {
        let mut buf = BytesMut::new();
        put_header(len, &mut buf).unwrap();
        buf.to_vec()
    }

    fn round_trip(len: usize) -> usize {
        let mut buf = BytesMut::new();
        put_header(len, &mut buf).unwrap();
        let head = buf.len();
        buf.resize(head + len, 1);
        let mut decoder = Decoder::new(usize::MAX);
        let out = decoder.decode(&mut buf).unwrap().expect("a whole frame");
        assert_eq!(out.len(), len);
        assert!(buf.is_empty());
        assert!(decoder.decode(&mut buf).unwrap().is_none());
        head
    }

    // The sizes hbb_common's own tests use (bytes_codec.rs test_codec1..4):
    // total length = header + payload.
    #[test]
    fn header_length_follows_the_payload_size() {
        assert_eq!(round_trip(0x3F), 1);
        assert_eq!(round_trip(0x3F + 1), 2);
        assert_eq!(round_trip(0x3FFF), 2);
        assert_eq!(round_trip(0x3FFF + 1), 3);
        assert_eq!(round_trip(0x3F_FFFF), 3);
        assert_eq!(round_trip(0x3F_FFFF + 1), 4);
    }

    // Bytes worked out by hand from `Encoder::encode`: (len << 2) | (header
    // length - 1), little endian.
    #[test]
    fn header_bytes_match_the_reference_encoding() {
        assert_eq!(header(0), [0x00]);
        assert_eq!(header(5), [0x14]);
        assert_eq!(header(0x3F), [0xFC]);
        assert_eq!(header(0x40), [0x01, 0x01]);
        assert_eq!(header(0x3FFF), [0xFD, 0xFF]);
        assert_eq!(header(0x4000), [0x02, 0x00, 0x01]);
        assert_eq!(header(0x3F_FFFF), [0xFE, 0xFF, 0xFF]);
        assert_eq!(header(0x40_0000), [0x03, 0x00, 0x00, 0x01]);
        assert_eq!(header(MAX_FRAME_LENGTH), [0xFF, 0xFF, 0xFF, 0xFF]);
        let mut buf = BytesMut::new();
        assert!(put_header(MAX_FRAME_LENGTH + 1, &mut buf).is_err());
    }

    #[test]
    fn frames_arriving_in_pieces_are_reassembled() {
        let mut wire = BytesMut::new();
        for payload in [&b"hello"[..], &[7u8; 300][..], &b""[..], &b"x"[..]] {
            put_header(payload.len(), &mut wire).unwrap();
            wire.extend_from_slice(payload);
        }
        let mut decoder = Decoder::new(usize::MAX);
        let mut buf = BytesMut::new();
        let mut got = Vec::new();
        for byte in wire {
            buf.put_u8(byte);
            while let Some(frame) = decoder.decode(&mut buf).unwrap() {
                got.push(frame.to_vec());
            }
        }
        assert_eq!(got, vec![b"hello".to_vec(), vec![7u8; 300], Vec::new(), b"x".to_vec()]);
    }

    #[test]
    fn a_frame_over_the_limit_is_refused_before_its_payload_arrives() {
        let mut buf = BytesMut::new();
        put_header(128 * 1024 + 1, &mut buf).unwrap();
        let mut decoder = Decoder::new(128 * 1024);
        assert!(decoder.decode(&mut buf).is_err());

        let mut buf = BytesMut::new();
        put_header(128 * 1024, &mut buf).unwrap();
        assert!(decoder.decode(&mut buf).unwrap().is_none());
        decoder.set_max_packet_length(usize::MAX);
    }
}
