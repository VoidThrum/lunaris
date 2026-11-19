//! The HTTP side of GameStream: the client's identity (key and certificate),
//! the small HTTP/1.1 client (plain to the HTTP port, TLS with the client
//! certificate to the HTTPS port), the XML answers, and the requests
//! `serverinfo`, `applist`, `launch`, `resume`, `cancel`.
//!
//! Ported from moonlight-embedded `libgamestream` (`client.c`, `http.c`,
//! `mkcert.c`, `xml.c`), with moonlight-qt `app/backend/nvhttp.cpp` read for the
//! places where today's Moonlight differs, and Sunshine `src/nvhttp.cpp` /
//! `src/crypto.cpp` read for what the host expects. Each ported piece names its
//! source.
//!
//! Rules kept here: the texts of [`Error`] are for the person at the screen and
//! never contain the host's address; log lines never contain the PIN, a key, a
//! certificate, a salt or a query string (query strings carry all of these).

use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::rustls;

/// The port GameStream hosts listen on for plain HTTP (libgamestream/client.c gs_init).
pub const DEFAULT_HTTP_PORT: u16 = 47989;
/// The HTTPS port used when `serverinfo` does not say (libgamestream/client.c load_serverinfo).
pub const DEFAULT_HTTPS_PORT: u16 = 47984;

/// Time-out of an ordinary request (moonlight-qt nvhttp.cpp REQUEST_TIMEOUT_MS).
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Time-out of `launch`: the host may change its display mode and probe its
/// encoders before it answers (moonlight-qt nvhttp.cpp LAUNCH_TIMEOUT_MS).
pub const LAUNCH_TIMEOUT: Duration = Duration::from_secs(120);
/// Time-out of `resume` (moonlight-qt nvhttp.cpp RESUME_TIMEOUT_MS).
pub const RESUME_TIMEOUT: Duration = Duration::from_secs(30);
/// Time-out of `cancel` (moonlight-qt nvhttp.cpp QUIT_TIMEOUT_MS).
pub const QUIT_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest wait for the TCP connection itself.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Largest answer accepted (the application list of a big library is a few hundred kB at most).
const MAX_ANSWER: usize = 8 * 1024 * 1024;
/// The longest answer taken over plain HTTP, where anybody on the way can
/// answer: `serverinfo` and the stages of pairing are a few kilobytes (the
/// largest carries one certificate in hex).
const MAX_PLAIN_ANSWER: usize = 64 * 1024;
/// The most elements one answer may have (an application list has four or
/// five per application).
const MAX_XML_NODES: usize = 20_000;

// ServerCodecModeSupport bits (moonlight-common-c src/Limelight.h SCM_*).
pub const SCM_H264: u32 = 0x0000_0001;
pub const SCM_HEVC: u32 = 0x0000_0100;
pub const SCM_HEVC_MAIN10: u32 = 0x0000_0200;
pub const SCM_AV1_MAIN8: u32 = 0x0001_0000;
pub const SCM_AV1_MAIN10: u32 = 0x0002_0000;
pub const SCM_H264_HIGH8_444: u32 = 0x0004_0000;
pub const SCM_HEVC_REXT8_444: u32 = 0x0008_0000;
pub const SCM_HEVC_REXT10_444: u32 = 0x0010_0000;
pub const SCM_AV1_HIGH8_444: u32 = 0x0020_0000;
pub const SCM_AV1_HIGH10_444: u32 = 0x0040_0000;

/// `surroundAudioInfo` for stereo: channel mask 0x3 in the high half, 2 channels
/// in the low half (moonlight-common-c src/Limelight.h
/// SURROUNDAUDIOINFO_FROM_AUDIO_CONFIGURATION(AUDIO_CONFIGURATION_STEREO); it is
/// also Sunshine's default, nvhttp.cpp make_launch_session "196610").
pub const SURROUND_AUDIO_INFO_STEREO: u32 = (0x3 << 16) | 2;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// What went wrong, worded for the person at the screen (`Display`). The host's
/// address is never part of the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// No connection could be made.
    Unreachable,
    /// The host did not answer in time.
    Timeout,
    /// The connection ended before the answer was complete.
    ConnectionLost,
    /// The host does not know this client's certificate (status 401), or no
    /// host certificate is known yet (never paired).
    NotPaired,
    /// The host showed another certificate than the one kept from pairing.
    HostChanged,
    /// Pairing: the PIN typed at the host was not the one shown here.
    PinRejected,
    /// Pairing: nobody entered the PIN in time.
    PairingTimeout,
    /// Pairing: the request was turned down at the host.
    PairingRefused,
    /// Pairing: the host is already in a pairing with this client.
    PairingInProgress,
    /// Pairing failed for a reason the host did not explain.
    PairingFailed,
    /// The host is in a session that this client may not take over.
    Busy,
    /// The host could not start capturing or encoding its screen.
    CaptureFailed,
    /// The host insists on encryption that the request did not offer.
    EncryptionRequired,
    /// The application asked for does not exist on the host.
    AppNotFound,
    /// The host refused with a status and a message of its own.
    Host { code: u32, message: String },
    /// The answer could not be understood (the detail is for logs, `Debug`).
    Protocol(&'static str),
    /// The stored identity (key, certificate) cannot be used.
    Identity(&'static str),
    /// A fault on this side (random numbers, key generation).
    Internal(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Unreachable => f.write_str("Could not reach the computer"),
            Error::Timeout => f.write_str("The computer did not answer in time"),
            Error::ConnectionLost => f.write_str("The connection to the computer was lost"),
            Error::NotPaired => {
                f.write_str("This computer does not accept the pairing. Pair with it again.")
            }
            Error::HostChanged => f.write_str(
                "The computer that answers at this address is not the one that was paired (it shows another certificate). Nothing was sent to it. If Sunshine was reinstalled there, remove this computer and add it again.",
            ),
            Error::PinRejected => f.write_str("The PIN was not accepted"),
            Error::PairingTimeout => f.write_str("Pairing was not completed in time"),
            Error::PairingRefused => f.write_str("Pairing was refused at the other computer"),
            Error::PairingInProgress => f.write_str(
                "The computer is already in a pairing with this program. Wait a few minutes and try again.",
            ),
            Error::PairingFailed => f.write_str("Pairing failed"),
            Error::Busy => f.write_str("This computer is already streaming to another client"),
            Error::CaptureFailed => f.write_str(
                "The computer could not start capturing its screen. Is a display connected and turned on?",
            ),
            Error::EncryptionRequired => {
                f.write_str("The computer requires encryption that was not offered")
            }
            Error::AppNotFound => f.write_str("The application was not found on the computer"),
            Error::Host { code, message } => {
                if message.is_empty() {
                    write!(f, "The computer refused the request (status {code})")
                } else {
                    write!(f, "The computer refused the request: {message} (status {code})")
                }
            }
            Error::Protocol(_) => {
                f.write_str("The computer gave an answer that could not be understood")
            }
            Error::Identity(_) => f.write_str("The stored pairing data cannot be used. Pair again."),
            Error::Internal(_) => f.write_str("Something went wrong inside the program"),
        }
    }
}

impl std::error::Error for Error {}

// ---------------------------------------------------------------------------
// Small helpers: hex, random numbers, PEM, DER
// ---------------------------------------------------------------------------

/// Lower-case hex (libgamestream/client.c bytes_to_hex).
pub(super) fn hex_encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 15) as usize] as char);
    }
    out
}

/// Hex to bytes; `None` when the text is not hex of whole bytes
/// (libgamestream/client.c hex_to_bytes, which does not check).
pub(super) fn hex_decode(text: &str) -> Option<Vec<u8>> {
    let t = text.trim().as_bytes();
    if t.len() % 2 != 0 {
        return None;
    }
    let digit = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    let mut out = Vec::with_capacity(t.len() / 2);
    for pair in t.chunks(2) {
        out.push(digit(pair[0])? << 4 | digit(pair[1])?);
    }
    Some(out)
}

/// Random bytes from the system (the reference uses OpenSSL RAND_bytes).
pub(super) fn random_bytes<const N: usize>() -> Result<[u8; N], Error> {
    use ring::rand::SecureRandom;
    let mut out = [0u8; N];
    ring::rand::SystemRandom::new()
        .fill(&mut out)
        .map_err(|_| Error::Internal("no random numbers"))?;
    Ok(out)
}

/// PEM text as OpenSSL writes it: base64 in lines of 64 characters.
pub(super) fn pem_encode(label: &str, der: &[u8]) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(der);
    let mut out = format!("-----BEGIN {label}-----\n");
    for line in b64.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(line).unwrap_or_default());
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

/// The first PEM block of a text: its label and its bytes.
pub(super) fn pem_decode(pem: &str) -> Option<(String, Vec<u8>)> {
    let begin = pem.find("-----BEGIN ")?;
    let rest = &pem[begin + 11..];
    let label_end = rest.find("-----")?;
    let label = rest[..label_end].to_string();
    let body = &rest[label_end + 5..];
    let end = body.find(&format!("-----END {label}-----"))?;
    let b64: String = body[..end].chars().filter(|c| !c.is_whitespace()).collect();
    let der = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    Some((label, der))
}

/// The bytes of a PEM certificate.
pub(super) fn cert_der_from_pem(pem: &str) -> Option<Vec<u8>> {
    match pem_decode(pem) {
        Some((label, der)) if label == "CERTIFICATE" => Some(der),
        _ => None,
    }
}

fn der_tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    let n = content.len();
    if n < 0x80 {
        out.push(n as u8);
    } else {
        let bytes = n.to_be_bytes();
        let skip = bytes.iter().take_while(|b| **b == 0).count();
        out.push(0x80 | (bytes.len() - skip) as u8);
        out.extend_from_slice(&bytes[skip..]);
    }
    out.extend_from_slice(content);
    out
}

/// Reads one DER element: (tag, content, the whole element, what follows).
fn der_read(data: &[u8]) -> Option<(u8, &[u8], &[u8], &[u8])> {
    let tag = *data.first()?;
    let first = *data.get(1)? as usize;
    let (len, header) = if first < 0x80 {
        (first, 2)
    } else {
        let count = first & 0x7f;
        if count == 0 || count > 4 {
            return None;
        }
        let mut len = 0usize;
        for i in 0..count {
            len = len << 8 | *data.get(2 + i)? as usize;
        }
        (len, 2 + count)
    };
    let end = header.checked_add(len)?;
    if end > data.len() {
        return None;
    }
    Some((tag, &data[header..end], &data[..end], &data[end..]))
}

