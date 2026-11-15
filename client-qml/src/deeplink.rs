//! `lunaris://connect?...` links: parsing with validation, and the decision
//! whether a link may connect at once. A link is data from a web page or any
//! local process, so it connects without a question only to the server the
//! user is signed in to. Any other server waits here until the user confirms
//! in a dialog, and its session gets no clipboard sync or file transfer.

use std::sync::Mutex;
use url::Url;

use crate::bridge::{self, AppArgs};

const MAX_URL_LEN: usize = 8192;
const RES_RANGE: std::ops::RangeInclusive<u32> = 64..=8192;
const FPS_RANGE: std::ops::RangeInclusive<u32> = 1..=480;
/// kbit/s.
const BITRATE_RANGE: std::ops::RangeInclusive<u32> = 100..=500_000;
const QUEUE_RANGE: std::ops::RangeInclusive<u32> = 1..=4096;
const MAX_TEXT_LEN: usize = 256;

const CODECS: &[&str] = &["auto", "h264", "h265", "hevc", "av1", "h264_444", "h264-444"];
const INPUT_PROTOCOLS: &[&str] = &["webrtc", "webtransport"];

/// A link to a server the user is not signed in to, waiting for the answer.
static PENDING: Mutex<Option<AppArgs>> = Mutex::new(None);
/// (server, token, host_id) of the confirmed untrusted session.
static UNTRUSTED: Mutex<Option<(String, String, String)>> = Mutex::new(None);

/// What the user is asked about.
pub struct LinkInfo {
    pub server_host: String,
    pub computer: String,
}

/// (secure, host, port) of a server address; `ws` is `http`, `wss` is `https`.
fn server_key(server: &str) -> Option<(bool, String, u16)> {
    let u = Url::parse(server.trim()).ok()?;
    let secure = match u.scheme() {
        "http" | "ws" => false,
        "https" | "wss" => true,
        _ => return None,
    };
    let host = u.host_str()?.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    let port = u.port_or_known_default()?;
    Some((secure, host, port))
}

