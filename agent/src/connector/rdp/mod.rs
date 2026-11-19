//! RDP connector: the client side of the Remote Desktop Protocol through
//! IronRDP (Devolutions; MIT OR Apache-2.0).
//!
//! - Security: NLA (CredSSP over TLS, NTLM; MS-CSSP) or TLS with the
//!   server's own logon (MS-RDPBCGR 5.4.5.1), or either as the server
//!   chooses. Standard RDP Security (RC4) is never offered.
//! - The server's certificate is pinned when the computer is added: the
//!   set-up ([`Connector::setup`]) opens a TLS connection, keeps the
//!   certificate's SHA-256 with the target, and logs in to nothing. Every
//!   session then takes that certificate and no other (tls.rs).
//! - The picture: IronRDP decodes the server's bitmaps (planar, interleaved,
//!   RemoteFX as ironrdp-session negotiates them) into its image; the
//!   changed rectangles are copied into the picture that raw_video.rs
//!   encodes.
//! - The pointer: the server's pointer shapes and positions go to the
//!   viewer, which draws the pointer itself.
//! - Input: keys by scan code (where they are on the keyboard), text as
//!   Unicode keyboard events, the mouse with both wheels; fast-path input.
//! - Clipboard text both ways (CLIPRDR, clipboard.rs).
//! - Not done: sound (RDPSND), files, display resizing during a session
//!   (Display Control), several monitors, Kerberos, RD Gateway, smart cards.

pub mod clipboard;
pub mod session;
pub mod tls;

#[cfg(test)]
mod fake_server;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::time::Duration;

use ironrdp_connector::ClientConnector;
use ironrdp_tokio::TokioFramed;
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use super::raw_video::EncoderChoice;
use super::{Capabilities, Codec, Connector, Description, Field, Session, SetupStep, Target, Viewer};

pub const NOTE: &str = "Remote Desktop must be enabled on that computer (Windows: Settings, System, Remote Desktop). When you press Add, its certificate is checked and remembered; Lunaris refuses the connection if it changes later.";
pub const SETUP_MESSAGE: &str = "Checking the computer's certificate…";
/// Where the pinned certificate's SHA-256 (hex) is kept with the target.
pub const PIN_KEY: &str = "server_cert_sha256";

/// The security protocols offered (MS-RDPBCGR 2.2.1.1.1 requestedProtocols).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Security {
    /// PROTOCOL_HYBRID / HYBRID_EX: CredSSP.
    Nla,
    /// PROTOCOL_SSL: TLS, the logon on the server's screen.
    Tls,
    /// Both; the server chooses.
    Any,
}

impl Security {
    pub fn of(value: &str) -> anyhow::Result<Security> {
        match value.trim() {
            "" | "nla" => Ok(Security::Nla),
            "tls" => Ok(Security::Tls),
            "any" => Ok(Security::Any),
            other => anyhow::bail!("\"{other}\" is not a security choice (nla, tls or any)"),
        }
    }
}

pub struct RdpConnector {
    pub(crate) encoders: EncoderChoice,
}

impl RdpConnector {
    pub fn new() -> RdpConnector {
        RdpConnector { encoders: EncoderChoice::Auto }
    }
}

impl Default for RdpConnector {
    fn default() -> Self {
        RdpConnector::new()
    }
}

/// The X.224 negotiation and the TLS handshake only, for the certificate.
pub async fn fetch_certificate(options: &session::Options) -> anyhow::Result<tls::Upgraded<TcpStream>> {
    let address = format!("{}:{}", options.host, options.port);
    let stream = match tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(&address)).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => anyhow::bail!("The computer could not be reached ({e})"),
        Err(_) => anyhow::bail!("The computer did not answer"),
    };
    let client_addr = stream.local_addr()?;
    let config = session::connector_config(options, session::Options::desktop_size(options, &Viewer { codecs: Vec::new(), fps: 0, width: 0, height: 0, bitrate_kbps: 0 }));
    let mut connector = ClientConnector::new(config, client_addr);
    let mut framed = TokioFramed::new(stream);
    ironrdp_tokio::connect_begin(&mut framed, &mut connector).await.map_err(|e| anyhow::anyhow!(session::explain(&e, options.security)))?;
    let stream = framed.into_inner_no_leftover();
    tls::upgrade(stream, &options.host, None).await.map_err(|e| anyhow::anyhow!("{e}"))
}

#[async_trait::async_trait]
impl Connector for RdpConnector {
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
            id: "rdp".into(),
            name: "Remote Desktop (RDP)".into(),
            fields: vec![
                field("host", "Address", "text", "", true),
                field("port", "Port", "port", &session::DEFAULT_PORT.to_string(), false),
                field("username", "User name", "text", "", false),
                field("password", "Password", "secret", "", false),
                field("domain", "Domain", "text", "", false),
                Field {
                    options: vec![
                        option("nla", "NLA (recommended)"),
                        option("tls", "TLS (sign in on the computer's screen)"),
                        option("any", "Either"),
                    ],
                    ..field("security", "Security", "choice", "nla", false)
                },
                field("resolution", "Resolution", "text", "auto", false),
            ],
            note: NOTE.into(),
            capabilities: Capabilities {
                video: vec![Codec::H264, Codec::H265, Codec::Av1],
                audio: false,
                input: true,
                clipboard: true,
                cursor: true,
                files: false,
                setup: true,
            },
        }
    }

    async fn connect(&self, target: &Target, viewer: &Viewer) -> anyhow::Result<Session> {
        session::connect(target, viewer, self.encoders).await
    }

    /// Trust on first use: the certificate the computer shows now is the one
    /// taken from now on. No login happens here.
    async fn setup(&self, target: &Target, steps: mpsc::Sender<SetupStep>) -> anyhow::Result<BTreeMap<String, String>> {
        let options = session::Options::of(target)?;
        let _ = steps.send(SetupStep { message: SETUP_MESSAGE.into(), code: None }).await;
        let upgraded = tokio::time::timeout(Duration::from_secs(20), fetch_certificate(&options))
            .await
            .map_err(|_| anyhow::anyhow!("The computer did not finish the secure connection"))??;
        tracing::info!("RDP: certificate pinned for a new computer (SHA-256 {})", tls::display_fingerprint(&upgraded.fingerprint));
        let mut values = BTreeMap::new();
        values.insert(PIN_KEY.to_string(), upgraded.fingerprint);
        Ok(values)
    }
}