/// The parts of an X.509 certificate that pairing and the fake host of the
/// tests need.
pub(super) struct CertParts {
    /// The signature on the certificate (the BIT STRING's bytes), which both
    /// sides mix into the pairing hashes (libgamestream/client.c gs_pair
    /// X509_get0_signature; Sunshine crypto.cpp signature()).
    pub signature: Vec<u8>,
    /// The RSA public key (`RSAPublicKey`, PKCS#1) out of SubjectPublicKeyInfo.
    pub rsa_public_key: Vec<u8>,
    /// The signed part (TBSCertificate), whole.
    pub tbs: Vec<u8>,
}

pub(super) fn cert_parts(der: &[u8]) -> Option<CertParts> {
    let (tag, cert, _, _) = der_read(der)?;
    if tag != 0x30 {
        return None;
    }
    let (tag, tbs, tbs_raw, rest) = der_read(cert)?;
    if tag != 0x30 {
        return None;
    }
    let (_, _, _, rest) = der_read(rest)?; // signatureAlgorithm
    let (tag, sig, _, _) = der_read(rest)?;
    if tag != 0x03 || sig.is_empty() {
        return None;
    }
    // TBSCertificate: [0] version (optional), serial, signature, issuer,
    // validity, subject, subjectPublicKeyInfo, ...
    let mut inner = tbs;
    let (tag, _, _, after) = der_read(inner)?;
    if tag == 0xa0 {
        inner = after;
    }
    for _ in 0..5 {
        let (_, _, _, after) = der_read(inner)?;
        inner = after;
    }
    let (tag, spki, _, _) = der_read(inner)?;
    if tag != 0x30 {
        return None;
    }
    let (_, _, _, rest) = der_read(spki)?; // algorithm
    let (tag, key_bits, _, _) = der_read(rest)?;
    if tag != 0x03 || key_bits.is_empty() {
        return None;
    }
    Some(CertParts {
        signature: sig[1..].to_vec(),
        rsa_public_key: key_bits[1..].to_vec(),
        tbs: tbs_raw.to_vec(),
    })
}

/// SHA-256 with RSA (PKCS#1 v1.5) signature with a PEM private key
/// (libgamestream/client.c sign_it: EVP_DigestSign with EVP_sha256).
pub(super) fn rsa_sign_sha256(key_pem: &str, message: &[u8]) -> Result<Vec<u8>, Error> {
    let (label, der) = pem_decode(key_pem).ok_or(Error::Identity("key is not PEM"))?;
    let key = match label.as_str() {
        "PRIVATE KEY" => ring::rsa::KeyPair::from_pkcs8(&der),
        "RSA PRIVATE KEY" => ring::rsa::KeyPair::from_der(&der),
        _ => return Err(Error::Identity("key is of an unknown kind")),
    }
    .map_err(|_| Error::Identity("key cannot be read"))?;
    let mut signature = vec![0u8; key.public().modulus_len()];
    key.sign(
        &ring::signature::RSA_PKCS1_SHA256,
        &ring::rand::SystemRandom::new(),
        message,
        &mut signature,
    )
    .map_err(|_| Error::Internal("signing failed"))?;
    Ok(signature)
}

/// Checks a SHA-256 with RSA signature against the key of a PEM certificate
/// (libgamestream/client.c verifySignature).
pub(super) fn rsa_verify_sha256(cert_pem: &str, message: &[u8], signature: &[u8]) -> bool {
    let Some(parts) = cert_der_from_pem(cert_pem).and_then(|der| cert_parts(&der)) else {
        return false;
    };
    ring::signature::UnparsedPublicKey::new(
        &ring::signature::RSA_PKCS1_2048_8192_SHA256,
        &parts.rsa_public_key,
    )
    .verify(message, signature)
    .is_ok()
}

/// Whether a certificate carries a valid signature made with its own key: what
/// the host's OpenSSL checks when the client shows it (Sunshine crypto.cpp
/// cert_chain_t::verify, X509_verify_cert with the certificate as its own
/// trust anchor; dates are ignored there by openssl_verify_cb).
pub(super) fn cert_is_self_signed(cert_pem: &str) -> bool {
    let Some(parts) = cert_der_from_pem(cert_pem).and_then(|der| cert_parts(&der)) else {
        return false;
    };
    rsa_verify_sha256(cert_pem, &parts.tbs, &parts.signature)
}

/// (year, month, day) of a day counted from 1970-01-01.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

/// An X.509 time: UTCTime up to 2049, GeneralizedTime after (RFC 5280 4.1.2.5).
fn der_time(unix: u64) -> Vec<u8> {
    let (year, month, day) = civil_from_days((unix / 86_400) as i64);
    let s = unix % 86_400;
    let clock = format!("{:02}{:02}{:02}{:02}{:02}Z", month, day, s / 3600, s % 3600 / 60, s % 60);
    if (1950..2050).contains(&year) {
        der_tlv(0x17, format!("{:02}{}", year % 100, clock).as_bytes())
    } else {
        der_tlv(0x18, format!("{:04}{}", year, clock).as_bytes())
    }
}

/// The system's random numbers in the shape the `rsa` crate asks for.
struct SystemRng;

#[derive(Debug)]
struct SystemRngError;

impl fmt::Display for SystemRngError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("no random numbers")
    }
}

impl std::error::Error for SystemRngError {}

impl rsa::rand_core::TryRng for SystemRng {
    type Error = SystemRngError;

    fn try_next_u32(&mut self) -> Result<u32, Self::Error> {
        let mut b = [0u8; 4];
        self.try_fill_bytes(&mut b)?;
        Ok(u32::from_le_bytes(b))
    }

    fn try_next_u64(&mut self) -> Result<u64, Self::Error> {
        let mut b = [0u8; 8];
        self.try_fill_bytes(&mut b)?;
        Ok(u64::from_le_bytes(b))
    }

    fn try_fill_bytes(&mut self, dst: &mut [u8]) -> Result<(), Self::Error> {
        use ring::rand::SecureRandom;
        ring::rand::SystemRandom::new().fill(dst).map_err(|_| SystemRngError)
    }
}

impl rsa::rand_core::TryCryptoRng for SystemRng {}

/// A new RSA-2048 key with a self-signed certificate: `(certificate PEM, key PEM)`.
///
/// The certificate is what moonlight-embedded libgamestream/mkcert.c mkcert()
/// makes: X.509 v3, the given serial number (0 there), valid from now for
/// `years` of 365 days, subject = issuer = `CN=<common_name>`, signed with the
/// key itself, SHA-256 with RSA. The key is written as PKCS#8 ("BEGIN PRIVATE
/// KEY"), as PEM_write_PrivateKey does. The host checks only that the
/// signature holds and that the certificate is byte for byte the one it stored
/// at pairing (Sunshine crypto.cpp cert_chain_t::verify, nvhttp.cpp
/// verify_client_certificate), and takes the signature's bytes into the
/// pairing hash.
pub(super) fn make_certificate(
    common_name: &str,
    serial: &[u8],
    years: u64,
) -> Result<(String, String), Error> {
    use rsa::pkcs8::{EncodePrivateKey, EncodePublicKey};

    let key = // rsa 0.10.0-rc.18 takes an infallible generator: the system's random
    // source not answering ends the program here, as it would in ring.
    rsa::RsaPrivateKey::new(&mut rsa::rand_core::UnwrapErr(SystemRng), 2048)
        .map_err(|_| Error::Internal("key generation failed"))?;
    let pkcs8 = key.to_pkcs8_der().map_err(|_| Error::Internal("key encoding failed"))?;
    let spki = key
        .to_public_key()
        .to_public_key_der()
        .map_err(|_| Error::Internal("key encoding failed"))?;
    let key_pem = pem_encode("PRIVATE KEY", pkcs8.as_bytes());

    // sha256WithRSAEncryption, 1.2.840.113549.1.1.11, with NULL parameters.
    let algorithm = der_tlv(
        0x30,
        &[
            der_tlv(0x06, &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b]),
            der_tlv(0x05, &[]),
        ]
        .concat(),
    );
    // Name: one commonName (2.5.4.3) as UTF8String, as OpenSSL writes it.
    let name = der_tlv(
        0x30,
        &der_tlv(
            0x31,
            &der_tlv(
                0x30,
                &[der_tlv(0x06, &[0x55, 0x04, 0x03]), der_tlv(0x0c, common_name.as_bytes())]
                    .concat(),
            ),
        ),
    );
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let validity =
        der_tlv(0x30, &[der_time(now), der_time(now + 60 * 60 * 24 * 365 * years)].concat());
    // A positive INTEGER: no needless leading zeros, a zero in front when the top bit is set.
    let mut serial_bytes: Vec<u8> = serial.iter().copied().skip_while(|b| *b == 0).collect();
    if serial_bytes.first().map_or(true, |b| b & 0x80 != 0) {
        serial_bytes.insert(0, 0);
    }
    let tbs = der_tlv(
        0x30,
        &[
            der_tlv(0xa0, &der_tlv(0x02, &[2])), // version 3 (X509_set_version(cert, 2))
            der_tlv(0x02, &serial_bytes),
            algorithm.clone(),
            name.clone(),
            validity,
            name,
            spki.as_bytes().to_vec(),
        ]
        .concat(),
    );
    let signature = rsa_sign_sha256(&key_pem, &tbs)?;
    let mut bits = vec![0u8];
    bits.extend_from_slice(&signature);
    let cert = der_tlv(0x30, &[tbs, algorithm, der_tlv(0x03, &bits)].concat());
    Ok((pem_encode("CERTIFICATE", &cert), key_pem))
}

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

/// What this client is to a host: made once, shown at pairing, kept with the
/// source. None of it may be logged.
#[derive(Clone, PartialEq, Eq)]
pub struct Identity {
    /// The client's certificate, PEM.
    pub client_cert_pem: String,
    /// The client's RSA key, PEM (PKCS#8; PKCS#1 "RSA PRIVATE KEY" is read too).
    pub client_key_pem: String,
    /// 16 hex digits, sent as `uniqueid` with every request.
    pub unique_id: String,
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never the key or the certificate.
        f.debug_struct("Identity").finish_non_exhaustive()
    }
}

impl Identity {
    /// A new identity: an RSA-2048 key, the certificate of
    /// moonlight-embedded libgamestream/mkcert.c (CN "NVIDIA GameStream
    /// Client", serial 0, ten years, SHA-256 with RSA), and a random unique id
    /// of 16 hex digits (libgamestream/client.c UNIQUEID_CHARS; the reference
    /// writes the fixed "0123456789ABCDEF", and Sunshine keys its pairing
    /// sessions by this id, nvhttp.cpp map_id_sess, so a random one keeps two
    /// clients from colliding).
    ///
    /// Generating the key takes from a fraction of a second to a few seconds of
    /// CPU: call it from `tokio::task::spawn_blocking` when on a runtime.
    pub fn generate() -> Result<Identity, Error> {
        let (client_cert_pem, client_key_pem) =
            make_certificate("NVIDIA GameStream Client", &[0], 10)?;
        let unique_id = hex_encode(&random_bytes::<8>()?).to_uppercase();
        Ok(Identity { client_cert_pem, client_key_pem, unique_id })
    }

