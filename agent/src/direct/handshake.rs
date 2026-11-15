//! The direct-connection handshake, both sides (the desktop app uses the
//! client side through this crate; the server has its own copy in
//! server/src/links.rs, kept in step by the same test vectors).
//!
//! 1. TLS on the direct port with a certificate the agent makes at start.
//!    Nothing can vouch for it, so the client accepts any certificate (it
//!    still checks the handshake signature, so the peer owns that
//!    certificate's key) and relies on step 3 instead.
//! 2. WebSocket over it, without the password in the URL.
//! 3. Client: `hello`; agent: `challenge` with a random nonce; client:
//!    `proof` = its own nonce and HMAC-SHA256(password, CLIENT_LABEL ||
//!    agent nonce || client nonce || SHA-256 of the agent's certificate as
//!    the client saw it); agent: `ok` with HMAC over AGENT_LABEL and the same
//!    data, or `refused`. Binding the certificate into the MAC is the
//!    `tls-server-end-point` channel binding of SCRAM (RFC 5929): someone in
//!    the middle presents another certificate, so without the password the
//!    MACs don't match on either side.
//!
//! Without a password (the person at the computer accepts) there is no
//! binding: TLS still keeps passive listeners out.
//!
//! A client with a password goes on only after the agent's MAC checked out,
//! whatever else the answer says. Older agents don't speak TLS: the client
//! then gives up (there is no plain fallback an attacker could force, the
//! password never goes in a URL); the agent still takes old clients over
//! plain ws:// with `?token=`.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::{self, Message as WsMessage};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::{info, warn};

pub const CLIENT_LABEL: &[u8] = b"lunaris-direct-auth-v1 client";
pub const AGENT_LABEL: &[u8] = b"lunaris-direct-auth-v1 agent";
pub const NONCE_LEN: usize = 32;
pub type Nonce = [u8; NONCE_LEN];
pub type Digest = [u8; 32];

/// Each step of a connection attempt (TCP, TLS, WebSocket, proof).
const STEP_TIMEOUT: Duration = Duration::from_secs(8);

pub fn sha256(data: &[u8]) -> Digest {
    ring::digest::digest(&ring::digest::SHA256, data).as_ref().try_into().expect("SHA-256 is 32 bytes")
}

fn mac_input(label: &[u8], agent_nonce: &Nonce, client_nonce: &Nonce, cert_hash: &Digest) -> Vec<u8> {
    [label, &agent_nonce[..], &client_nonce[..], &cert_hash[..]].concat()
}

fn mac_key(password: &str) -> hmac::Key {
    hmac::Key::new(hmac::HMAC_SHA256, password.trim().as_bytes())
}

fn mac(label: &[u8], password: &str, agent_nonce: &Nonce, client_nonce: &Nonce, cert_hash: &Digest) -> Digest {
    let tag = hmac::sign(&mac_key(password), &mac_input(label, agent_nonce, client_nonce, cert_hash));
    tag.as_ref().try_into().expect("HMAC-SHA256 is 32 bytes")
}

/// The client's proof that it knows the password.
pub fn client_mac(password: &str, agent_nonce: &Nonce, client_nonce: &Nonce, cert_hash: &Digest) -> Digest {
    mac(CLIENT_LABEL, password, agent_nonce, client_nonce, cert_hash)
}

/// The agent's answer, proving it knows the password too.
pub fn agent_mac(password: &str, agent_nonce: &Nonce, client_nonce: &Nonce, cert_hash: &Digest) -> Digest {
    mac(AGENT_LABEL, password, agent_nonce, client_nonce, cert_hash)
}

/// Checks a received MAC in constant time (ring's HMAC verify).
fn mac_ok(label: &[u8], password: &str, agent_nonce: &Nonce, client_nonce: &Nonce, cert_hash: &Digest, tag: &[u8]) -> bool {
    hmac::verify(&mac_key(password), &mac_input(label, agent_nonce, client_nonce, cert_hash), tag).is_ok()
}

