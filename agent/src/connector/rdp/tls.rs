//! The TLS upgrade of an RDP connection (MS-RDPBCGR 5.4.5.1, "Transport
//! Layer Security"), with the server's certificate pinned on first use.
//!
//! The client configuration is ironrdp-tls's (`rustls_verifier.rs`
//! `rustls_client_config`): session resumption off, because CredSSP does
//! not allow it (MS-CSSP: "TLS session resumption is not supported"). The
//! certificate check is Lunaris's own and not ironrdp-tls's, which trusts
//! the system's roots and asks a callback only when they fail: here the
//! certificate seen when the computer was added (`setup`) is the only one
//! taken, whoever signed it. The handshake signatures are still verified
//! with the certificate's key (as in lib.rs `server_tls`), so a server that
//! shows the pinned certificate must hold its private key.
//!
//! The server's public key for CredSSP (MS-CSSP 3.1.5: the key is bound to
//! the TLS channel) is taken from the certificate as ironrdp-tls
//! `extract_tls_server_public_key` takes it: the subjectPublicKey BIT
//! STRING of the SubjectPublicKeyInfo.

use std::sync::{Arc, Mutex};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

/// The SHA-256 of a certificate (DER), lowercase hex.
pub fn fingerprint(der: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, der).as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// The fingerprint shown to people: upper-case pairs with colons.
pub fn display_fingerprint(hex: &str) -> String {
    hex.as_bytes().chunks(2).map(|p| std::str::from_utf8(p).unwrap_or("").to_uppercase()).collect::<Vec<_>>().join(":")
}

/// What the TLS handshake did with the certificate.
#[derive(Debug)]
pub enum CertificateError {
    /// Another certificate than the pinned one: (seen, pinned) fingerprints.
    Changed { seen: String, pinned: String },
    Tls(std::io::Error),
}

impl std::fmt::Display for CertificateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CertificateError::Changed { seen, pinned } => write!(
                f,
                "The computer's certificate is not the one it had when it was added (SHA-256 {} instead of {}). If the computer was reinstalled or its certificate renewed, remove it here and add it again; otherwise someone may be intercepting the connection, and nothing was sent",
                display_fingerprint(seen),
                display_fingerprint(pinned)
            ),
            CertificateError::Tls(e) => write!(f, "The secure connection to the computer failed ({e})"),
        }
    }
}

impl std::error::Error for CertificateError {}

/// Takes the certificate that was pinned (or, with `None`, any one: the
/// set-up) and records the one seen.
struct PinningVerifier {
    pinned: Option<String>,
    algorithms: WebPkiSupportedAlgorithms,
    seen: Mutex<Option<Vec<u8>>>,
}

impl std::fmt::Debug for PinningVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PinningVerifier")
    }
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

impl ServerCertVerifier for PinningVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        *self.seen.lock().unwrap() = Some(end_entity.as_ref().to_vec());
        match &self.pinned {
            None => Ok(ServerCertVerified::assertion()),
            Some(pinned) if *pinned == fingerprint(end_entity.as_ref()) => Ok(ServerCertVerified::assertion()),
            Some(_) => Err(rustls::Error::InvalidCertificate(rustls::CertificateError::ApplicationVerificationFailure)),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// The TLS stream and what was learnt of the server.
pub struct Upgraded<S> {
    pub stream: tokio_rustls::client::TlsStream<S>,
    /// The certificate (DER) and its fingerprint.
    pub certificate: Vec<u8>,
    pub fingerprint: String,
    /// For CredSSP.
    pub public_key: Vec<u8>,
}

/// Upgrades `stream` to TLS for `host`, taking only the certificate whose
/// fingerprint is `pinned` (any one when `None`).
pub async fn upgrade<S>(stream: S, host: &str, pinned: Option<&str>) -> Result<Upgraded<S>, CertificateError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let provider = provider();
    let verifier = Arc::new(PinningVerifier {
        pinned: pinned.map(str::to_string),
        algorithms: provider.signature_verification_algorithms,
        seen: Mutex::new(None),
    });
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| CertificateError::Tls(std::io::Error::other(e)))?
        .dangerous()
        .with_custom_certificate_verifier(verifier.clone())
        .with_no_client_auth();
    // ironrdp-tls rustls_verifier.rs: CredSSP does not support TLS session
    // resumption (MS-CSSP).
    config.resumption = rustls::client::Resumption::disabled();
    // An IP address is a valid server name (no SNI is sent for it).
    let name = ServerName::try_from(host.to_string()).map_err(|e| CertificateError::Tls(std::io::Error::other(e)))?;
    let connected = tokio_rustls::TlsConnector::from(Arc::new(config)).connect(name, stream).await;
    let mut stream = match connected {
        Ok(stream) => stream,
        Err(e) => {
            if let (Some(pinned), Some(seen)) = (pinned, verifier.seen.lock().unwrap().as_deref()) {
                let seen = fingerprint(seen);
                if seen != pinned {
                    return Err(CertificateError::Changed { seen, pinned: pinned.to_string() });
                }
            }
            return Err(CertificateError::Tls(e));
        }
    };
    // ironrdp-tls rustls.rs: flush so that the handshake is complete.
    stream.flush().await.map_err(CertificateError::Tls)?;
    let certificate = stream
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|c| c.first())
        .map(|c| c.as_ref().to_vec())
        .ok_or_else(|| CertificateError::Tls(std::io::Error::other("the server sent no certificate")))?;
    let public_key = public_key_of(&certificate).ok_or_else(|| CertificateError::Tls(std::io::Error::other("the server's certificate could not be read")))?;
    let fingerprint = fingerprint(&certificate);
    Ok(Upgraded { stream, certificate, fingerprint, public_key })
}

/// The subjectPublicKey of a certificate (see the module's notes).
pub fn public_key_of(der: &[u8]) -> Option<Vec<u8>> {
    use x509_cert::der::Decode as _;
    let cert = x509_cert::Certificate::from_der(der).ok()?;
    cert.tbs_certificate().subject_public_key_info().subject_public_key.as_bytes().map(<[u8]>::to_vec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprints() {
        assert_eq!(fingerprint(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(display_fingerprint("ba78ff"), "BA:78:FF");
    }
}