    /// Checks that the texts are a certificate and a key that can be used.
    pub fn check(&self) -> Result<(), Error> {
        // The host takes only a certificate whose own signature holds.
        if !cert_is_self_signed(&self.client_cert_pem) {
            return Err(Error::Identity("certificate cannot be read or is not self-signed"));
        }
        // The key must be the certificate's.
        let signature = rsa_sign_sha256(&self.client_key_pem, b"check")?;
        if !rsa_verify_sha256(&self.client_cert_pem, b"check", &signature) {
            return Err(Error::Identity("key does not fit the certificate"));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// XML
// ---------------------------------------------------------------------------

/// One element of an answer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct XmlNode {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    /// The element's own text (entities resolved), without that of its children.
    pub text: String,
    pub children: Vec<XmlNode>,
}

impl XmlNode {
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    /// The first element of this name, this one included, searched depth first
    /// (libgamestream/xml.c xml_search looks at any depth too).
    pub fn find(&self, name: &str) -> Option<&XmlNode> {
        if self.name == name {
            return Some(self);
        }
        self.children.iter().find_map(|c| c.find(name))
    }

    /// The text of the first element of this name; empty when there is none
    /// (libgamestream/xml.c xml_search gives an empty string then).
    pub fn text_of(&self, name: &str) -> &str {
        self.find(name).map(|n| n.text.as_str()).unwrap_or("")
    }

    fn collect<'a>(&'a self, name: &str, out: &mut Vec<&'a XmlNode>) {
        if self.name == name {
            out.push(self);
        }
        for c in &self.children {
            c.collect(name, out);
        }
    }

    /// All elements of this name at any depth, in document order.
    pub fn find_all(&self, name: &str) -> Vec<&XmlNode> {
        let mut out = Vec::new();
        self.collect(name, &mut out);
        out
    }
}

struct XmlParser<'a> {
    s: &'a [u8],
    pos: usize,
    /// Elements begun so far (`MAX_XML_NODES`).
    nodes: usize,
}

