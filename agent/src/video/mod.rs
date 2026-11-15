pub mod annexb;
pub mod av1;
pub mod h264;
pub mod h265;

use bytes::BytesMut;
use std::ops::Range;

pub fn trim_bytes_to_range(mut buf: BytesMut, range: Range<usize>) -> BytesMut {
    if range.start > 0 {
        let _ = buf.split_to(range.start);
    }

    if range.end - range.start < buf.len() {
        let _ = buf.split_off(range.end - range.start);
    }

    buf
}

/// The encoder's frame as a `BytesMut` without copying it (`Bytes::from(Vec)`
/// keeps the allocation, and a unique `Bytes` converts back in place), so the
/// NAL units split from it are views into the same buffer.
pub fn frame_buffer(frame: Vec<u8>) -> BytesMut {
    BytesMut::from(bytes::Bytes::from(frame))
}
