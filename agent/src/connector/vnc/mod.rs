//! VNC connector: the client side of RFB 3.8 (RFC 6143), written from the
//! RFC (no VNC crate: none found that is maintained and covers ZRLE and
//! the cursor; the RFB code here is small).
//!
//! - Security: None (1) and VNC Authentication (2, §7.2.2). The others
//!   (VeNCrypt / TLS, RA2, Apple, ...) are named in a clear error. Plain VNC
//!   is not encrypted: the person reads that in the connector's note.
//! - Encodings asked for: ZRLE, CopyRect, Raw, and the pseudo-encodings
//!   Cursor, ExtendedDesktopSize and DesktopSize (rfb.rs).
//! - The picture is kept here and encoded with this computer's encoders
//!   (raw_video.rs); updates are asked for at the viewer's frame rate.
//! - Input: keys as X keysyms (keysym.rs), the pointer with the wheel as
//!   buttons 4-7; clipboard text both ways in Latin-1.
//! - Not done: Back / Forward mouse buttons, Unicode clipboard (the
//!   Extended Clipboard extension), sound (RFB has none), several screens.

pub mod keysym;
pub mod rfb;
pub mod session;

#[cfg(test)]
mod fake_server;
#[cfg(test)]
mod tests;

use super::raw_video::EncoderChoice;
use super::{Capabilities, Codec, Connector, Description, Field, Session, Target, Viewer};

pub const NOTE: &str = "A VNC server must be running on that computer, with \"None\" or \"VNC password\" security. VNC does not encrypt the picture or what you type: use it only on a network you trust, or through a VPN.";

pub struct VncConnector {
    pub(crate) encoders: EncoderChoice,
}

impl VncConnector {
    pub fn new() -> VncConnector {
        VncConnector { encoders: EncoderChoice::Auto }
    }
}

impl Default for VncConnector {
    fn default() -> Self {
        VncConnector::new()
    }
}

#[async_trait::async_trait]
impl Connector for VncConnector {
    fn describe(&self) -> Description {
        let field = |key: &str, label: &str, kind: &str, default: &str, required: bool| Field {
            key: key.into(),
            label: label.into(),
            kind: kind.into(),
            default: default.into(),
            required,
            options: Vec::new(),
        };
        let option = |value: &str, label: &str| common::FieldOptionMsg { value: value.into(), label: label.into() };
        Description {
            id: "vnc".into(),
            name: "VNC".into(),
            fields: vec![
                field("host", "Address", "text", "", true),
                field("port", "Port", "port", &session::DEFAULT_PORT.to_string(), false),
                field("password", "Password", "secret", "", false),
                Field {
                    options: vec![option("off", "Control"), option("on", "View only")],
                    ..field("view_only", "Mode", "choice", "off", false)
                },
            ],
            note: NOTE.into(),
            capabilities: Capabilities {
                video: vec![Codec::H264, Codec::H265, Codec::Av1],
                audio: false,
                input: true,
                clipboard: true,
                cursor: true,
                files: false,
                setup: false,
            },
        }
    }

    async fn connect(&self, target: &Target, viewer: &Viewer) -> anyhow::Result<Session> {
        session::connect(target, viewer, self.encoders).await
    }
}