fn xml_unescape(raw: &str) -> String {
    if !raw.contains('&') {
        return raw.to_string();
    }
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let Some(semi) = rest.find(';').filter(|i| *i <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..semi];
        let ch = match entity {
            "lt" => Some('<'),
            "gt" => Some('>'),
            "amp" => Some('&'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => {
                let code = if let Some(hex) = entity.strip_prefix("#x") {
                    u32::from_str_radix(hex, 16).ok()
                } else if let Some(dec) = entity.strip_prefix('#') {
                    dec.parse().ok()
                } else {
                    None
                };
                code.and_then(char::from_u32)
            }
        };
        match ch {
            Some(c) => {
                out.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

impl<'a> XmlParser<'a> {
    fn starts_with(&self, what: &str) -> bool {
        self.s[self.pos..].starts_with(what.as_bytes())
    }

    fn skip_past(&mut self, what: &str) -> Option<()> {
        let w = what.as_bytes();
        let at = self.s[self.pos..].windows(w.len()).position(|x| x == w)?;
        self.pos += at + w.len();
        Some(())
    }

    fn skip_space(&mut self) {
        while self.pos < self.s.len() && self.s[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }

    /// Skips white space, the XML declaration, comments and a DOCTYPE.
    fn skip_misc(&mut self) -> Option<()> {
        loop {
            self.skip_space();
            if self.starts_with("<?") {
                self.skip_past("?>")?;
            } else if self.starts_with("<!--") {
                self.skip_past("-->")?;
            } else if self.starts_with("<!") {
                self.skip_past(">")?;
            } else {
                return Some(());
            }
        }
    }

    fn name(&mut self) -> Option<String> {
        let start = self.pos;
        while self.pos < self.s.len() {
            let c = self.s[self.pos];
            if c.is_ascii_whitespace() || matches!(c, b'>' | b'/' | b'=' | b'<') {
                break;
            }
            self.pos += 1;
        }
        if self.pos == start {
            return None;
        }
        std::str::from_utf8(&self.s[start..self.pos]).ok().map(str::to_string)
    }

    fn text_until(&mut self, stop: u8) -> Option<&'a str> {
        let start = self.pos;
        while self.pos < self.s.len() && self.s[self.pos] != stop {
            self.pos += 1;
        }
        std::str::from_utf8(&self.s[start..self.pos]).ok()
    }

    fn element(&mut self, depth: usize) -> Option<XmlNode> {
        self.nodes += 1;
        if depth > 32 || self.nodes > MAX_XML_NODES || !self.starts_with("<") {
            return None;
        }
        self.pos += 1;
        let mut node = XmlNode { name: self.name()?, ..Default::default() };
        loop {
            self.skip_space();
            match *self.s.get(self.pos)? {
                b'/' => {
                    self.pos += 1;
                    if *self.s.get(self.pos)? != b'>' {
                        return None;
                    }
                    self.pos += 1;
                    return Some(node);
                }
                b'>' => {
                    self.pos += 1;
                    break;
                }
                _ => {
                    let key = self.name()?;
                    self.skip_space();
                    if *self.s.get(self.pos)? != b'=' {
                        return None;
                    }
                    self.pos += 1;
                    self.skip_space();
                    let quote = *self.s.get(self.pos)?;
                    if quote != b'"' && quote != b'\'' {
                        return None;
                    }
                    self.pos += 1;
                    let value = xml_unescape(self.text_until(quote)?);
                    if self.pos >= self.s.len() {
                        return None;
                    }
                    self.pos += 1;
                    node.attrs.push((key, value));
                }
            }
        }
        loop {
            let text = self.text_until(b'<')?;
            node.text.push_str(&xml_unescape(text));
            if self.pos >= self.s.len() {
                return None;
            }
            if self.starts_with("</") {
                self.pos += 2;
                if self.name()? != node.name {
                    return None;
                }
                self.skip_space();
                if *self.s.get(self.pos)? != b'>' {
                    return None;
                }
                self.pos += 1;
                return Some(node);
            } else if self.starts_with("<!--") {
                self.skip_past("-->")?;
            } else if self.starts_with("<![CDATA[") {
                self.pos += 9;
                let start = self.pos;
                self.skip_past("]]>")?;
                node.text.push_str(std::str::from_utf8(&self.s[start..self.pos - 3]).ok()?);
            } else if self.starts_with("<?") {
                self.skip_past("?>")?;
            } else {
                node.children.push(self.element(depth + 1)?);
            }
        }
    }
}

/// Parses an answer into its root element.
pub fn parse_xml(text: &str) -> Result<XmlNode, Error> {
    let mut p = XmlParser { s: text.as_bytes(), pos: 0, nodes: 0 };
    p.skip_misc().ok_or(Error::Protocol("XML is cut off"))?;
    let root = p.element(0).ok_or(Error::Protocol("XML is malformed"))?;
    Ok(root)
}

/// The status of an answer: `status_code` and `status_message` of `<root>`
/// (libgamestream/xml.c xml_status and _xml_start_status_element: anything but
/// 200, or another root element, is an error).
pub fn xml_status(root: &XmlNode) -> (u32, String) {
    if root.name != "root" {
        return (0, String::new());
    }
    let code = root.attr("status_code").and_then(|c| c.trim().parse().ok()).unwrap_or(0);
    (code, root.attr("status_message").unwrap_or("").to_string())
}

/// The host's own message, made safe to show: one line, printable, short.
pub(super) fn tidy_message(message: &str) -> String {
    let mut out: String = message
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(200)
        .collect();
    out = out.trim().to_string();
    out
}

/// Turns a status other than 200 into an error. 401 is what Sunshine answers
/// over HTTPS to a certificate it does not know (nvhttp.cpp on_verify_failed).
pub(super) fn check_status(root: &XmlNode) -> Result<(), Error> {
    match xml_status(root) {
        (200, _) => Ok(()),
        (401, _) => Err(Error::NotPaired),
        (0, _) => Err(Error::Protocol("answer without status")),
        (code, message) => Err(Error::Host { code, message: tidy_message(&message) }),
    }
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

/// Percent-encodes a query value.
pub(super) fn url_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// The longest line that may announce a chunk (its size in hex, and
/// extensions nobody here reads).
const MAX_CHUNK_LINE: usize = 256;

/// Decodes the chunks of `data` from `*pos` on into `out`, as far as they
/// are complete, and moves `*pos` behind them: a body that arrives in parts
/// is gone through once. `Ok(true)` when the last chunk was seen. Nothing a
/// peer writes makes this read outside `data` or keep more than `limit`
/// bytes: the size it declares is checked before anything is added to it.
fn decode_chunked_step(data: &[u8], pos: &mut usize, out: &mut Vec<u8>, limit: usize) -> Result<bool, &'static str> {
    loop {
        let rest = data.get(*pos..).ok_or("chunked body is malformed")?;
        let Some(line_end) = find(rest, b"\r\n") else {
            if rest.len() > MAX_CHUNK_LINE {
                return Err("chunk size line too long");
            }
            return Ok(false);
        };
        if line_end > MAX_CHUNK_LINE {
            return Err("chunk size line too long");
        }
        let line = std::str::from_utf8(&rest[..line_end]).map_err(|_| "chunk size is not text")?;
        let size = usize::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16).map_err(|_| "chunk size is not a number")?;
        if size == 0 {
            return Ok(true);
        }
        if size > limit.saturating_sub(out.len()) {
            return Err("answer too long");
        }
        // Bounded by `limit` now: no sum below can overflow.
        let start = line_end + 2;
        let Some(chunk) = rest.get(start..start + size) else { return Ok(false) };
        if rest.len() < start + size + 2 {
            return Ok(false);
        }
        out.extend_from_slice(chunk);
        *pos += start + size + 2;
    }
}

/// Decodes a whole chunked body; `None` while it is not complete or when it
/// is malformed.
#[cfg(test)]
fn decode_chunked(data: &[u8]) -> Option<Vec<u8>> {
    let (mut pos, mut out) = (0, Vec::new());
    match decode_chunked_step(data, &mut pos, &mut out, MAX_ANSWER) {
        Ok(true) => Some(out),
        _ => None,
    }
}

fn invalid(what: &'static str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, what)
}

/// Sends one request and reads the whole answer: (HTTP status, body). A
/// body longer than `limit` is an error.
async fn exchange<S>(stream: &mut S, request: &[u8], limit: usize) -> std::io::Result<(u16, Vec<u8>)>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    stream.write_all(request).await?;
    stream.flush().await?;

    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    let header_end = loop {
        if let Some(at) = find(&buf, b"\r\n\r\n") {
            break at;
        }
        if buf.len() > 64 * 1024 {
            return Err(invalid("HTTP header too long"));
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let mut words = status_line.split_whitespace();
    if !words.next().is_some_and(|v| v.starts_with("HTTP/1.")) {
        return Err(invalid("not an HTTP answer"));
    }
    let status: u16 =
        words.next().and_then(|c| c.parse().ok()).ok_or_else(|| invalid("no HTTP status"))?;
    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    for line in lines {
        let Some((key, value)) = line.split_once(':') else { continue };
        let value = value.trim();
        if key.eq_ignore_ascii_case("content-length") {
            content_length = value.parse().ok();
        } else if key.eq_ignore_ascii_case("transfer-encoding") {
            chunked = value.to_ascii_lowercase().contains("chunked");
        }
    }
    let mut body = buf.split_off(header_end + 4);

    if chunked {
        let (mut pos, mut decoded) = (0usize, Vec::new());
        loop {
            match decode_chunked_step(&body, &mut pos, &mut decoded, limit) {
                Ok(true) => return Ok((status, decoded)),
                Ok(false) => {}
                Err(what) => return Err(invalid(what)),
            }
            // What is decoded is not kept a second time.
            if pos > 0 {
                body.drain(..pos);
                pos = 0;
            }
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            body.extend_from_slice(&chunk[..n]);
        }
    }
    if let Some(len) = content_length {
        if len > limit {
            return Err(invalid("answer too long"));
        }
        while body.len() < len {
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            body.extend_from_slice(&chunk[..n]);
        }
        body.truncate(len);
        return Ok((status, body));
    }
    // Neither: the body ends with the connection. A TLS peer that closes
    // without saying so first shows as UnexpectedEof; that is the end too.
    loop {
        if body.len() > limit {
            return Err(invalid("answer too long"));
        }
        match stream.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => body.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e),
        }
    }
    Ok((status, body))
}

/// Accepts the host's self-signed certificate only when it is, byte for byte,
/// the one kept from pairing. (The reference switches verification off:
/// libgamestream/http.c http_init CURLOPT_SSL_VERIFYPEER 0; moonlight-qt pins
/// the certificate as done here, nvpairingmanager.cpp "Pin this cert for TLS".)
#[derive(Debug)]
struct PinnedCertificate {
    der: Vec<u8>,
    algorithms: rustls::crypto::WebPkiSupportedAlgorithms,
}

impl rustls::client::danger::ServerCertVerifier for PinnedCertificate {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if end_entity.as_ref() == self.der.as_slice() {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// The TLS settings for one host: the client's certificate and key, the
/// host's certificate pinned (libgamestream/http.c http_init: client
/// certificate and key as PEM, no session cache).
fn tls_config(identity: &Identity, server_cert_pem: &str) -> Result<Arc<rustls::ClientConfig>, Error> {
    let pinned = cert_der_from_pem(server_cert_pem)
        .ok_or(Error::Identity("the computer's certificate cannot be read"))?;
    let cert = cert_der_from_pem(&identity.client_cert_pem)
        .ok_or(Error::Identity("certificate cannot be read"))?;
    let (label, key_der) =
        pem_decode(&identity.client_key_pem).ok_or(Error::Identity("key is not PEM"))?;
    let key = match label.as_str() {
        "PRIVATE KEY" => rustls::pki_types::PrivateKeyDer::Pkcs8(key_der.into()),
        "RSA PRIVATE KEY" => rustls::pki_types::PrivateKeyDer::Pkcs1(key_der.into()),
        _ => return Err(Error::Identity("key is of an unknown kind")),
    };
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = PinnedCertificate {
        der: pinned,
        algorithms: provider.signature_verification_algorithms,
    };
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|_| Error::Internal("TLS set-up failed"))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_client_auth_cert(vec![cert.into()], key)
        .map_err(|_| Error::Identity("key does not fit the certificate"))?;
    // Sunshine notes the client's certificate while the handshake runs
    // (nvhttp.cpp https_server.verify, last_verified_client_cert): every
    // request makes a full handshake, as CURLOPT_SSL_SESSIONID_CACHE 0 does.
    config.resumption = rustls::client::Resumption::disabled();
    Ok(Arc::new(config))
}

fn map_io_error(e: &std::io::Error) -> Error {
    if let Some(tls) = e.get_ref().and_then(|inner| inner.downcast_ref::<rustls::Error>()) {
        return match tls {
            rustls::Error::InvalidCertificate(_) => Error::HostChanged,
            _ => Error::ConnectionLost,
        };
    }
    match e.kind() {
        std::io::ErrorKind::InvalidData => Error::Protocol("HTTP answer is malformed"),
        _ => Error::ConnectionLost,
    }
}

/// One GET to `host:port`, over TLS when `tls` is given; the body as text.
/// `what` names the request in log lines (never the query).
pub(super) async fn http_get(
    host: &str,
    port: u16,
    tls: Option<&Arc<rustls::ClientConfig>>,
    target: &str,
    what: &str,
    timeout: Duration,
) -> Result<String, Error> {
    let host_header = if host.contains(':') { format!("[{host}]") } else { host.to_string() };
    let request = format!(
        "GET {target} HTTP/1.1\r\nHost: {host_header}:{port}\r\nAccept: */*\r\nConnection: close\r\n\r\n"
    );
    let scheme = if tls.is_some() { "https" } else { "http" };
    log::debug!("sunshine: {what} over {scheme} to {host}:{port}");

    let run = async {
        let connect = tokio::time::timeout(CONNECT_TIMEOUT.min(timeout), TcpStream::connect((host, port)));
        let stream = match connect.await {
            Err(_) => return Err(Error::Unreachable),
            Ok(Err(e)) => {
                log::debug!("sunshine: {what}: no connection to {host}:{port}: {e}");
                return Err(Error::Unreachable);
            }
            Ok(Ok(s)) => s,
        };
        let _ = stream.set_nodelay(true);
        let answer = match tls {
            Some(config) => {
                let name = rustls::pki_types::ServerName::try_from(host.to_string())
                    .or_else(|_| rustls::pki_types::ServerName::try_from("gamestream.invalid"))
                    .map_err(|_| Error::Internal("TLS set-up failed"))?;
                let connector = tokio_rustls::TlsConnector::from(config.clone());
                let mut stream = connector.connect(name, stream).await.map_err(|e| {
                    log::debug!("sunshine: {what}: TLS to {host}:{port} failed: {e}");
                    map_io_error(&e)
                })?;
                exchange(&mut stream, request.as_bytes(), MAX_ANSWER).await
            }
            None => {
                let mut stream = stream;
                exchange(&mut stream, request.as_bytes(), MAX_PLAIN_ANSWER).await
            }
        };
        answer.map_err(|e| {
            log::debug!("sunshine: {what}: exchange with {host}:{port} failed: {e}");
            map_io_error(&e)
        })
    };
    let (status, body) = match tokio::time::timeout(timeout, run).await {
        Err(_) => {
            log::debug!("sunshine: {what}: {host}:{port} did not answer in {timeout:?}");
            return Err(Error::Timeout);
        }
        Ok(result) => result?,
    };
    // libgamestream/http.c http_init CURLOPT_FAILONERROR: HTTP 400 and above fail.
    if status >= 400 {
        log::debug!("sunshine: {what}: HTTP status {status} from {host}:{port}");
        return Err(Error::Protocol("HTTP error status"));
    }
    String::from_utf8(body).map_err(|_| Error::Protocol("answer is not UTF-8"))
}

// ---------------------------------------------------------------------------
// Answers
// ---------------------------------------------------------------------------

/// A display mode a host lists (`SupportedDisplayMode`; GeForce Experience
/// does, Sunshine does not).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayMode {
    pub width: u32,
    pub height: u32,
    pub refresh: u32,
}

/// The answer to `serverinfo`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerInfo {
    pub hostname: String,
    /// `appversion`, e.g. "7.1.431.-1" (Sunshine); RTSP and the control stream
    /// choose their dialect by it (moonlight-common-c serverInfoAppVersion).
    pub app_version: String,
    /// `GfeVersion`, e.g. "3.23.0.74" (moonlight-common-c serverInfoGfeVersion).
    pub gfe_version: String,
    /// The host's own id.
    pub unique_id: String,
    /// `HttpsPort`; [`DEFAULT_HTTPS_PORT`] when the host does not say.
    pub https_port: u16,
    /// `ExternalPort` (0 when absent).
    pub external_port: u16,
    /// `ServerCodecModeSupport`: the `SCM_*` bits; `SCM_H264` when absent.
    pub server_codec_mode_support: u32,
    /// `PairStatus` is 1. Only an answer over HTTPS can say so: over plain HTTP
    /// Sunshine always answers 0 (nvhttp.cpp serverinfo).
    pub paired: bool,
    /// `currentgame`, forced to 0 unless `state` says the host is busy, as the
    /// reference does (libgamestream/client.c load_serverinfo).
    pub current_game: u32,
    /// `state`, e.g. "SUNSHINE_SERVER_FREE" / "SUNSHINE_SERVER_BUSY".
    pub state: String,
    /// `mac` (Sunshine gives the real one only over HTTPS).
    pub mac: String,
    /// `LocalIP`.
    pub local_ip: String,
    /// `ExternalIP`, when the host has one configured.
    pub external_ip: Option<String>,
    /// `MaxLumaPixelsHEVC` (0: no HEVC).
    pub max_luma_pixels_hevc: u64,
    /// `gputype` (GeForce Experience only).
    pub gpu_type: Option<String>,
    /// `GsVersion` (GeForce Experience only).
    pub gs_version: Option<String>,
    /// `SupportedDisplayMode` (GeForce Experience only; empty with Sunshine).
    pub display_modes: Vec<DisplayMode>,
    /// The state names NVIDIA's software ("MJOLNIR"): libgamestream/client.c
    /// load_serverinfo isNvidiaSoftware.
    pub is_nvidia_software: bool,
    /// The answer came over HTTPS (so `paired` means something).
    pub over_https: bool,
    /// Every element directly under the root with its text, for what has no
    /// field here.
    pub fields: BTreeMap<String, String>,
}

impl ServerInfo {
    /// The first number of `appversion` (7 for Sunshine): chooses SHA-256 or
    /// SHA-1 in pairing (libgamestream/client.c serverMajorVersion).
    pub fn server_major_version(&self) -> u32 {
        leading_number(&self.app_version)
    }

    /// The host says it is in a session.
    pub fn is_busy(&self) -> bool {
        self.state.contains("_SERVER_BUSY")
    }

    pub fn supports_h264(&self) -> bool {
        self.server_codec_mode_support & SCM_H264 != 0
    }

    pub fn supports_hevc(&self) -> bool {
        self.server_codec_mode_support & SCM_HEVC != 0
    }

    pub fn supports_av1(&self) -> bool {
        self.server_codec_mode_support & SCM_AV1_MAIN8 != 0
    }
}

/// `atoi`: the number a text starts with, 0 when it starts with none.
fn leading_number(text: &str) -> u32 {
    let t = text.trim_start();
    let digits = t.bytes().take_while(u8::is_ascii_digit).count();
    t[..digits].parse().unwrap_or(0)
}

/// Reads a `serverinfo` answer (libgamestream/client.c load_serverinfo).
pub fn parse_server_info(xml: &str, over_https: bool) -> Result<ServerInfo, Error> {
    let root = parse_xml(xml)?;
    check_status(&root)?;
    let text = |name: &str| root.text_of(name).trim().to_string();
    let optional = |name: &str| root.find(name).map(|n| n.text.trim().to_string());

    let current_game = text("currentgame");
    let pair_status = text("PairStatus");
    let app_version = text("appversion");
    let state = text("state");
    // "These fields are present on all version of GFE that this client supports"
    if current_game.is_empty() || pair_status.is_empty() || app_version.is_empty() || state.is_empty() {
        return Err(Error::Protocol("serverinfo lacks a required field"));
    }
    let codec_text = text("ServerCodecModeSupport");
    let mut info = ServerInfo {
        hostname: text("hostname"),
        gfe_version: text("GfeVersion"),
        unique_id: text("uniqueid"),
        https_port: match leading_number(&text("HttpsPort")) {
            0 => DEFAULT_HTTPS_PORT,
            p => u16::try_from(p).unwrap_or(DEFAULT_HTTPS_PORT),
        },
        external_port: u16::try_from(leading_number(&text("ExternalPort"))).unwrap_or(0),
        server_codec_mode_support: if codec_text.is_empty() {
            SCM_H264
        } else {
            leading_number(&codec_text)
        },
        paired: pair_status == "1",
        current_game: leading_number(&current_game),
        mac: text("mac"),
        local_ip: text("LocalIP"),
        external_ip: optional("ExternalIP"),
        max_luma_pixels_hevc: text("MaxLumaPixelsHEVC").parse().unwrap_or(0),
        gpu_type: optional("gputype"),
        gs_version: optional("GsVersion"),
        is_nvidia_software: state.contains("MJOLNIR"),
        over_https,
        app_version,
        state,
        ..Default::default()
    };
    // "After GFE 2.8, current game remains set even after streaming has ended."
    if !info.is_busy() {
        info.current_game = 0;
    }
    // libgamestream/xml.c xml_modelist
    for mode in root.find_all("DisplayMode") {
        info.display_modes.push(DisplayMode {
            width: leading_number(mode.text_of("Width")),
            height: leading_number(mode.text_of("Height")),
            refresh: leading_number(mode.text_of("RefreshRate")),
        });
    }
    for child in &root.children {
        if child.children.is_empty() {
            info.fields.insert(child.name.clone(), child.text.trim().to_string());
        }
    }
    Ok(info)
}

/// One application of the host's list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    pub id: i32,
    pub title: String,
    pub hdr_supported: bool,
}