/// Constant-time comparison of two fixed-length digests.
pub fn digest_eq(a: &Digest, b: &Digest) -> bool {
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn random_nonce() -> Nonce {
    let mut nonce = [0u8; NONCE_LEN];
    SystemRandom::new().fill(&mut nonce).expect("the OS random source failed");
    nonce
}

pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

pub fn from_hex<const N: usize>(text: &str) -> Option<[u8; N]> {
    if text.len() != 2 * N || !text.bytes().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; N];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

/// Handshake messages (JSON text frames, before any `ClientMessage`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "auth", rename_all = "snake_case")]
pub enum AuthMessage {
    Hello { version: u32 },
    Challenge { nonce: String },
    Proof {
        client_nonce: String,
        /// Absent without a password (asks the person at the computer).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mac: Option<String>,
    },
    Ok {
        /// The password was right; `mac` then proves the agent knows it.
        trusted: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mac: Option<String>,
    },
    /// "unauthorized" or "too_many_attempts".
    Refused { reason: String },
}

pub const REFUSED_UNAUTHORIZED: &str = "unauthorized";
pub const REFUSED_TOO_MANY: &str = "too_many_attempts";

async fn send_auth<S: AsyncRead + AsyncWrite + Unpin>(ws: &mut WebSocketStream<S>, msg: &AuthMessage) -> Result<(), tungstenite::Error> {
    let json = serde_json::to_string(msg).expect("handshake messages serialize");
    ws.send(WsMessage::Text(json.into())).await
}

/// The next handshake message; None when the peer closed or sent something else.
async fn recv_auth<S: AsyncRead + AsyncWrite + Unpin>(ws: &mut WebSocketStream<S>) -> Option<AuthMessage> {
    loop {
        match ws.next().await? {
            Ok(WsMessage::Text(text)) => return serde_json::from_str(&text).ok(),
            Ok(WsMessage::Ping(_) | WsMessage::Pong(_)) => continue,
            _ => return None,
        }
    }
}

// ---- Agent side ----

/// What a peer showed of the password.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presented {
    Nothing,
    Right,
    Wrong,
}

/// TLS of the direct listener: a fresh self-signed certificate per start.
pub struct TlsServer {
    pub acceptor: tokio_rustls::TlsAcceptor,
    pub cert_hash: Digest,
}

pub fn make_tls_server() -> anyhow::Result<TlsServer> {
    let made = rcgen::generate_simple_self_signed(vec!["lunaris-agent".to_string()])?;
    let cert: CertificateDer<'static> = made.cert.der().clone();
    let cert_hash = sha256(&cert);
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(made.signing_key.serialize_der()));
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)?;
    Ok(TlsServer { acceptor: tokio_rustls::TlsAcceptor::from(Arc::new(config)), cert_hash })
}

/// Runs the agent side of step 3. `decide` turns what the client showed into
/// the admission (and counts failures); the client is told the outcome.
pub(super) async fn agent_auth<S: AsyncRead + AsyncWrite + Unpin>(
    ws: &mut WebSocketStream<S>,
    password: &str,
    cert_hash: &Digest,
    decide: impl FnOnce(Presented) -> super::Admission,
) -> anyhow::Result<super::Admission> {
    use super::Admission;
    match recv_auth(ws).await {
        Some(AuthMessage::Hello { .. }) => {}
        other => anyhow::bail!("expected hello, got {:?}", other),
    }
    let agent_nonce = random_nonce();
    send_auth(ws, &AuthMessage::Challenge { nonce: to_hex(&agent_nonce) }).await?;
    let (client_nonce, mac) = match recv_auth(ws).await {
        Some(AuthMessage::Proof { client_nonce, mac }) => (client_nonce, mac),
        other => anyhow::bail!("expected proof, got {:?}", other),
    };
    let Some(client_nonce) = from_hex::<NONCE_LEN>(&client_nonce) else {
        anyhow::bail!("bad client nonce");
    };
    let presented = match mac.as_deref() {
        None | Some("") => Presented::Nothing,
        Some(_) if password.trim().is_empty() => Presented::Wrong,
        Some(tag) => {
            let tag = from_hex::<32>(tag).unwrap_or([0; 32]);
            if mac_ok(CLIENT_LABEL, password, &agent_nonce, &client_nonce, cert_hash, &tag) {
                Presented::Right
            } else {
                Presented::Wrong
            }
        }
    };
    let admission = decide(presented);
    let answer = match admission {
        Admission::Accept { trusted: true, .. } => AuthMessage::Ok {
            trusted: true,
            mac: Some(to_hex(&agent_mac(password, &agent_nonce, &client_nonce, cert_hash))),
        },
        // Click mode with the right password: still asked here, but the
        // client, which holds a password, needs our MAC to go on.
        Admission::Accept { trusted: false, .. } => AuthMessage::Ok {
            trusted: false,
            mac: (presented == Presented::Right)
                .then(|| to_hex(&agent_mac(password, &agent_nonce, &client_nonce, cert_hash))),
        },
        Admission::TooManyAttempts => AuthMessage::Refused { reason: REFUSED_TOO_MANY.into() },
        Admission::Unauthorized | Admission::NotFound => AuthMessage::Refused { reason: REFUSED_UNAUTHORIZED.into() },
    };
    send_auth(ws, &answer).await?;
    Ok(admission)
}

