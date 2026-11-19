//! HTTP: the release check and the downloads (blocking reqwest, on the
//! updater's own threads, never inside a tokio runtime).

use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

use crate::release::Release;

fn client(timeout: Option<Duration>) -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .user_agent(crate::USER_AGENT)
        .connect_timeout(Duration::from_secs(20))
        .timeout(timeout)
        .https_only(true)
        .build()
        .map_err(|e| e.to_string())
}

/// Reads at most `cap` bytes of a response.
fn read_capped(mut r: impl Read, cap: u64) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    r.by_ref().take(cap + 1).read_to_end(&mut out).map_err(|e| e.to_string())?;
    if out.len() as u64 > cap {
        return Err("the answer is too large".into());
    }
    Ok(out)
}

/// The latest stable release.
pub fn fetch_latest() -> Result<Release, String> {
    let res = client(Some(Duration::from_secs(30)))?
        .get(crate::LATEST_RELEASE_API)
        .header("Accept", "application/vnd.github+json")
        .send()
        .map_err(|e| e.to_string())?;
    let status = res.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err("no release published yet".into());
    }
    if !status.is_success() {
        return Err(format!("GitHub answered {status}"));
    }
    let body = read_capped(res, 4 << 20)?;
    crate::release::parse_latest(&body)
}

/// A small release file (SHA256SUMS, its signature).
pub fn fetch_small(url: &str, cap: u64) -> Result<Vec<u8>, String> {
    check_url(url)?;
    let res = client(Some(Duration::from_secs(60)))?.get(url).send().map_err(|e| e.to_string())?;
    if !res.status().is_success() {
        return Err(format!("the download answered {}", res.status()));
    }
    read_capped(res, cap)
}

fn check_url(url: &str) -> Result<(), String> {
    if url.starts_with(crate::DOWNLOAD_PREFIX) {
        Ok(())
    } else {
        Err(format!("not a Lunaris release file: {url}"))
    }
}

/// Downloads `url` into `path` (a new file), returns its SHA-256.
/// `expected_size` (from GitHub, 0 = unknown) caps the size; `progress`
/// gets the bytes written so far.
pub fn download_to(url: &str, path: &Path, expected_size: u64, progress: &dyn Fn(u64)) -> Result<[u8; 32], String> {
    check_url(url)?;
    // Redirects to GitHub's download host are followed (reqwest's default
    // policy, https only).
    let mut res = client(None)?.get(url).send().map_err(|e| e.to_string())?;
    if !res.status().is_success() {
        return Err(format!("the download answered {}", res.status()));
    }
    let cap = if expected_size > 0 { expected_size } else { 2 << 30 };
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buf = vec![0u8; 256 * 1024];
    let mut total: u64 = 0;
    loop {
        let n = res.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > cap {
            return Err("the download is larger than the release says".into());
        }
        ctx.update(&buf[..n]);
        file.write_all(&buf[..n]).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        progress(total);
    }
    if expected_size > 0 && total != expected_size {
        return Err("the download is incomplete".into());
    }
    file.sync_all().map_err(|e| e.to_string())?;
    Ok(ctx.finish().as_ref().try_into().expect("SHA-256 is 32 bytes"))
}

#[cfg(test)]
mod tests {
    /// Asks GitHub for real: `cargo test -p updater --lib -- --ignored`.
    #[test]
    #[ignore]
    fn live_latest_release() {
        let r = super::fetch_latest().expect("latest release");
        eprintln!("{} {} assets={:?}", r.tag, r.page_url, r.asset_names());
        assert!(crate::version::parse(&r.tag).is_some());
        assert!(!r.assets.is_empty());
    }
}