/// Reads an `applist` answer (libgamestream/xml.c xml_applist: `App` with
/// `AppTitle` and `ID`; Sunshine nvhttp.cpp applist adds `IsHdrSupported`).
pub fn parse_app_list(xml: &str) -> Result<Vec<App>, Error> {
    let root = parse_xml(xml)?;
    check_status(&root)?;
    Ok(root
        .find_all("App")
        .into_iter()
        .map(|app| App {
            id: app.text_of("ID").trim().parse().unwrap_or(0),
            title: app.text_of("AppTitle").to_string(),
            hdr_supported: app.text_of("IsHdrSupported").trim() == "1",
        })
        .collect())
}

/// What `launch` and `resume` send. Every value Moonlight sends today is here
/// (libgamestream/client.c gs_start_app, moonlight-qt nvhttp.cpp
/// NvHTTP::startApp, moonlight-common-c Connection.c
/// LiGetLaunchUrlQueryParameters), plus the two that Sunshine reads and those
/// do not send (nvhttp.cpp make_launch_session).
#[derive(Clone, PartialEq, Eq)]
pub struct LaunchParams {
    /// `appid`: the `ID` of the application list. Sunshine starts no program
    /// for 0 or less (nvhttp.cpp launch).
    pub app_id: i32,
    /// `mode=<width>x<height>x<fps>`.
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// `sops`: let the host change the game's settings for streaming.
    pub sops: bool,
    /// `rikey`: 16 random bytes, the session's AES key. The host uses it for
    /// the RTSP encryption (with `core_version` 1 and more), the control
    /// stream, input, audio and video encryption (Sunshine nvhttp.cpp
    /// make_launch_session gcm_key; moonlight-common-c remoteInputAesKey).
    pub rikey: [u8; 16],
    /// `rikeyid`: the first four bytes of the session's 16-byte IV, read as a
    /// big-endian signed number; the other twelve bytes are zero
    /// (libgamestream/client.c gs_start_app; Sunshine nvhttp.cpp
    /// make_launch_session writes it back big-endian into `iv[0..4]`).
    pub rikey_id: i32,
    /// `localAudioPlayMode`: 1 keeps the sound playing at the host too.
    pub local_audio_play_mode: bool,
    /// `surroundAudioInfo`: channel mask << 16 | channel count.
    pub surround_audio_info: u32,
    /// `remoteControllersBitmap` and `gcmap`: one bit per game controller.
    pub gamepad_mask: u32,
    /// `gcpersist` (moonlight-qt: keep controllers when the client leaves).
    pub persist_gamepads: bool,
    /// `hdrMode=1` and the client's HDR capability fields.
    pub hdr: bool,
    /// `corever`: 0 = video and control stream encryption only, 1 = RTSP is
    /// encrypted as well and the session URL comes back as `rtspenc://`
    /// (moonlight-common-c Connection.c LiGetLaunchUrlQueryParameters sends 1;
    /// Sunshine nvhttp.cpp make_launch_session `corever >= 1`).
    pub core_version: u32,
    /// `surroundParams`: read by Sunshine, sent by nobody in the references;
    /// not sent when `None`.
    pub surround_params: Option<String>,
    /// `continuousAudio`: read by Sunshine; not sent when false.
    pub continuous_audio: bool,
}

impl fmt::Debug for LaunchParams {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never the key.
        f.debug_struct("LaunchParams")
            .field("app_id", &self.app_id)
            .field("mode", &format_args!("{}x{}x{}", self.width, self.height, self.fps))
            .field("core_version", &self.core_version)
            .finish_non_exhaustive()
    }
}

impl LaunchParams {
    /// Stereo, no controllers, no HDR, encrypted RTSP, with a fresh random
    /// key and key id (libgamestream/client.c gs_start_app RAND_bytes).
    pub fn new(app_id: i32, width: u32, height: u32, fps: u32) -> Result<LaunchParams, Error> {
        Ok(LaunchParams {
            app_id,
            width,
            height,
            fps,
            sops: false,
            rikey: random_bytes::<16>()?,
            rikey_id: i32::from_be_bytes(random_bytes::<4>()?),
            local_audio_play_mode: false,
            surround_audio_info: SURROUND_AUDIO_INFO_STEREO,
            gamepad_mask: 0,
            persist_gamepads: false,
            hdr: false,
            core_version: 1,
            surround_params: None,
            continuous_audio: false,
        })
    }

    /// The session's 16-byte IV that belongs to `rikey_id`: the id big-endian
    /// in the first four bytes, zeros after (moonlight-common-c
    /// StreamConfig.remoteInputAesIv; Sunshine launch_session->iv).
    pub fn rikey_iv(&self) -> [u8; 16] {
        let mut iv = [0u8; 16];
        iv[..4].copy_from_slice(&self.rikey_id.to_be_bytes());
        iv
    }

    /// The query after `uniqueid` and `uuid`, in the order of moonlight-qt
    /// nvhttp.cpp NvHTTP::startApp.
    pub(super) fn query(&self) -> String {
        let mut q = format!(
            "appid={}&mode={}x{}x{}&additionalStates=1&sops={}&rikey={}&rikeyid={}",
            self.app_id,
            self.width,
            self.height,
            self.fps,
            self.sops as u8,
            hex_encode(&self.rikey),
            self.rikey_id,
        );
        if self.hdr {
            q.push_str("&hdrMode=1&clientHdrCapVersion=0&clientHdrCapSupportedFlagsInUint32=0&clientHdrCapMetaDataId=NV_STATIC_METADATA_TYPE_1&clientHdrCapDisplayData=0x0x0x0x0x0x0x0x0x0x0");
        }
        q.push_str(&format!(
            "&localAudioPlayMode={}&surroundAudioInfo={}&remoteControllersBitmap={}&gcmap={}&gcpersist={}",
            self.local_audio_play_mode as u8,
            self.surround_audio_info,
            self.gamepad_mask,
            self.gamepad_mask,
            self.persist_gamepads as u8,
        ));
        if let Some(params) = &self.surround_params {
            q.push_str(&format!("&surroundParams={}", url_encode(params)));
        }
        if self.continuous_audio {
            q.push_str("&continuousAudio=1");
        }
        q.push_str(&format!("&corever={}", self.core_version));
        q
    }
}

/// The answer to `launch` or `resume`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchAnswer {
    /// `sessionUrl0`: where RTSP is, e.g. `rtspenc://192.168.1.2:48010`
    /// (`rtsp://` when `core_version` was 0). Hosts older than this element
    /// give none: RTSP is then at the host's address, port 48010.
    pub session_url: Option<String>,
    /// The request was `resume`.
    pub resumed: bool,
    /// Every element directly under the root with its text (`gamesession` or
    /// `resume`, `sessionUrl0`, and whatever else a host adds).
    pub fields: BTreeMap<String, String>,
}