// ---- Client side ----

/// Accepts the agent's self-signed certificate: there is nothing to check it
/// against. The password proof is bound to this certificate's hash, which is
/// what stops a man in the middle. Handshake signatures are still verified,
/// so the peer must own the key of the certificate it shows.
#[derive(Debug)]
struct AcceptAnyCert(Arc<CryptoProvider>);

impl ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

fn client_tls_config() -> Result<Arc<rustls::ClientConfig>, rustls::Error> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyCert(provider)))
        .with_no_client_auth();
    Ok(Arc::new(config))
}

/// The host part of "host:port" / "[v6]:port", for SNI (not checked).
fn server_name(address: &str) -> ServerName<'static> {
    let host = if let Some(rest) = address.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        address.rsplit_once(':').map(|(h, _)| h).unwrap_or(address)
    };
    ServerName::try_from(host.to_string())
        .unwrap_or_else(|_| ServerName::try_from("lunaris-agent").expect("valid DNS name"))
}

/// A direct connection, handshake done (always TLS).
pub struct Connected {
    pub ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    /// The password was accepted and the agent proved it knows it. False:
    /// connected without a password, the person at the computer will be asked.
    pub trusted: bool,
    /// This side's address of the connection.
    pub local_ip: Option<IpAddr>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConnectError {
    Unreachable(String),
    /// TLS failed: an agent from before secure direct connections, or
    /// someone in the way. The password is never sent without TLS.
    NotSecure(String),
    WrongPassword,
    /// With a password: the answer did not carry the agent's proof. Either
    /// the password is wrong, or the peer is not that computer.
    NotProven,
    TooManyAttempts,
    /// No such path: not a Lunaris agent, or one too old.
    NotFound,
    Refused(String),
    Protocol(String),
}

/// Full sentences: the desktop app shows them as they are.
impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::Unreachable(e) => write!(f, "Could not reach the computer ({}).", e),
            ConnectError::NotSecure(e) => write!(
                f,
                "Could not connect to the computer securely (TLS failed: {}). If it runs a Lunaris agent \
                 older than {}, update that agent; the password is not sent without encryption.",
                e,
                env!("CARGO_PKG_VERSION")
            ),
            ConnectError::WrongPassword => write!(f, "Wrong password."),
            ConnectError::NotProven => {
                write!(f, "Wrong password, or this is not the computer you expect (it could not prove it knows the password).")
            }
            ConnectError::TooManyAttempts => write!(f, "Too many wrong passwords: try again in a minute."),
            ConnectError::NotFound => write!(f, "That address is not a Lunaris agent, or its agent is too old (update it)."),
            ConnectError::Refused(e) => write!(f, "The computer refused the connection ({}).", e),
            ConnectError::Protocol(e) => write!(f, "The connection failed: {}.", e),
        }
    }
}

impl std::error::Error for ConnectError {}