pub fn same_server(a: &str, b: &str) -> bool {
    match (server_key(a), server_key(b)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

fn is_trusted(server: &str) -> bool {
    bridge::saved_server_url().is_some_and(|saved| same_server(&saved, server))
}

fn bounded(v: &str, range: &std::ops::RangeInclusive<u32>) -> Option<u32> {
    v.trim().parse::<u32>().ok().filter(|n| range.contains(n))
}

fn text(v: &str) -> String {
    v.chars().filter(|c| !c.is_control()).take(MAX_TEXT_LEN).collect()
}

pub fn parse_deeplink_url(url_str: &str) -> Option<AppArgs> {
    if url_str.len() > MAX_URL_LEN || !url_str.starts_with("lunaris://") {
        return None;
    }
    let parsed_url = Url::parse(url_str).ok()?;

    let mut host_id = String::new();
    let mut server_url = String::new();
    let mut token = String::new();

    let mut width = 1920; // Default resolution
    let mut height = 1080;
    let mut fps = 60;
    let mut bitrate = 8000;
    let mut codec = "h264".to_string();
    let mut app_id: Option<u32> = None;
    let mut mouse_queue_limit = 256;
    let mut host_name = "Desktop • Host".to_string();
    let mut disable_cuda = false;
    let mut input_protocol = "webrtc".to_string();
    let mut encoder: Option<String> = None;
    let mut display_id: Option<String> = None;
    let mut render_backend: Option<String> = None;

    for (k, v) in parsed_url.query_pairs() {
        match k.as_ref() {
            "host_id" => host_id = text(&v),
            "server" => server_url = v.trim().to_string(),
            "token" => token = v.trim().to_string(),
            "host_name" => {
                let t = text(&v);
                if !t.trim().is_empty() {
                    host_name = t;
                }
            }
            "input_protocol" => {
                let value = v.trim().to_lowercase();
                if INPUT_PROTOCOLS.contains(&value.as_str()) {
                    input_protocol = value;
                }
            }
            "encoder" => {
                let value = text(&v).to_lowercase();
                if !value.is_empty() && value != "auto" {
                    encoder = Some(value);
                }
            }
            "display" | "display_id" => {
                let value = text(&v);
                if !value.is_empty() && value != "default" {
                    display_id = Some(value);
                }
            }
            "render_backend" | "gpu_backend" | "decode_backend" => {
                render_backend = Some(text(&v));
            }
            "app_id" => {
                if let Ok(id) = v.parse::<u32>() {
                    app_id = Some(id);
                }
            }
            "res" => {
                if let Some((w, h)) = v.split_once('x') {
                    if let (Some(w), Some(h)) = (bounded(w, &RES_RANGE), bounded(h, &RES_RANGE)) {
                        width = w;
                        height = h;
                    }
                }
            }
            "fps" => {
                if let Some(f) = bounded(&v, &FPS_RANGE) {
                    fps = f;
                }
            }
            "bitrate" => {
                if let Some(b) = bounded(&v, &BITRATE_RANGE) {
                    bitrate = b;
                }
            }
            "codec" => {
                let value = v.trim().to_lowercase();
                if CODECS.contains(&value.as_str()) {
                    codec = value;
                }
            }
            "mouse_queue_limit" => {
                if let Some(limit) = bounded(&v, &QUEUE_RANGE) {
                    mouse_queue_limit = limit;
                }
            }
            "disable_cuda" => {
                if let Ok(val) = v.parse::<bool>() {
                    disable_cuda = val;
                } else if v.as_ref() == "1" || v.as_ref() == "true" {
                    disable_cuda = true;
                }
            }
            _ => {}
        }
    }

    if server_key(&server_url).is_none() {
        return None;
    }

    let render_backend = render_backend
        .map(|value| bridge::normalize_render_backend(&value, disable_cuda))
        .unwrap_or_else(|| bridge::render_backend_from_disable_cuda(disable_cuda));
    disable_cuda = bridge::render_backend_disables_cuda(&render_backend);

    if host_id.is_empty() || token.is_empty() {
        return None;
    }
    Some(AppArgs {
        host_id,
        server_url,
        token,
        width,
        height,
        fps,
        bitrate,
        codec,
        app_id,
        mouse_queue_limit,
        host_name,
        disable_cuda,
        render_backend,
        input_protocol,
        encoder,
        display_id,
    })
}

/// What a link asks for: connect now (it is for the signed-in server), or
/// wait for the user's answer.
pub enum Verdict {
    Connect(AppArgs),
    NeedsConfirmation,
    Invalid,
}

/// Judge a link. Same rules for the command line and the single-instance
/// port. A link needing confirmation is stored and replaces an older one.
pub fn judge(url: &str) -> Verdict {
    let Some(args) = parse_deeplink_url(url) else {
        return Verdict::Invalid;
    };
    if is_trusted(&args.server_url) {
        *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = None;
        Verdict::Connect(args)
    } else {
        *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = Some(args);
        Verdict::NeedsConfirmation
    }
}

/// The link waiting for an answer, for the dialog.
pub fn pending_info() -> Option<LinkInfo> {
    let lock = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    let a = lock.as_ref()?;
    let (secure, host, port) = server_key(&a.server_url)?;
    let default_port = if secure { 443 } else { 80 };
    let server_host = if port == default_port { host } else { format!("{}:{}", host, port) };
    Some(LinkInfo { server_host, computer: a.host_name.clone() })
}

/// The user said yes: the link becomes the session to start. Its session has
/// clipboard sync and file transfer off.
pub fn confirm_pending() -> bool {
    let Some(args) = PENDING.lock().unwrap_or_else(|e| e.into_inner()).take() else {
        return false;
    };
    *UNTRUSTED.lock().unwrap_or_else(|e| e.into_inner()) =
        Some((args.server_url.clone(), args.token.clone(), args.host_id.clone()));
    *bridge::ACTIVE_CONFIG.lock().unwrap_or_else(|e| e.into_inner()) = Some(args);
    true
}

pub fn cancel_pending() {
    *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// A link to the signed-in server starts a normal session.
pub fn mark_trusted_session() {
    *UNTRUSTED.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Is the running session the one started from an unconfirmed-server link?
/// (A session the user starts from the app has other credentials.)
pub fn session_is_untrusted() -> bool {
    let u = UNTRUSTED.lock().unwrap_or_else(|e| e.into_inner());
    let Some((server, token, host)) = u.as_ref() else { return false };
    let active = bridge::ACTIVE_CONFIG.lock().unwrap_or_else(|e| e.into_inner());
    match active.as_ref() {
        Some(a) => a.server_url == *server && a.token == *token && a.host_id == *host,
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "lunaris://connect?host_id=h1&token=t&server=";

    fn p(extra: &str) -> Option<AppArgs> {
        parse_deeplink_url(&format!("{}wss%3A%2F%2Fsrv.example%3A8443{}", BASE, extra))
    }

    #[test]
    fn accepts_a_good_link() {
        let a = p("&res=1280x720&fps=30&bitrate=5000&codec=AV1&host_name=PC").unwrap();
        assert_eq!((a.width, a.height, a.fps, a.bitrate), (1280, 720, 30, 5000));
        assert_eq!(a.codec, "av1");
        assert_eq!(a.host_name, "PC");
        assert_eq!(a.server_url, "wss://srv.example:8443");
    }

    #[test]
    fn rejects_bad_servers() {
        for s in ["file%3A%2F%2F%2Fetc", "javascript%3Aalert(1)", "ws%3A%2F%2F", "", "ftp%3A%2F%2Fa"] {
            assert!(parse_deeplink_url(&format!("{}{}", BASE, s)).is_none(), "{}", s);
        }
        assert!(parse_deeplink_url("http://x/?host_id=h&token=t&server=ws://a").is_none());
        assert!(parse_deeplink_url("lunaris://connect?server=ws%3A%2F%2Fa").is_none());
    }

    #[test]
    fn out_of_range_numbers_keep_defaults() {
        let a = p("&res=99999x1&fps=0&bitrate=4294967295&mouse_queue_limit=0").unwrap();
        assert_eq!((a.width, a.height, a.fps, a.bitrate, a.mouse_queue_limit), (1920, 1080, 60, 8000, 256));
        let a = p("&fps=481").unwrap();
        assert_eq!(a.fps, 60);
        let a = p("&fps=480&res=64x8192&bitrate=500000").unwrap();
        assert_eq!((a.fps, a.width, a.height, a.bitrate), (480, 64, 8192, 500_000));
    }

    #[test]
    fn unknown_enums_fall_back() {
        let a = p("&codec=evil&input_protocol=x&render_backend=zzz").unwrap();
        assert_eq!(a.codec, "h264");
        assert_eq!(a.input_protocol, "webrtc");
        assert_eq!(a.render_backend, bridge::render_backend_from_disable_cuda(false));
    }

    #[test]
    fn oversized_and_garbage_do_not_panic() {
        assert!(parse_deeplink_url(&format!("lunaris://x?{}", "a".repeat(20000))).is_none());
        assert!(parse_deeplink_url("lunaris://\u{0}\u{ffff}%zz?&&==").is_none());
        assert!(parse_deeplink_url("").is_none());
    }

    #[test]
    fn same_server_normalises() {
        assert!(same_server("http://Host:8080", "ws://host:8080/ws"));
        assert!(same_server("https://a.example", "wss://a.example:443"));
        assert!(!same_server("http://a.example", "https://a.example"));
        assert!(!same_server("http://a.example:8080", "http://a.example:8081"));
        assert!(!same_server("http://a.example", "http://b.example"));
        assert!(!same_server("nonsense", "nonsense"));
    }
}
