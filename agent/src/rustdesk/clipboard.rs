//! Clipboard text between this computer and a RustDesk client, both ways,
//! in RustDesk's messages (`Clipboard`, `MultiClipboards`).
//!
//! The messages are ported from rustdesk/src/clipboard.rs (`mod proto`:
//! `plain_to_proto`, `from_clipboard`, `get_msg_if_not_support_multi_clip`,
//! and `is_support_multi_clipboard`). This computer's clipboard is the one
//! share.rs already keeps for Lunaris's own sessions (`clipboard_thread`):
//! text only, what was there before the session is not sent, and what the
//! agent itself put there to type text is not echoed.

use std::sync::mpsc as std_mpsc;

use bytes::Bytes;
use tokio::sync::mpsc;

use super::cursor::{compress, decompress};
use super::proto::{message, Clipboard, ClipboardFormat, Message, MultiClipboards};
use crate::share::MAX_CLIPBOARD_BYTES;

/// This computer's clipboard for one session: text to put on it, and text
/// that appeared on it.
pub(crate) struct ClipboardLink {
    pub set: std_mpsc::Sender<String>,
    pub changed: mpsc::UnboundedReceiver<String>,
}

impl ClipboardLink {
    /// The real clipboard (share.rs `clipboard_thread`, which ends when the
    /// link is dropped).
    pub(crate) fn system() -> Option<Self> {
        let (set, set_rx) = std_mpsc::channel();
        let (changed_tx, changed) = mpsc::unbounded_channel();
        std::thread::Builder::new()
            .name("lunaris-rustdesk-clipboard".into())
            .spawn(move || crate::share::clipboard_thread(set_rx, changed_tx))
            .ok()?;
        Some(ClipboardLink { set, changed })
    }
}

/// hbb_common/src/lib.rs `get_version_number`: "1.2.3" -> 1002030 (the last
/// part times ten, leaving room for a "-n" suffix).
pub(crate) fn get_version_number(v: &str) -> i64 {
    let mut versions = v.split('-');
    let mut n = 0;
    if let Some(v) = versions.next() {
        let mut last = 0;
        for x in v.split('.') {
            last = x.parse::<i64>().unwrap_or(0);
            n = n * 1000 + last;
        }
        n -= last;
        n += last * 10;
    }
    if let Some(v) = versions.next() {
        n += v.parse::<i64>().unwrap_or(0);
    }
    n
}

/// rustdesk/src/clipboard.rs `is_support_multi_clipboard`.
pub(crate) fn is_support_multi_clipboard(peer_version: &str, peer_platform: &str) -> bool {
    if get_version_number(peer_version) < get_version_number("1.3.0") {
        return false;
    }
    // "" and `whoami::Platform::Ios.to_string()`.
    if ["", "iOS"].contains(&peer_platform) {
        return false;
    }
    if peer_platform == "Android" && get_version_number(peer_version) < get_version_number("1.3.3") {
        return false;
    }
    true
}

/// `plain_to_proto`: text, compressed when that makes it smaller.
fn text_to_proto(text: &str) -> Clipboard {
    let compressed = compress(text.as_bytes());
    let compress = !compressed.is_empty() && compressed.len() < text.len();
    Clipboard {
        compress,
        content: if compress { Bytes::from(compressed) } else { Bytes::copy_from_slice(text.as_bytes()) },
        format: ClipboardFormat::Text as i32,
        ..Default::default()
    }
}

/// The message telling the client this computer's clipboard text
/// (rustdesk/src/clipboard.rs `create_multi_clipboards`; a client from
/// before `MultiClipboards` gets the text alone,
/// `get_msg_if_not_support_multi_clip`).
pub(crate) fn text_message(text: &str, peer_version: &str, peer_platform: &str) -> Message {
    let clipboard = text_to_proto(text);
    let union = if is_support_multi_clipboard(peer_version, peer_platform) {
        message::Union::MultiClipboards(MultiClipboards { clipboards: vec![clipboard] })
    } else {
        message::Union::Clipboard(clipboard)
    };
    Message { union: Some(union) }
}

/// `from_clipboard` for the one format carried here: the text of a
/// `Clipboard`, `None` for other formats, bad UTF-8 or more than share.rs
/// lets a client set.
fn text_of(clipboard: &Clipboard) -> Option<String> {
    if ClipboardFormat::try_from(clipboard.format) != Ok(ClipboardFormat::Text) {
        return None;
    }
    let data = if clipboard.compress {
        decompress(&clipboard.content, MAX_CLIPBOARD_BYTES)?
    } else if clipboard.content.len() <= MAX_CLIPBOARD_BYTES {
        clipboard.content.to_vec()
    } else {
        return None;
    };
    String::from_utf8(data).ok()
}

