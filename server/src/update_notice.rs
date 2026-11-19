//! The standalone server's "a new version exists" notice: a log line at
//! start (after a short delay) and then daily when GitHub has a newer
//! stable release. The server never updates itself (packages, Docker
//! images and services are updated by whoever runs them).
//!
//! Only in the standalone `server` binary ([`enable`] from its `main`), not
//! in the app's `lunaris server` child (the app has its own updater), and
//! not with `LUNARIS_NO_UPDATE_CHECK` set. Nothing about this server is
//! sent: one HTTPS GET with a generic User-Agent.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::rustls;
use tracing::{debug, warn};

static ENABLED: AtomicBool = AtomicBool::new(false);

const HOST: &str = "api.github.com";
const PATH: &str = "/repos/collyn/lunaris/releases/latest";

/// Turns the notice on (the standalone server binary).
pub fn enable() {
    ENABLED.store(true, Ordering::SeqCst);
}

/// Starts the daily check on the current tokio runtime when enabled.
pub(crate) fn spawn() {
    if !ENABLED.load(Ordering::SeqCst) || std::env::var_os("LUNARIS_NO_UPDATE_CHECK").is_some() {
        return;
    }
    tokio::spawn(async {
        tokio::time::sleep(Duration::from_secs(30)).await;
        loop {
            match tokio::time::timeout(Duration::from_secs(60), fetch_latest()).await {
                Ok(Ok(release)) if updater::version::is_newer(updater::CURRENT_VERSION, &release.tag) => warn!(
                    "Lunaris {} is available (this server is {}): {}",
                    release.version,
                    updater::CURRENT_VERSION,
                    release.page_url
                ),
                Ok(Ok(_)) => debug!("Update check: this server is the latest version"),
                Ok(Err(e)) => debug!("Update check failed: {e}"),
                Err(_) => debug!("Update check failed: timed out"),
            }
            tokio::time::sleep(Duration::from_secs(24 * 3600)).await;
        }
    });
}

async fn fetch_latest() -> Result<updater::release::Release, String> {
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(cert);
    }
    if roots.is_empty() {
        return Err("no CA certificates on this system".into());
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let tcp = tokio::net::TcpStream::connect((HOST, 443)).await.map_err(|e| e.to_string())?;
    let name = rustls::pki_types::ServerName::try_from(HOST).map_err(|e| e.to_string())?;
    let mut tls = tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(name, tcp)
        .await
        .map_err(|e| e.to_string())?;
    let request = format!(
        "GET {PATH} HTTP/1.1\r\nHost: {HOST}\r\nUser-Agent: {}\r\nAccept: application/vnd.github+json\r\nConnection: close\r\n\r\n",
        updater::USER_AGENT
    );
    tls.write_all(request.as_bytes()).await.map_err(|e| e.to_string())?;
    let mut response = Vec::new();
    (&mut tls).take(4 << 20).read_to_end(&mut response).await.map_err(|e| e.to_string())?;
    let body = http_body(&response)?;
    updater::release::parse_latest(&body)
}

/// The body of an HTTP/1.1 200 response (plain or chunked).
fn http_body(response: &[u8]) -> Result<Vec<u8>, String> {
    let split = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("incomplete answer")?;
    let head = String::from_utf8_lossy(&response[..split]).to_ascii_lowercase();
    let body = &response[split + 4..];
    let status = head.lines().next().unwrap_or("");
    if status.split_whitespace().nth(1) != Some("200") {
        return Err(format!("GitHub answered {}", status.trim()));
    }
    if head.lines().any(|l| l.starts_with("transfer-encoding:") && l.contains("chunked")) {
        dechunk(body)
    } else {
        Ok(body.to_vec())
    }
}

fn dechunk(mut data: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let line_end = data.windows(2).position(|w| w == b"\r\n").ok_or("bad chunk")?;
        let size_text = String::from_utf8_lossy(&data[..line_end]);
        let size = usize::from_str_radix(size_text.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| "bad chunk size")?;
        data = &data[line_end + 2..];
        if size == 0 {
            return Ok(out);
        }
        if data.len() < size + 2 {
            return Err("truncated chunk".into());
        }
        out.extend_from_slice(&data[..size]);
        data = &data[size + 2..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_and_chunked_bodies() {
        let plain = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}";
        assert_eq!(http_body(plain).unwrap(), b"{}");
        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n{\"a\"\r\n3;x=y\r\n:1}\r\n0\r\n\r\n";
        assert_eq!(http_body(chunked).unwrap(), b"{\"a\":1}");
        assert!(http_body(b"HTTP/1.1 403 Forbidden\r\n\r\nrate limited").is_err());
        assert!(http_body(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nff\r\nab").is_err());
    }
}
