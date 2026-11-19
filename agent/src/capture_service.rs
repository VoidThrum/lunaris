//! `--capture-service` (Linux): the root helper that owns DRM/KMS and hands
//! the screen's framebuffers to the agent in the user's session, so Wayland
//! sessions need no portal prompt (lunaris-media `capture::drm_service`).
//!
//! Started by systemd (`lunaris-capture.service`, see
//! scripts/linux-host/lunaris-capture.service); it never encodes and never
//! opens a network socket. Security model: docs/linux-capture-service.md.

use clap::Parser;
use std::ffi::OsString;
use std::path::PathBuf;

/// Options of the capture service (separate from the agent's own options).
#[derive(Parser, Debug)]
#[command(name = "lunaris-agent --capture-service")]
struct CaptureServiceArgs {
    /// Run the capture service.
    #[arg(long)]
    capture_service: bool,

    /// Socket to listen on.
    #[arg(long, value_name = "PATH", default_value = lunaris_media::capture::drm_service::DEFAULT_SOCKET_PATH)]
    socket: PathBuf,

    /// Further executable allowed to receive frames (besides this binary);
    /// it and its directories must be owned by root and not writable by
    /// others. Repeatable.
    #[arg(long, value_name = "PATH")]
    allow_exe: Vec<PathBuf>,

    /// AppImage allowed to receive frames (matched through the `APPIMAGE`
    /// variable its runtime sets; must be owned by root). Repeatable.
    #[arg(long, value_name = "PATH")]
    allow_appimage: Vec<PathBuf>,

    /// Most clients served at once.
    #[arg(long, default_value_t = 4)]
    max_clients: usize,
}

/// `Some(exit code)` when `args` ask for the capture service (it then ran).
pub fn dispatch(args: &[OsString]) -> Option<std::process::ExitCode> {
    if !args.iter().skip(1).any(|a| a == "--capture-service") {
        return None;
    }
    let args = match CaptureServiceArgs::try_parse_from(args) {
        Ok(a) => a,
        Err(e) => e.exit(),
    };
    Some(run(args))
}

fn run(args: CaptureServiceArgs) -> std::process::ExitCode {
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
    let _ = tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()),
        ))
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr).without_time())
        .try_init();
    let config = lunaris_media::capture::drm_service::ServiceConfig {
        socket_path: args.socket,
        allow_exes: args.allow_exe,
        allow_appimages: args.allow_appimage,
        max_clients: args.max_clients.clamp(1, 16),
    };
    match lunaris_media::capture::drm_service::serve(config) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: capture service: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_flag_selects_the_service() {
        let plain: Vec<OsString> = ["lunaris-agent", "--cli"].iter().map(OsString::from).collect();
        assert!(dispatch(&plain).is_none());
        let parsed = CaptureServiceArgs::try_parse_from([
            "lunaris-agent",
            "--capture-service",
            "--socket",
            "/run/x.sock",
            "--allow-exe",
            "/usr/bin/lunaris-client",
            "--allow-appimage",
            "/opt/lunaris-agent/lunaris-agent.AppImage",
        ])
        .unwrap();
        assert!(parsed.capture_service);
        assert_eq!(parsed.socket, PathBuf::from("/run/x.sock"));
        assert_eq!(parsed.allow_exe, [PathBuf::from("/usr/bin/lunaris-client")]);
        assert_eq!(parsed.allow_appimage.len(), 1);
        let defaults = CaptureServiceArgs::try_parse_from(["x", "--capture-service"]).unwrap();
        assert_eq!(defaults.socket, PathBuf::from("/run/lunaris/capture.sock"));
        // The agent's own options are not accepted here.
        assert!(CaptureServiceArgs::try_parse_from(["x", "--capture-service", "--cli"]).is_err());
    }
}
