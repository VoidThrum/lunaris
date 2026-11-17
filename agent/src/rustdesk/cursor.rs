//! The host's cursor for a RustDesk client, which draws it itself: the
//! shape as `CursorData` (once; `CursorId` when it is shown again) and, when
//! the client asks to see the remote cursor, where it is (`CursorPosition`).
//!
//! Ported from rustdesk/src/server/input_service.rs (`run_cursor`,
//! `cursor_shape_message`, `MouseCursorSub::send`, `run_pos`).

use std::collections::HashSet;

use bytes::Bytes;
use lunaris_media::types::CursorImage;

use super::proto::{message, CursorData, CursorPosition, Message};

/// hbb_common/src/config.rs `COMPRESS_LEVEL`.
const COMPRESS_LEVEL: i32 = 3;
/// The client refuses larger cursors (rustdesk/src/client/io_loop.rs
/// `decode_cursor_data`: `MAX_CURSOR_SIZE`).
const MAX_CURSOR_SIZE: u32 = 512;
/// Shapes remembered as sent, per connection (a client keeps what it got).
const MAX_SENT_SHAPES: usize = 4096;

/// hbb_common/src/compress.rs `compress`: one zstd frame.
pub(crate) fn compress(data: &[u8]) -> Vec<u8> {
    zstd::bulk::compress(data, COMPRESS_LEVEL).unwrap_or_default()
}

/// hbb_common/src/compress.rs `decompress`, with the caller's limit on what
/// comes out; `None` when it is not zstd or larger than that.
pub(crate) fn decompress(data: &[u8], limit: usize) -> Option<Vec<u8>> {
    use std::io::Read;
    let decoder = zstd::Decoder::new(data).ok()?;
    let mut out = Vec::new();
    decoder.take(limit as u64 + 1).read_to_end(&mut out).ok()?;
    (out.len() <= limit).then_some(out)
}

/// rustdesk/src/common.rs `cursor_content_id`: one id per cursor look, an
/// xxh3 hash of its size, hot spot and pixels, held to 53 bits.
pub(crate) fn cursor_content_id(width: i32, height: i32, hotx: i32, hoty: i32, colors: &[u8]) -> u64 {
    let mut hasher = xxhash_rust::xxh3::Xxh3::new();
    for v in [width, height, hotx, hoty] {
        hasher.update(&v.to_le_bytes());
    }
    hasher.update(colors);
    hasher.digest() & ((1 << 53) - 1)
}

/// What one connection was sent of the cursor.
#[derive(Default)]
pub(crate) struct CursorSender {
    /// `MouseCursorSub::cached`: ids the client already has the pixels of.
    sent: HashSet<u64>,
    /// The shape shown now (`StateCursor::cursor_data`), for a client that
    /// starts to want it later.
    current: Option<(u64, CursorData)>,
    /// Whether the current shape went out.
    current_sent: bool,
    position: Option<(i32, i32)>,
}

impl CursorSender {
    /// A new shape from the pipeline (RGBA, as RustDesk's `CursorData.colors`
    /// before compression). Shapes the client would refuse are dropped.
    pub(crate) fn set_shape(&mut self, image: &CursorImage) {
        let (width, height) = (image.width, image.height);
        if width == 0 || height == 0 || width > MAX_CURSOR_SIZE || height > MAX_CURSOR_SIZE {
            return;
        }
        if image.rgba_data.len() != (width * height * 4) as usize {
            return;
        }
        // The client also refuses a hot spot outside the picture.
        let hotx = image.hotspot_x.min(width - 1) as i32;
        let hoty = image.hotspot_y.min(height - 1) as i32;
        let id = cursor_content_id(width as i32, height as i32, hotx, hoty, &image.rgba_data);
        if self.current.as_ref().is_some_and(|(current, _)| *current == id) {
            return;
        }
        // `cursor_shape_message`: compressed once, named by content.
        let data = CursorData {
            id,
            hotx,
            hoty,
            width: width as i32,
            height: height as i32,
            colors: Bytes::from(compress(&image.rgba_data)),
        };
        self.current = Some((id, data));
        self.current_sent = false;
    }

    /// The message for the current shape when the client has not been told
    /// of it yet (`MouseCursorSub::send`: the pixels the first time, only
    /// the id after).
    pub(crate) fn shape_message(&mut self) -> Option<Message> {
        if self.current_sent {
            return None;
        }
        let (id, data) = self.current.as_ref()?;
        self.current_sent = true;
        if self.sent.contains(id) {
            return Some(Message { union: Some(message::Union::CursorId(*id)) });
        }
        if self.sent.len() >= MAX_SENT_SHAPES {
            self.sent.clear();
        }
        self.sent.insert(*id);
        Some(Message { union: Some(message::Union::CursorData(data.clone())) })
    }