fn parse_launch_answer(xml: &str, resumed: bool) -> Result<LaunchAnswer, Error> {
    let root = parse_xml(xml)?;
    // Statuses of Sunshine nvhttp.cpp launch() and resume(); 470 is what
    // GeForce Experience answers when another client owns the session.
    match xml_status(&root) {
        (200, _) => {}
        (401, _) => return Err(Error::NotPaired),
        (470, _) => return Err(Error::Busy),
        (400, m) if m.contains("already running") => return Err(Error::Busy),
        (403, _) => return Err(Error::EncryptionRequired),
        (404, _) => return Err(Error::AppNotFound),
        (503, m) if m.contains("capture") || m.contains("encod") => return Err(Error::CaptureFailed),
        _ => check_status(&root)?,
    }
    // libgamestream/client.c gs_start_app: "gamesession", else "resume"; "0" is a failure.
    let mut result = root.text_of("gamesession").trim();
    if result.is_empty() {
        result = root.text_of("resume").trim();
    }
    if result == "0" {
        return Err(Error::Host { code: 200, message: String::new() });
    }
    let session_url = root
        .find("sessionUrl0")
        .map(|n| n.text.trim().to_string())
        .filter(|u| !u.is_empty());
    let mut fields = BTreeMap::new();
    for child in &root.children {
        if child.children.is_empty() {
            fields.insert(child.name.clone(), child.text.trim().to_string());
        }
    }
    Ok(LaunchAnswer { session_url, resumed, fields })
}

// ---------------------------------------------------------------------------
// The client
// ---------------------------------------------------------------------------

/// The requests to one host.
///
/// Plain HTTP goes to `http_port`; HTTPS goes to the port `serverinfo` reports
/// and is possible only when the host's certificate from pairing is given
/// (before pairing none is known: only `serverinfo` over HTTP and pairing
/// itself work then).
pub struct HostClient {
    host: String,
    http_port: u16,
    https_port: AtomicU16,
    unique_id: String,
    tls: Option<Arc<rustls::ClientConfig>>,
}

impl fmt::Debug for HostClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostClient").field("paired", &self.tls.is_some()).finish_non_exhaustive()
    }
}

impl HostClient {
    /// `server_cert_pem`: the host's certificate kept from pairing
    /// ([`super::pair::Paired::server_cert_pem`]), `None` before pairing.
    pub fn new(
        host: &str,
        http_port: u16,
        identity: &Identity,
        server_cert_pem: Option<&str>,
    ) -> Result<HostClient, Error> {
        let tls = match server_cert_pem {
            Some(pem) => Some(tls_config(identity, pem)?),
            None => None,
        };
        Ok(HostClient {
            host: host.trim().to_string(),
            http_port: if http_port == 0 { DEFAULT_HTTP_PORT } else { http_port },
            https_port: AtomicU16::new(0),
            unique_id: identity.unique_id.clone(),
            tls,
        })
    }

    /// The HTTPS port in use; 0 until `serverinfo` was asked or it was set.
    pub fn https_port(&self) -> u16 {
        self.https_port.load(Ordering::Relaxed)
    }

    /// Sets the HTTPS port without asking `serverinfo`.
    pub fn set_https_port(&self, port: u16) {
        self.https_port.store(port, Ordering::Relaxed);
    }

    /// `/<path>?uniqueid=<id>&uuid=<random>[&<query>]`, as every request of
    /// libgamestream/client.c starts.
    fn target(&self, path: &str, query: &str) -> String {
        let mut target = format!(
            "/{path}?uniqueid={}&uuid={}",
            url_encode(&self.unique_id),
            uuid::Uuid::new_v4()
        );
        if !query.is_empty() {
            target.push('&');
            target.push_str(query);
        }
        target
    }

    pub(super) async fn get_http(&self, path: &str, query: &str, timeout: Duration) -> Result<String, Error> {
        http_get(&self.host, self.http_port, None, &self.target(path, query), path, timeout).await
    }

    pub(super) async fn get_https(&self, path: &str, query: &str, timeout: Duration) -> Result<String, Error> {
        let tls = self.tls.as_ref().ok_or(Error::NotPaired)?;
        if self.https_port() == 0 {
            // libgamestream/client.c load_server_status: "Fetch the HTTPS port if we don't have one yet"
            let info = self.server_info_http().await?;
            self.set_https_port(info.https_port);
        }
        http_get(&self.host, self.https_port(), Some(tls), &self.target(path, query), path, timeout).await
    }

    /// `serverinfo` over plain HTTP: works unpaired, `paired` is always false.
    pub async fn server_info_http(&self) -> Result<ServerInfo, Error> {
        let xml = self.get_http("serverinfo", "", REQUEST_TIMEOUT).await?;
        let info = parse_server_info(&xml, false)?;
        self.set_https_port(info.https_port);
        Ok(info)
    }

    /// `serverinfo` as the reference asks it (libgamestream/client.c
    /// load_server_status): over HTTP once for the HTTPS port, then over HTTPS
    /// (only that answer says whether the host knows this client), and over
    /// HTTP again when the host turns the certificate down (moonlight-qt
    /// nvhttp.cpp NvHTTP::getServerInfo falls back on status 401 only; so does
    /// this, and on a changed host certificate). Check `paired` and
    /// `over_https` of the answer.
    pub async fn server_info(&self) -> Result<ServerInfo, Error> {
        if self.tls.is_none() {
            return self.server_info_http().await;
        }
        let result = match self.get_https("serverinfo", "", REQUEST_TIMEOUT).await {
            Ok(xml) => parse_server_info(&xml, true),
            Err(e) => Err(e),
        };
        match result {
            // The host answered with its own certificate and does not know
            // this client: what it says over HTTP is good enough to tell the
            // person so.
            Err(Error::NotPaired) => {
                log::info!("sunshine: the host does not accept the pairing; asking over HTTP");
                self.server_info_http().await
            }
            // Another certificate than the pinned one: whoever answers there
            // is not the computer that was paired. Nothing it (or anybody
            // else) says over plain HTTP is taken in its place: the person is
            // told that, not "pair again".
            Err(Error::HostChanged) => {
                log::warn!("sunshine: the computer at this address shows another certificate than the one kept from pairing; not connecting");
                Err(Error::HostChanged)
            }
            other => other,
        }
    }

    /// The host's applications (libgamestream/client.c gs_applist). HTTPS only.
    pub async fn app_list(&self) -> Result<Vec<App>, Error> {
        let xml = self.get_https("applist", "", REQUEST_TIMEOUT).await?;
        parse_app_list(&xml)
    }

    /// Starts an application and a session (`/launch`). The host answers
    /// status 400 when something already runs: use [`HostClient::resume`]
    /// then, or [`HostClient::start_app`] which chooses.
    pub async fn launch(&self, params: &LaunchParams) -> Result<LaunchAnswer, Error> {
        let xml = self.get_https("launch", &params.query(), LAUNCH_TIMEOUT).await?;
        parse_launch_answer(&xml, false)
    }

    /// Joins the session of the application that runs (`/resume`).
    pub async fn resume(&self, params: &LaunchParams) -> Result<LaunchAnswer, Error> {
        let xml = self.get_https("resume", &params.query(), RESUME_TIMEOUT).await?;
        parse_launch_answer(&xml, true)
    }

    /// `resume` when `info` says an application runs, `launch` otherwise
    /// (libgamestream/client.c gs_start_app `server->currentGame ? "resume" :
    /// "launch"`), with the frame rate sent as 0 above 60 to NVIDIA's software
    /// only (same function, "Using an FPS value over 60 causes SOPS to default
    /// to 720p60").
    pub async fn start_app(&self, info: &ServerInfo, params: &LaunchParams) -> Result<LaunchAnswer, Error> {
        let mut params = params.clone();
        if info.is_nvidia_software && params.fps > 60 {
            params.fps = 0;
        }
        if info.current_game != 0 {
            self.resume(&params).await
        } else {
            self.launch(&params).await
        }
    }

    /// Ends the session and the application (libgamestream/client.c
    /// gs_quit_app). As moonlight-qt nvhttp.cpp NvHTTP::quitApp, asks
    /// `serverinfo` afterwards: a host that still runs something did not let
    /// this client end another client's session ([`Error::Busy`]).
    pub async fn cancel(&self) -> Result<(), Error> {
        let xml = self.get_https("cancel", "", QUIT_TIMEOUT).await?;
        let root = parse_xml(&xml)?;
        check_status(&root)?;
        if root.text_of("cancel").trim() == "0" {
            return Err(Error::Host { code: 200, message: String::new() });
        }
        match self.server_info().await {
            Ok(info) if info.current_game != 0 => Err(Error::Busy),
            _ => Ok(()),
        }
    }