fn http_error(e: tungstenite::Error) -> ConnectError {
    match e {
        tungstenite::Error::Http(resp) => match resp.status().as_u16() {
            401 => ConnectError::WrongPassword,
            429 => ConnectError::TooManyAttempts,
            404 => ConnectError::NotFound,
            code => ConnectError::Refused(format!("HTTP {}", code)),
        },
        e => ConnectError::Unreachable(e.to_string()),
    }
}

/// Connects to an agent's direct port: `address` "host:port", `path`
/// "/ws/client" or "/ws/server", `query` extra URL parameters (e.g.
/// "name=..."), never the password. TLS only: when it fails there is no
/// plain fallback, which anyone on the network could force to read the
/// password.
pub async fn connect(address: &str, path: &str, password: &str, query: &str) -> Result<Connected, ConnectError> {
    let query = query.trim_start_matches(['&', '?']);
    let tcp = match tokio::time::timeout(STEP_TIMEOUT, TcpStream::connect(address)).await {
        Ok(Ok(tcp)) => tcp,
        Ok(Err(e)) => return Err(ConnectError::Unreachable(e.to_string())),
        Err(_) => return Err(ConnectError::Unreachable("timed out".into())),
    };
    let _ = tcp.set_nodelay(true);
    let local_ip = tcp.local_addr().ok().map(|a| a.ip());
    let tls_config = client_tls_config().map_err(|e| ConnectError::Protocol(e.to_string()))?;
    let connector = tokio_rustls::TlsConnector::from(tls_config);
    let tls = match tokio::time::timeout(STEP_TIMEOUT, connector.connect(server_name(address), tcp)).await {
        Ok(Ok(tls)) => tls,
        Ok(Err(e)) => {
            warn!("TLS with {} failed ({}): not connecting (an older agent?)", address, e);
            return Err(ConnectError::NotSecure(e.to_string()));
        }
        Err(_) => return Err(ConnectError::NotSecure("timed out".into())),
    };
    let cert_hash = tls
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|certs| certs.first())
        .map(|cert| sha256(cert))
        .ok_or_else(|| ConnectError::Protocol("no certificate".into()))?;
    let sep = if query.is_empty() { "" } else { "?" };
    let url = format!("wss://{}{}{}{}", address, path, sep, query);
    let (mut ws, _) =
        match tokio::time::timeout(STEP_TIMEOUT, tokio_tungstenite::client_async(url, MaybeTlsStream::Rustls(tls))).await {
            Ok(r) => r.map_err(http_error)?,
            Err(_) => return Err(ConnectError::Unreachable("WebSocket: timed out".into())),
        };
    let trusted = match tokio::time::timeout(STEP_TIMEOUT, client_auth(&mut ws, password, &cert_hash)).await {
        Ok(r) => r?,
        Err(_) => return Err(ConnectError::Protocol("no answer to the password proof".into())),
    };
    info!("Connected to {} over TLS ({})", address, if trusted { "password" } else { "to be accepted there" });
    Ok(Connected { ws, trusted, local_ip })
}

/// What the client makes of the agent's answer to its proof; Ok(trusted).
/// With a password, only an answer carrying the agent's valid MAC goes on:
/// anything else (including "ok, untrusted") could come from an impostor
/// with its own certificate.
fn judge_answer(
    answer: Option<AuthMessage>,
    password: &str,
    agent_nonce: &Nonce,
    client_nonce: &Nonce,
    cert_hash: &Digest,
) -> Result<bool, ConnectError> {
    let has_password = !password.trim().is_empty();
    match answer {
        Some(AuthMessage::Refused { reason }) => Err(refusal(&reason)),
        Some(AuthMessage::Ok { trusted, mac }) if has_password => {
            let tag = mac.as_deref().and_then(from_hex::<32>);
            let proven = tag.is_some_and(|t| mac_ok(AGENT_LABEL, password, agent_nonce, client_nonce, cert_hash, &t));
            // Proven but untrusted: an agent in click mode, it asks there.
            if proven { Ok(trusted) } else { Err(ConnectError::NotProven) }
        }
        // No password: click-to-accept, nothing to prove either way.
        Some(AuthMessage::Ok { trusted: false, .. }) => Ok(false),
        Some(AuthMessage::Ok { trusted: true, .. }) => Err(ConnectError::NotProven),
        _ => Err(ConnectError::Protocol("unexpected answer to the password proof".into())),
    }
}