/// Whether there is text among `clipboards` that is too large to take
/// (more than share.rs lets pass): it is not cut short, and the person is
/// told instead.
pub(crate) fn has_oversized_text(clipboards: &[Clipboard]) -> bool {
    clipboards.iter().any(|clipboard| {
        ClipboardFormat::try_from(clipboard.format) == Ok(ClipboardFormat::Text)
            && if clipboard.compress {
                !clipboard.content.is_empty() && decompress(&clipboard.content, MAX_CLIPBOARD_BYTES).is_none()
            } else {
                clipboard.content.len() > MAX_CLIPBOARD_BYTES
            }
    })
}

/// The text a client's clipboard message carries
/// (rustdesk/src/server/connection.rs `on_message`: `Clipboard` and
/// `MultiClipboards` both end in `update_clipboard`).
pub(crate) fn text_from(clipboards: &[Clipboard]) -> Option<String> {
    clipboards.iter().find_map(text_of)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_numbers_as_rustdesk_counts_them() {
        // The examples in hbb_common's comment.
        assert_eq!(get_version_number("1.1.10"), 1001100);
        assert_eq!(get_version_number("1.2.3"), 1002030);
        assert_eq!(get_version_number("1.2.3-1"), 1002031);
        assert!(get_version_number("1.4.9") < get_version_number("1.5.1"));
    }

    #[test]
    fn older_and_ios_clients_get_the_single_clipboard_message() {
        assert!(is_support_multi_clipboard("1.5.1", "Linux"));
        assert!(is_support_multi_clipboard("1.3.0", "Windows"));
        assert!(!is_support_multi_clipboard("1.2.7", "Linux"));
        assert!(!is_support_multi_clipboard("1.5.1", "iOS"));
        assert!(!is_support_multi_clipboard("1.5.1", ""));
        assert!(!is_support_multi_clipboard("1.3.2", "Android"));
        assert!(is_support_multi_clipboard("1.3.3", "Android"));
        assert!(matches!(text_message("x", "1.5.1", "Linux").union, Some(message::Union::MultiClipboards(_))));
        assert!(matches!(text_message("x", "1.2.7", "Linux").union, Some(message::Union::Clipboard(_))));
    }

    #[test]
    fn text_goes_both_ways_compressed_only_when_smaller() {
        // Short text does not shrink: sent as it is.
        let short = text_to_proto("Tiếng Việt");
        assert!(!short.compress);
        assert_eq!(&short.content[..], "Tiếng Việt".as_bytes());
        assert_eq!(text_from(&[short]).as_deref(), Some("Tiếng Việt"));
        // Long, repetitive text does: one zstd frame.
        let long_text = "Tiếng Việt ".repeat(200);
        let long = text_to_proto(&long_text);
        assert!(long.compress);
        assert!(long.content.len() < long_text.len());
        assert_eq!(&long.content[..4], &[0x28, 0xB5, 0x2F, 0xFD]);
        assert_eq!(text_from(&[long]).as_deref(), Some(long_text.as_str()));
    }

    #[test]
    fn only_text_within_the_limit_is_taken() {
        let image = Clipboard { format: ClipboardFormat::ImagePng as i32, content: Bytes::from_static(b"png"), ..Default::default() };
        let text = Clipboard { format: ClipboardFormat::Text as i32, content: Bytes::from_static(b"hello"), ..Default::default() };
        assert_eq!(text_from(&[image.clone(), text]).as_deref(), Some("hello"));
        assert_eq!(text_from(&[image]), None);
        let bad_utf8 = Clipboard { content: Bytes::from_static(&[0xFF, 0xFE]), ..Default::default() };
        assert_eq!(text_from(&[bad_utf8]), None);
        let not_zstd = Clipboard { compress: true, content: Bytes::from_static(b"plain"), ..Default::default() };
        assert_eq!(text_from(&[not_zstd]), None);
        // A small message that unpacks to more than the limit.
        let bomb = Clipboard { compress: true, content: Bytes::from(compress(&vec![b'a'; MAX_CLIPBOARD_BYTES + 1])), ..Default::default() };
        assert!(bomb.content.len() < 10_000);
        assert_eq!(text_from(&[bomb]), None);
        let huge = Clipboard { content: Bytes::from(vec![b'a'; MAX_CLIPBOARD_BYTES + 1]), ..Default::default() };
        assert_eq!(text_from(&[huge]), None);
    }
}