    /// Tells the host to forget this client (libgamestream/client.c
    /// gs_unpair). GeForce Experience knows `/unpair`; Sunshine does not
    /// (nvhttp.cpp start(): no such resource, the answer is status 404) and
    /// forgets a client only from its own web page.
    pub async fn unpair(&self) -> Result<(), Error> {
        self.get_http("unpair", "", REQUEST_TIMEOUT).await.map(|_| ())
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    // A real answer of the test host (Sunshine 2026.914, Linux), over HTTP.
    const SERVERINFO_HTTP: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<root status_code=\"200\"><hostname>lunaris-s1-test</hostname><appversion>7.1.431.-1</appversion><GfeVersion>3.23.0.74</GfeVersion><uniqueid>01234567-89AB-CDEF-0123-456789ABCDEF</uniqueid><HttpsPort>47984</HttpsPort><ExternalPort>47989</ExternalPort><MaxLumaPixelsHEVC>1869449984</MaxLumaPixelsHEVC><mac>00:00:00:00:00:00</mac><LocalIP>192.168.1.20</LocalIP><ServerCodecModeSupport>769</ServerCodecModeSupport><PairStatus>0</PairStatus><currentgame>0</currentgame><state>SUNSHINE_SERVER_FREE</state></root>";

    #[test]
    fn serverinfo_of_the_test_host() {
        let info = parse_server_info(SERVERINFO_HTTP, false).unwrap();
        assert_eq!(info.hostname, "lunaris-s1-test");
        assert_eq!(info.app_version, "7.1.431.-1");
        assert_eq!(info.server_major_version(), 7);
        assert_eq!(info.gfe_version, "3.23.0.74");
        assert_eq!(info.unique_id, "01234567-89AB-CDEF-0123-456789ABCDEF");
        assert_eq!(info.https_port, 47984);
        assert_eq!(info.external_port, 47989);
        assert_eq!(info.server_codec_mode_support, 769);
        assert!(info.supports_h264() && info.supports_hevc() && !info.supports_av1());
        assert_eq!(info.server_codec_mode_support & SCM_HEVC_MAIN10, SCM_HEVC_MAIN10);
        assert!(!info.paired);
        assert_eq!(info.current_game, 0);
        assert_eq!(info.state, "SUNSHINE_SERVER_FREE");
        assert_eq!(info.max_luma_pixels_hevc, 1869449984);
        assert_eq!(info.local_ip, "192.168.1.20");
        assert_eq!(info.external_ip, None);
        assert!(info.display_modes.is_empty());
        assert!(!info.is_nvidia_software);
        assert_eq!(info.fields.get("mac").map(String::as_str), Some("00:00:00:00:00:00"));
    }

    #[test]
    fn serverinfo_current_game_only_while_busy() {
        // libgamestream/client.c load_serverinfo: currentgame is forced to 0 unless busy.
        let free = SERVERINFO_HTTP.replace("<currentgame>0<", "<currentgame>881448767<");
        assert_eq!(parse_server_info(&free, true).unwrap().current_game, 0);
        let busy = free.replace("SUNSHINE_SERVER_FREE", "SUNSHINE_SERVER_BUSY");
        let info = parse_server_info(&busy, true).unwrap();
        assert_eq!(info.current_game, 881448767);
        assert!(info.is_busy());
    }

    #[test]
    fn serverinfo_of_geforce_experience_shape() {
        // The shape GeForce Experience answers in (display modes, gputype), built by hand.
        let xml = "<?xml version=\"1.0\" encoding=\"UTF-16\"?><root protocol_version=\"0.1\" query=\"serverinfo\" status_code=\"200\" status_message=\"OK\"><appversion>7.1.431.0</appversion><GfeVersion>3.20.4.14</GfeVersion><gputype>GeForce GTX 1070</gputype><GsVersion>3.20.4.14</GsVersion><hostname>PC &amp; more</hostname><PairStatus>1</PairStatus><currentgame>0</currentgame><state>MJOLNIR_STATE_SERVER_AVAILABLE</state><SupportedDisplayMode><DisplayMode><Height>1080</Height><Width>1920</Width><RefreshRate>60</RefreshRate></DisplayMode><DisplayMode><Height>2160</Height><Width>3840</Width><RefreshRate>120</RefreshRate></DisplayMode></SupportedDisplayMode></root>";
        let info = parse_server_info(xml, true).unwrap();
        assert!(info.paired && info.is_nvidia_software);
        assert_eq!(info.hostname, "PC & more");
        assert_eq!(info.https_port, DEFAULT_HTTPS_PORT);
        assert_eq!(info.server_codec_mode_support, SCM_H264);
        assert_eq!(info.gpu_type.as_deref(), Some("GeForce GTX 1070"));
        assert_eq!(
            info.display_modes,
            vec![
                DisplayMode { width: 1920, height: 1080, refresh: 60 },
                DisplayMode { width: 3840, height: 2160, refresh: 120 }
            ]
        );
    }

    #[test]
    fn status_of_answers() {
        // Sunshine nvhttp.cpp on_verify_failed
        let denied = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<root status_code=\"401\" query=\"/serverinfo\" status_message=\"The client is not authorized. Certificate verification failed.\"/>";
        let root = parse_xml(denied).unwrap();
        assert_eq!(xml_status(&root).0, 401);
        assert_eq!(parse_server_info(denied, true), Err(Error::NotPaired));
        // Sunshine nvhttp.cpp not_found
        let missing = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<root status_code=\"404\"/>";
        assert_eq!(
            check_status(&parse_xml(missing).unwrap()),
            Err(Error::Host { code: 404, message: String::new() })
        );
        assert_eq!(check_status(&parse_xml("<other status_code=\"200\"/>").unwrap()), Err(Error::Protocol("answer without status")));
        assert!(parse_xml("<root status_code=\"200\"><a>").is_err());
        assert!(parse_xml("no xml").is_err());
    }

    #[test]
    fn app_list_answer() {
        // The shape of Sunshine nvhttp.cpp applist.
        let xml = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<root status_code=\"200\"><App><IsHdrSupported>1</IsHdrSupported><AppTitle>Desktop</AppTitle><ID>881448767</ID></App><App><IsHdrSupported>0</IsHdrSupported><AppTitle>Steam &lt;Big Picture&gt; &#233;</AppTitle><ID>1093255277</ID></App></root>";
        let apps = parse_app_list(xml).unwrap();
        assert_eq!(apps.len(), 2);
        assert_eq!(apps[0], App { id: 881448767, title: "Desktop".into(), hdr_supported: true });
        assert_eq!(apps[1].title, "Steam <Big Picture> \u{e9}");
        assert_eq!(apps[1].id, 1093255277);
    }

    #[test]
    fn launch_answers() {
        let ok = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<root status_code=\"200\"><sessionUrl0>rtspenc://192.168.1.20:48010</sessionUrl0><gamesession>1</gamesession></root>";
        let answer = parse_launch_answer(ok, false).unwrap();
        assert_eq!(answer.session_url.as_deref(), Some("rtspenc://192.168.1.20:48010"));
        assert_eq!(answer.fields.get("gamesession").map(String::as_str), Some("1"));
        let resumed = "<root status_code=\"200\"><sessionUrl0>rtsp://10.0.0.2:48010</sessionUrl0><resume>1</resume></root>";
        assert!(parse_launch_answer(resumed, true).unwrap().resumed);
        // Sunshine nvhttp.cpp launch(): the statuses it answers.
        let running = "<root status_code=\"400\" status_message=\"An app is already running on this host\"><resume>0</resume></root>";
        assert_eq!(parse_launch_answer(running, false), Err(Error::Busy));
        let capture = "<root status_code=\"503\" status_message=\"Failed to initialize video capture/encoding. Is a display connected and turned on?\"><gamesession>0</gamesession></root>";
        assert_eq!(parse_launch_answer(capture, false), Err(Error::CaptureFailed));
        let encryption = "<root status_code=\"403\" status_message=\"Encryption is mandatory for this host but unsupported by the client\"><gamesession>0</gamesession></root>";
        assert_eq!(parse_launch_answer(encryption, false), Err(Error::EncryptionRequired));
        let no_app = "<root status_code=\"404\" status_message=\"Failed to start the specified application\"><gamesession>0</gamesession></root>";
        assert_eq!(parse_launch_answer(no_app, false), Err(Error::AppNotFound));
        let nothing = "<root status_code=\"503\" status_message=\"No running app to resume\"><resume>0</resume></root>";
        assert_eq!(
            parse_launch_answer(nothing, true),
            Err(Error::Host { code: 503, message: "No running app to resume".into() })
        );
        let zero = "<root status_code=\"200\"><gamesession>0</gamesession></root>";
        assert!(parse_launch_answer(zero, false).is_err());
    }

    #[test]
    fn launch_query_and_key_id() {
        let mut p = LaunchParams::new(881448767, 1920, 1080, 60).unwrap();
        p.rikey = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
        // libgamestream/client.c gs_start_app: the IV's first four bytes, read
        // big-endian, printed with %d. 0xfffffffe is -2.
        p.rikey_id = i32::from_be_bytes([0xff, 0xff, 0xff, 0xfe]);
        assert_eq!(p.rikey_iv(), [0xff, 0xff, 0xff, 0xfe, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(
            p.query(),
            "appid=881448767&mode=1920x1080x60&additionalStates=1&sops=0&rikey=00112233445566778899aabbccddeeff&rikeyid=-2&localAudioPlayMode=0&surroundAudioInfo=196610&remoteControllersBitmap=0&gcmap=0&gcpersist=0&corever=1"
        );
        p.hdr = true;
        p.continuous_audio = true;
        p.surround_params = Some("a b".into());
        let q = p.query();
        assert!(q.contains("&hdrMode=1&clientHdrCapVersion=0"));
        assert!(q.ends_with("&surroundParams=a%20b&continuousAudio=1&corever=1"));
        // The key must not show in a debug print.
        assert!(!format!("{p:?}").contains("0011223344"));
    }

    #[test]
    fn hex_and_pem_round_trips() {
        assert_eq!(hex_encode(&[0, 0x0f, 0xa5, 0xff]), "000fa5ff");
        assert_eq!(hex_decode("000FA5ff"), Some(vec![0, 0x0f, 0xa5, 0xff]));
        assert_eq!(hex_decode("abc"), None);
        assert_eq!(hex_decode("zz"), None);
        let der: Vec<u8> = (0..200u8).collect();
        let pem = pem_encode("CERTIFICATE", &der);
        assert!(pem.starts_with("-----BEGIN CERTIFICATE-----\n"));
        assert!(pem.ends_with("-----END CERTIFICATE-----\n"));
        assert!(pem.lines().all(|l| l.len() <= 64));
        assert_eq!(pem_decode(&pem), Some(("CERTIFICATE".to_string(), der.clone())));
        assert_eq!(cert_der_from_pem(&pem), Some(der));
        assert_eq!(url_encode("My PC/1 & é"), "My%20PC%2F1%20%26%20%C3%A9");
    }

    #[test]
    fn der_times() {
        // 2026-11-17 00:00:00 UTC = 1794873600
        assert_eq!(civil_from_days(1_794_873_600 / 86_400), (2026, 11, 17));
        assert_eq!(der_time(1_794_873_600 + 3_723), [&[0x17, 13][..], b"261117010203Z"].concat());
        // 2050 and later: GeneralizedTime. 2051-01-01 = 2556144000
        assert_eq!(der_time(2_556_144_000), [&[0x18, 15][..], b"20510101000000Z"].concat());
        assert_eq!(der_tlv(0x04, &[0u8; 200])[..3], [0x04, 0x81, 200]);
        assert_eq!(der_tlv(0x04, &[0u8; 300])[..4], [0x04, 0x82, 1, 44]);
    }

    #[test]
    fn chunked_bodies() {
        assert_eq!(decode_chunked(b"5\r\nhello\r\n6;x=y\r\n world\r\n0\r\n\r\n"), Some(b"hello world".to_vec()));
        assert_eq!(decode_chunked(b"5\r\nhel"), None);
    }

    #[test]
    fn a_chunk_size_from_the_peer_cannot_overflow_or_read_outside() {
        // The largest numbers a size can be: once `size + 2` wrapped around
        // and the body was then cut beyond its end (a panic, which ends the
        // whole program in a release build).
        for size in ["FFFFFFFFFFFFFFFF", "FFFFFFFFFFFFFFFE", "7FFFFFFFFFFFFFFF", "FFFFFFFFFFFFFFFD"] {
            let body = format!("{size}\r\nhello\r\n0\r\n\r\n");
            let (mut pos, mut out) = (0, Vec::new());
            assert_eq!(decode_chunked_step(body.as_bytes(), &mut pos, &mut out, MAX_ANSWER), Err("answer too long"), "{size}");
            assert!(out.is_empty());
            assert_eq!(decode_chunked(body.as_bytes()), None);
        }
        // More digits than a number has, no number at all, a line without end.
        let (mut pos, mut out) = (0, Vec::new());
        assert!(decode_chunked_step(b"FFFFFFFFFFFFFFFFF\r\nx", &mut pos, &mut out, MAX_ANSWER).is_err());
        assert!(decode_chunked_step(b"zz\r\nx", &mut pos, &mut out, MAX_ANSWER).is_err());
        assert!(decode_chunked_step(&[b'1'; 300], &mut pos, &mut out, MAX_ANSWER).is_err());
        // A declared size within the limit but more than what is left of it.
        let (mut pos, mut out) = (0, vec![0u8; 60]);
        assert_eq!(decode_chunked_step(b"5\r\nhello\r\n", &mut pos, &mut out, 64), Err("answer too long"));
    }

    #[test]
    fn a_chunked_body_in_parts_is_gone_through_once() {
        let whole = b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        let (mut pos, mut out) = (0, Vec::new());
        // (The body is over with the line of the last chunk, "0".)
        for end in 1..whole.len() - 2 {
            let before = pos;
            assert_eq!(decode_chunked_step(&whole[..end], &mut pos, &mut out, 64), Ok(false), "{end}");
            assert!(pos >= before && pos <= end);
        }
        assert_eq!(decode_chunked_step(whole, &mut pos, &mut out, 64), Ok(true));
        assert_eq!(out, b"hello world");
    }

    /// One answer from a listener on this computer to one plain request.
    async fn answer_once(answer: Vec<u8>) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 2048];
            let _ = stream.read(&mut request).await;
            let _ = stream.write_all(&answer).await;
            let _ = stream.shutdown().await;
        });
        port
    }

    #[tokio::test]
    async fn plain_http_answers_are_short_or_refused() {
        let ok = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello".to_vec();
        let port = answer_once(ok).await;
        assert_eq!(http_get("127.0.0.1", port, None, "/x", "test", Duration::from_secs(5)).await.as_deref(), Ok("hello"));
        // Declared too long, sent too long without a length, too long in chunks,
        // and the chunk size that once ended the program.
        let big = vec![b'a'; MAX_PLAIN_ANSWER + 10];
        let mut declared = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", big.len()).into_bytes();
        declared.extend_from_slice(&big);
        let mut undeclared = b"HTTP/1.1 200 OK\r\n\r\n".to_vec();
        undeclared.extend_from_slice(&big);
        let mut chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
        for part in big.chunks(4096) {
            chunked.extend_from_slice(format!("{:x}\r\n", part.len()).as_bytes());
            chunked.extend_from_slice(part);
            chunked.extend_from_slice(b"\r\n");
        }
        chunked.extend_from_slice(b"0\r\n\r\n");
        let wrapping = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nFFFFFFFFFFFFFFFF\r\nhello\r\n0\r\n\r\n".to_vec();
        for answer in [declared, undeclared, chunked, wrapping] {
            let port = answer_once(answer).await;
            let result = http_get("127.0.0.1", port, None, "/x", "test", Duration::from_secs(5)).await;
            assert_eq!(result, Err(Error::Protocol("HTTP answer is malformed")));
        }
    }

    #[test]
    fn an_answer_with_too_many_elements_is_refused() {
        let fits = format!("<root status_code=\"200\">{}</root>", "<a/>".repeat(MAX_XML_NODES - 1));
        assert!(parse_xml(&fits).is_ok());
        let too_many = format!("<root status_code=\"200\">{}</root>", "<a/>".repeat(MAX_XML_NODES));
        assert!(parse_xml(&too_many).is_err());
    }

    // A certificate made with the OpenSSL command line (RSA-2048, SHA-256,
    // CN "Sunshine Gamestream Host", the subject Sunshine gives its own), and
    // the signature `openssl dgst -sha256 -sign` made with its key over the 16
    // bytes "0123456789abcdef": what a host sends as `pairingsecret`.
    pub(crate) const OPENSSL_CERT: &str = "-----BEGIN CERTIFICATE-----\n\
MIIDJzCCAg+gAwIBAgIUPIj0HWy/PSg9H3eSsFOVtiwSmNowDQYJKoZIhvcNAQEL\n\
BQAwIzEhMB8GA1UEAwwYU3Vuc2hpbmUgR2FtZXN0cmVhbSBIb3N0MB4XDTI2MTEx\n\
NzEzNTI1M1oXDTQ2MTExMjEzNTI1M1owIzEhMB8GA1UEAwwYU3Vuc2hpbmUgR2Ft\n\
ZXN0cmVhbSBIb3N0MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEA4Hom\n\
m/FjUErOBwP1LcwXdob6AE8rMxBohgZ9bq+AYZuZz+c9kvZbPIADNyp4FEfETp/j\n\
FK75sOk2DysSDq1A6ALwxbvu7vUdyJblr13R3ZG1TOSQQ4QvzrO2s6UfKy2Iowju\n\
LnUcMuXeZkTV5eAyKTe0tbwGWo0HL4iqVn65aFRVCKmHwtCHSgIkyxoZB+52In6f\n\
Vfcq/gxS5MxXbuyNlGth44MWCVsYpmvAffmluOhfQzvRGhGFeo5ylbJHWfeoCRLa\n\
Cq3LVJE+ekz3gGii0Pm91EnyKgzrBYNuzI5cgG0nNgCg75Sb12/lbveoBwUVinFv\n\
eN8l1Rjuu1UJUj6kGQIDAQABo1MwUTAdBgNVHQ4EFgQUwRKHCQ/izqNw1PLUoqOc\n\
tKztHmkwHwYDVR0jBBgwFoAUwRKHCQ/izqNw1PLUoqOctKztHmkwDwYDVR0TAQH/\n\
BAUwAwEB/zANBgkqhkiG9w0BAQsFAAOCAQEAy3nI2x1EJ5uWYoXvYS2z3vmQM5r+\n\
skmsfuZo2NjB6Dja+Cbw6iZ9RnjqSR/dfYJ/i/a0EXl0fonIh40gfKQtg9VhUaws\n\
hdU5QookcS/MZWkA1lAH9/7wDyE6ghKVGsU9NC9D84ersyTazxxQfCtJUNjsZfjB\n\
ClaI9qaYGZDRsY1fvFDMyQMaOUxln8D2UvixrdsA+Mf/dNzL506K4w3JWJ2dzsYR\n\
NAwNULDKa5VRqodMd20F3zxA0hsUWgVlkqG6liHxbK0mWGbpUVf3aByEl/pOHxYK\n\
X/xmz+KEdMmt5GpOA7yXCmRvvrC3iYq4yELgjY9enlxXTQIGSpemrXf6Bg==\n\
-----END CERTIFICATE-----\n\
";
    pub(crate) const OPENSSL_SIGNATURE_HEX: &str = "9aaaac868df5c986113bcb9189f335dc452ac1acbaec80e644d40d87dee06a21a9bf41bef75dca1f01751c98a17710d1c1b752f08eb80c31eb2fea2ddc60c4e2d8fccbb091619e41ab2e9dd220445f13908f5a7b8a8ee12fb152637b9a654dd7efde4404109997cc787a4092cdfb5e917520fc2d3008cae285a8d5a556d98369e055f1f712d1698738444b001a8ec42d66cd205ab81ba29cd81345b411886a6903dcb2fa312d4c0129d24247e756a247a4c506bbd5e37648dbcc1fe669f481988c3868b20d607dfe7866c8d7b5750ba0f3475a725b0b3a54b0ba62cdef21fb3d51d4c13d9af93c0669d82cb28119bde1807a8622783845d8101ab7b0f0f12fb9";

    #[test]
    fn verifies_what_openssl_signed() {
        let signature = hex_decode(OPENSSL_SIGNATURE_HEX).unwrap();
        assert_eq!(signature.len(), 256);
        assert!(rsa_verify_sha256(OPENSSL_CERT, b"0123456789abcdef", &signature));
        assert!(!rsa_verify_sha256(OPENSSL_CERT, b"0123456789abcdeF", &signature));
        let mut bad = signature.clone();
        bad[100] ^= 1;
        assert!(!rsa_verify_sha256(OPENSSL_CERT, b"0123456789abcdef", &bad));
        // The certificate's own signature (what goes into the pairing hash) checks out.
        assert!(cert_is_self_signed(OPENSSL_CERT));
        let parts = cert_parts(&cert_der_from_pem(OPENSSL_CERT).unwrap()).unwrap();
        assert_eq!(parts.signature.len(), 256);
        // `openssl x509 -noout -text` prints the signature; its last bytes are in the file.
        let tail = "ad77fa06";
        assert!(hex_encode(&parts.signature).ends_with(&tail));
    }

    /// One identity for all tests of this process: making a key is slow in a debug build.
    pub(crate) fn test_identity() -> &'static Identity {
        static IDENTITY: std::sync::OnceLock<Identity> = std::sync::OnceLock::new();
        IDENTITY.get_or_init(|| Identity::generate().unwrap())
    }

    #[test]
    fn identity_is_what_mkcert_makes() {
        let id = test_identity();
        id.check().unwrap();
        assert_eq!(id.unique_id.len(), 16);
        assert!(id.unique_id.bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(id.client_key_pem.starts_with("-----BEGIN PRIVATE KEY-----\n"));
        assert!(id.client_cert_pem.starts_with("-----BEGIN CERTIFICATE-----\n"));
        let der = cert_der_from_pem(&id.client_cert_pem).unwrap();
        let parts = cert_parts(&der).unwrap();
        assert_eq!(parts.signature.len(), 256); // RSA-2048
        // Self-signed with SHA-256 and RSA: the host's check.
        assert!(cert_is_self_signed(&id.client_cert_pem));
        // Subject and issuer are CN=NVIDIA GameStream Client (twice in the signed part).
        let cn = b"NVIDIA GameStream Client";
        assert_eq!(parts.tbs.windows(cn.len()).filter(|w| w == cn).count(), 2);
        // Signing with the key and verifying with the certificate agree.
        let sig = rsa_sign_sha256(&id.client_key_pem, b"client secret").unwrap();
        assert!(rsa_verify_sha256(&id.client_cert_pem, b"client secret", &sig));
        assert!(!rsa_verify_sha256(&id.client_cert_pem, b"client secreT", &sig));
        // rustls takes the pair as a client certificate, and webpki reads the certificate.
        tls_config(id, &id.client_cert_pem).unwrap();
        // Nothing secret in a debug print.
        assert_eq!(format!("{id:?}"), "Identity { .. }");
    }

    #[test]
    fn error_texts_are_for_people() {
        assert_eq!(Error::PinRejected.to_string(), "The PIN was not accepted");
        assert_eq!(Error::Busy.to_string(), "This computer is already streaming to another client");
        assert_eq!(Error::Unreachable.to_string(), "Could not reach the computer");
        assert_eq!(Error::PairingTimeout.to_string(), "Pairing was not completed in time");
        let e = Error::Host { code: 503, message: tidy_message("No running\napp to resume") };
        assert_eq!(e.to_string(), "The computer refused the request: No running app to resume (status 503)");
    }
}