/// Client side of step 3; Ok(trusted).
async fn client_auth<S: AsyncRead + AsyncWrite + Unpin>(
    ws: &mut WebSocketStream<S>,
    password: &str,
    cert_hash: &Digest,
) -> Result<bool, ConnectError> {
    let lost = |e: tungstenite::Error| ConnectError::Unreachable(e.to_string());
    send_auth(ws, &AuthMessage::Hello { version: 1 }).await.map_err(lost)?;
    let agent_nonce = match recv_auth(ws).await {
        Some(AuthMessage::Challenge { nonce }) => from_hex::<NONCE_LEN>(&nonce),
        Some(AuthMessage::Refused { reason }) => return Err(refusal(&reason)),
        _ => None,
    }
    .ok_or_else(|| ConnectError::Protocol("unexpected answer (not a Lunaris agent?)".into()))?;
    let client_nonce = random_nonce();
    let has_password = !password.trim().is_empty();
    let mac = has_password.then(|| to_hex(&client_mac(password, &agent_nonce, &client_nonce, cert_hash)));
    send_auth(ws, &AuthMessage::Proof { client_nonce: to_hex(&client_nonce), mac }).await.map_err(lost)?;
    judge_answer(recv_auth(ws).await, password, &agent_nonce, &client_nonce, cert_hash)
}