    /// The client stopped wanting the shape: the current one is told again
    /// when it wants it back.
    pub(crate) fn forget_current(&mut self) {
        self.current_sent = false;
    }

    /// `run_pos`: the position when it moved.
    pub(crate) fn position_message(&mut self, x: i32, y: i32) -> Option<Message> {
        if self.position == Some((x, y)) {
            return None;
        }
        self.position = Some((x, y));
        Some(Message { union: Some(message::Union::CursorPosition(CursorPosition { x, y })) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(fill: u8) -> CursorImage {
        CursorImage { width: 4, height: 2, hotspot_x: 1, hotspot_y: 1, rgba_data: vec![fill; 32] }
    }

    // xxh3-64 of the little-endian width, height, hot spot (4, 2, 1, 1) and
    // 32 bytes of 0x07, masked to 53 bits; computed with Python's xxhash.
    #[test]
    fn the_id_is_the_xxh3_of_the_shape() {
        assert_eq!(cursor_content_id(4, 2, 1, 1, &[7u8; 32]), CONTENT_ID_VECTOR);
        assert_ne!(cursor_content_id(4, 2, 1, 0, &[7u8; 32]), CONTENT_ID_VECTOR);
        assert!(cursor_content_id(4, 2, 1, 1, &[9u8; 32]) < (1 << 53));
    }

    const CONTENT_ID_VECTOR: u64 = 7956738265229296;

    #[test]
    fn pixels_are_one_zstd_frame_the_client_can_open() {
        let mut sender = CursorSender::default();
        sender.set_shape(&image(7));
        let Some(Message { union: Some(message::Union::CursorData(data)) }) = sender.shape_message() else {
            panic!("expected CursorData");
        };
        assert_eq!((data.width, data.height, data.hotx, data.hoty), (4, 2, 1, 1));
        assert_eq!(data.id, cursor_content_id(4, 2, 1, 1, &[7u8; 32]));
        // The zstd magic number, and what the client does with it
        // (io_loop.rs `decode_cursor_data`: `zstd::bulk::decompress(colors, w * h * 4)`).
        assert_eq!(&data.colors[..4], &[0x28, 0xB5, 0x2F, 0xFD]);
        assert_eq!(zstd::bulk::decompress(&data.colors, 32).unwrap(), vec![7u8; 32]);
        assert_eq!(decompress(&data.colors, 32).unwrap(), vec![7u8; 32]);
        assert!(decompress(&data.colors, 31).is_none());
        assert!(decompress(b"not zstd", 100).is_none());
    }

    #[test]
    fn a_shape_is_sent_once_and_named_by_its_id_after() {
        let mut sender = CursorSender::default();
        assert!(sender.shape_message().is_none());
        sender.set_shape(&image(7));
        assert!(matches!(sender.shape_message().unwrap().union, Some(message::Union::CursorData(_))));
        assert!(sender.shape_message().is_none());
        // The same shape again changes nothing.
        sender.set_shape(&image(7));
        assert!(sender.shape_message().is_none());
        sender.set_shape(&image(8));
        assert!(matches!(sender.shape_message().unwrap().union, Some(message::Union::CursorData(_))));
        // Back to the first: the client has its pixels.
        sender.set_shape(&image(7));
        let first = cursor_content_id(4, 2, 1, 1, &[7u8; 32]);
        assert_eq!(sender.shape_message().unwrap().union, Some(message::Union::CursorId(first)));
        sender.forget_current();
        assert_eq!(sender.shape_message().unwrap().union, Some(message::Union::CursorId(first)));
    }

    #[test]
    fn shapes_the_client_would_refuse_are_not_sent() {
        let mut sender = CursorSender::default();
        sender.set_shape(&CursorImage { width: 513, height: 1, hotspot_x: 0, hotspot_y: 0, rgba_data: vec![0; 513 * 4] });
        sender.set_shape(&CursorImage { width: 2, height: 2, hotspot_x: 0, hotspot_y: 0, rgba_data: vec![0; 15] });
        assert!(sender.shape_message().is_none());
        // A hot spot outside the picture is moved inside.
        sender.set_shape(&CursorImage { width: 2, height: 2, hotspot_x: 9, hotspot_y: 2, rgba_data: vec![0; 16] });
        let Some(message::Union::CursorData(data)) = sender.shape_message().unwrap().union else { panic!() };
        assert_eq!((data.hotx, data.hoty), (1, 1));
    }

    #[test]
    fn the_position_is_sent_when_it_changes() {
        let mut sender = CursorSender::default();
        assert!(sender.position_message(3, 4).is_some());
        assert!(sender.position_message(3, 4).is_none());
        assert!(sender.position_message(3, 5).is_some());
    }
}