fn refusal(reason: &str) -> ConnectError {
    match reason {
        REFUSED_UNAUTHORIZED => ConnectError::WrongPassword,
        REFUSED_TOO_MANY => ConnectError::TooManyAttempts,
        other => ConnectError::Refused(other.chars().filter(|c| !c.is_control()).take(100).collect()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The same vectors are in server/src/links.rs; computed independently
    // (Python hmac/hashlib).
    const PASSWORD: &str = "abcd-efgh-ijkl";
    const CERT_HASH_HEX: &str = "fc105ce3018116bb697b777e09fd7ce7039e628a31ad7aab6c9bbe02c3bf0d1d";
    const CLIENT_MAC_HEX: &str = "5443f1e19d8c91e2256950e29b0476d5f90f9e9bba6ee2d799e271af115739cd";
    const AGENT_MAC_HEX: &str = "a389673baba901cf246ec3b4b83db77a97e466384fc8c76897ddd2d3abe497e8";

    #[test]
    fn mac_test_vectors() {
        let cert_hash = sha256(b"lunaris test certificate");
        assert_eq!(to_hex(&cert_hash), CERT_HASH_HEX);
        let (an, cn) = ([0x11u8; 32], [0x22u8; 32]);
        assert_eq!(to_hex(&client_mac(PASSWORD, &an, &cn, &cert_hash)), CLIENT_MAC_HEX);
        assert_eq!(to_hex(&agent_mac(PASSWORD, &an, &cn, &cert_hash)), AGENT_MAC_HEX);
        // The password is trimmed, like everywhere else.
        assert_eq!(to_hex(&client_mac(" abcd-efgh-ijkl\n", &an, &cn, &cert_hash)), CLIENT_MAC_HEX);
        let tag = from_hex::<32>(CLIENT_MAC_HEX).unwrap();
        assert!(mac_ok(CLIENT_LABEL, PASSWORD, &an, &cn, &cert_hash, &tag));
        assert!(!mac_ok(AGENT_LABEL, PASSWORD, &an, &cn, &cert_hash, &tag));
        // Another certificate (a man in the middle) gives another MAC.
        assert!(!mac_ok(CLIENT_LABEL, PASSWORD, &an, &cn, &sha256(b"other"), &tag));
    }

    #[test]
    fn message_test_vectors() {
        let json = |m: &AuthMessage| serde_json::to_string(m).unwrap();
        assert_eq!(json(&AuthMessage::Hello { version: 1 }), r#"{"auth":"hello","version":1}"#);
        assert_eq!(json(&AuthMessage::Challenge { nonce: "ab".into() }), r#"{"auth":"challenge","nonce":"ab"}"#);
        assert_eq!(
            json(&AuthMessage::Proof { client_nonce: "cd".into(), mac: Some("ef".into()) }),
            r#"{"auth":"proof","client_nonce":"cd","mac":"ef"}"#
        );
        assert_eq!(json(&AuthMessage::Proof { client_nonce: "cd".into(), mac: None }), r#"{"auth":"proof","client_nonce":"cd"}"#);
        assert_eq!(json(&AuthMessage::Ok { trusted: false, mac: None }), r#"{"auth":"ok","trusted":false}"#);
        assert_eq!(
            json(&AuthMessage::Refused { reason: REFUSED_TOO_MANY.into() }),
            r#"{"auth":"refused","reason":"too_many_attempts"}"#
        );
    }

    #[test]
    fn with_a_password_only_the_agents_valid_mac_is_accepted() {
        let (an, cn, ch) = ([1u8; 32], [2u8; 32], sha256(b"cert"));
        let good = Some(to_hex(&agent_mac("pw", &an, &cn, &ch)));
        let ok = |trusted: bool, mac: Option<String>| Some(AuthMessage::Ok { trusted, mac });
        assert_eq!(judge_answer(ok(true, good.clone()), "pw", &an, &cn, &ch), Ok(true));
        // A click-mode agent that proved it knows the password: asked there.
        assert_eq!(judge_answer(ok(false, good.clone()), "pw", &an, &cn, &ch), Ok(false));
        // An impostor: no MAC, a wrong one, or one for another certificate.
        assert_eq!(judge_answer(ok(false, None), "pw", &an, &cn, &ch), Err(ConnectError::NotProven));
        assert_eq!(judge_answer(ok(true, None), "pw", &an, &cn, &ch), Err(ConnectError::NotProven));
        let client_side = Some(to_hex(&client_mac("pw", &an, &cn, &ch)));
        assert_eq!(judge_answer(ok(true, client_side), "pw", &an, &cn, &ch), Err(ConnectError::NotProven));
        let other_cert = Some(to_hex(&agent_mac("pw", &an, &cn, &sha256(b"other"))));
        assert_eq!(judge_answer(ok(true, other_cert), "pw", &an, &cn, &ch), Err(ConnectError::NotProven));
        // Without a password: click-to-accept as before, "trusted" can't be.
        assert_eq!(judge_answer(ok(false, None), "", &an, &cn, &ch), Ok(false));
        assert_eq!(judge_answer(ok(true, good), "", &an, &cn, &ch), Err(ConnectError::NotProven));
        let refused = Some(AuthMessage::Refused { reason: REFUSED_UNAUTHORIZED.into() });
        assert_eq!(judge_answer(refused, "pw", &an, &cn, &ch), Err(ConnectError::WrongPassword));
    }

    #[test]
    fn hex_round_trip_and_rejects() {
        let n = random_nonce();
        assert_eq!(from_hex::<32>(&to_hex(&n)), Some(n));
        assert_ne!(random_nonce(), n);
        assert_eq!(from_hex::<2>("+f00"), None);
        assert_eq!(from_hex::<2>("abc"), None);
        assert_eq!(from_hex::<1>("zz"), None);
    }

    #[test]
    fn digests_compare_equal_only_when_equal() {
        let a = sha256(b"a");
        assert!(digest_eq(&a, &sha256(b"a")));
        assert!(!digest_eq(&a, &sha256(b"b")));
    }

    #[test]
    fn server_names() {
        assert_eq!(server_name("192.168.1.2:8091"), ServerName::try_from("192.168.1.2").unwrap());
        assert_eq!(server_name("[fe80::1]:8091"), ServerName::try_from("fe80::1").unwrap());
        assert_eq!(server_name("pc.lan:8091"), ServerName::try_from("pc.lan").unwrap());
    }
}
