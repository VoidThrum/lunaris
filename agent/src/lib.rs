
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tracing::{debug, error, info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use common::{
    AgentMessage, RtcSdpType, RtcSessionDescription, ServerToAgentMessage, SignalingMessage,
};
use webrtc::peer_connection::{PeerConnection, RTCIceCandidateInit, RTCSessionDescription};

pub mod approval;
mod bridge;
pub mod bridge_agent;
mod buffer;
pub mod direct;
mod files;
mod gamepad;
mod hub;
mod input;
pub mod pairing;
mod rtcp_forward;
pub mod connector;
pub mod rustdesk;
mod share;
mod video;
#[cfg(target_os = "windows")]
pub mod win_service;


pub static CONNECTED_TO_SERVER: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
pub static AGENT_ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub static LAST_ERROR: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
pub static LAST_STREAM_STOP_TIME_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

fn get_current_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

use crate::bridge::{
    codec_supported_for_realtime, hardware_encoder_opens_blocking, setup_bridge_session, BridgeSession,
};
use crate::pairing::AgentConfig;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(long, default_value = "127.0.0.1")]
    host_ip: String,

    #[arg(long)]
    pair: bool,

    #[arg(long)]
    pin: Option<String>,

    #[arg(long)]
    server: Option<String>,

    #[arg(long)]
    name: Option<String>,

    #[arg(long)]
    cli: bool,

    /// Config file (default: %APPDATA%\\lunaris-agent / ~/.config/lunaris-agent,
    /// or ./agent_config.json when it exists)
    #[arg(long)]
    config: Option<String>,

    #[arg(long)]
    import_config: Option<String>,

    #[arg(long)]
    minimized: bool,

    /// Try to open each encoder at 1920x1080@60 and print which ones work, then exit
    #[arg(long)]
    probe_encoders: bool,

    /// Accept direct connections (no server) and print the addresses and the
    /// access password (saved in the config, generated on first use)
    #[arg(long)]
    direct: bool,

    /// Port for direct connections (default 8091)
    #[arg(long)]
    direct_port: Option<u16>,

    /// The app shows the "accept this connection?" questions: they go to
    /// stdout and the answers come on stdin (see approval.rs); logs go to
    /// stderr.
    #[arg(long)]
    approval_stdio: bool,

    /// The Lunaris app's helper for computers of other types it added for
    /// itself (see bridge_agent.rs): this computer only, sources only.
    #[arg(long, hide = true)]
    bridge_only: bool,

    /// Windows: started by the Service Control Manager (see win_service.rs)
    #[cfg(target_os = "windows")]
    #[arg(long, hide = true)]
    service_run: bool,

    /// Windows: send Ctrl+C to the host process with this id (the service
    /// runs this program again for it, see win_service.rs)
    #[cfg(target_os = "windows")]
    #[arg(long, hide = true, value_name = "PID")]
    terminate: Option<u32>,

    /// Windows: install the service that runs the host as SYSTEM in the
    /// console session, start it and make it start at boot (needs an
    /// elevated prompt)
    #[cfg(target_os = "windows")]
    #[arg(long)]
    install_service: bool,

    /// Windows: stop and remove that service (needs an elevated prompt)
    #[cfg(target_os = "windows")]
    #[arg(long)]
    uninstall_service: bool,
}

/// `--probe-encoders`: open each codec's encoder for real and report it.
fn probe_encoders() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let _ = tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "off,ffmpeg=warn,lunaris_media=error".into()),
        ))
        .with(tracing_subscriber::fmt::layer())
        .try_init();
    for codec in [
        lunaris_media::VideoCodec::H264,
        lunaris_media::VideoCodec::H265,
        lunaris_media::VideoCodec::AV1,
    ] {
        match lunaris_media::encode::probe_encoder(codec, 1920, 1080, 60) {
            Some(e) => println!("{:?}: {} ({:?})", codec, e.name, e.hw_type),
            None => println!("{:?}: no working encoder", codec),
        }
    }
    Ok(())
}

/// Runs the agent with command-line arguments (the first one is the program
/// name), headless, as `lunaris agent` does.
pub fn run<I, T>(args: I) -> std::process::ExitCode
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    run_with_gui::<I, T, fn() -> std::io::Stderr>(args, None)
}

/// A desktop window around the agent: where log output goes, and the
/// function that runs the window (the standalone `agent` binary has one).
pub struct Gui<W> {
    pub log_writer: W,
    pub run: fn(minimized: bool),
}

/// Like [`run`]; without `--cli` (or `--pair`) it opens `gui` instead.
pub fn run_with_gui<I, T, W>(args: I, gui: Option<Gui<W>>) -> std::process::ExitCode
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
    W: for<'a> tracing_subscriber::fmt::MakeWriter<'a> + Send + Sync + 'static,
{
    let args = match Args::try_parse_from(args) {
        Ok(args) => args,
        Err(e) => e.exit(),
    };
    // Service commands (Windows): same order as sunshinesvc.cpp main().
    #[cfg(target_os = "windows")]
    {
        if let Some(pid) = args.terminate {
            win_service::terminate_main(pid);
        }
        if args.install_service || args.uninstall_service {
            let result = if args.install_service {
                win_service::install_service(crate::pairing::config_path())
            } else {
                win_service::uninstall_service()
            };
            return match result {
                Ok(()) => std::process::ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::ExitCode::FAILURE
                }
            };
        }
        if args.service_run {
            win_service::service_run_main();
        }
    }
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: cannot start the async runtime: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    match runtime.block_on(main_async(args, gui)) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn main_async<W>(args: Args, gui: Option<Gui<W>>) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    W: for<'a> tracing_subscriber::fmt::MakeWriter<'a> + Send + Sync + 'static,
{
    if args.probe_encoders {
        return probe_encoders();
    }
    crate::pairing::set_config_path(args.config.clone());
    let config_path = crate::pairing::config_path().to_string();

    // If import_config is specified, we perform the import first
    if let Some(ref import_path) = args.import_config {
        let _ = tracing_subscriber::registry()
            .with(tracing_subscriber::EnvFilter::new(
                std::env::var("RUST_LOG").unwrap_or_else(|_| "info,agent=info,webrtc_sctp=off,dtls=error,rtc_sctp=off,rtc_dtls=error".into()),
            ))
            .with(tracing_subscriber::fmt::layer())
            .try_init();

        info!(
            "Importing configuration from {} into {}...",
            import_path, config_path
        );
        if let Err(e) = crate::pairing::import_config_file(import_path, &config_path) {
            error!("Failed to import configuration: {:?}", e);
            std::process::exit(1);
        }
        info!("Configuration imported and merged successfully!");
    }

    if let Some(gui) = gui {
        if !args.cli && !args.pair {
            // Log output goes to the window.
            let _ = tracing_subscriber::registry()
                .with(tracing_subscriber::EnvFilter::new(
                    std::env::var("RUST_LOG").unwrap_or_else(|_| {
                        {
                            if cfg!(debug_assertions) {
                                "info,agent=info,webrtc_sctp=off,dtls=error,rtc_sctp=off,rtc_dtls=error"
                            } else {
                                "info,agent=info,lunaris_media=info,webrtc_sctp=off,dtls=error,webrtc_ice=warn,rtc_sctp=off,rtc_dtls=error,rtc_ice=warn"
                            }
                        }
                        .into()
                    }),
                ))
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_writer(gui.log_writer)
                        .with_ansi(false),
                )
                .try_init();

            info!("Launching Lunaris Agent in Desktop GUI mode...");
            (gui.run)(args.minimized);
            return Ok(());
        }
    }

    // CLI mode execution
    // Init standard tracing logger
    let _ = tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(
            std::env::var("RUST_LOG").unwrap_or_else(|_| {
                "info,agent=info,webrtc_sctp=off,dtls=error,rtc_sctp=off,rtc_dtls=error".into()
            }),
        ))
        .with(tracing_subscriber::fmt::layer().with_writer(if args.approval_stdio || args.bridge_only {
            tracing_subscriber::fmt::writer::BoxMakeWriter::new(std::io::stderr)
        } else {
            tracing_subscriber::fmt::writer::BoxMakeWriter::new(std::io::stdout)
        }))
        .try_init();
    if args.approval_stdio {
        approval::attach_stdio();
    }

    info!("Launching Lunaris Agent in CLI mode...");

    let name = args.name.clone().unwrap_or_else(|| {
        hostname::get()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "LunarisHost".to_string())
    });

    if args.pair {
        info!("lunaris-media is the streaming backend.");
        return Ok(());
    }

    if args.bridge_only {
        // Its own config file, which may not be there yet (no computer
        // added): then there is nothing to show, and every session says so.
        let config = crate::pairing::load_config(&config_path).unwrap_or_else(|_| crate::pairing::new_config());
        return bridge_agent::run(config, name).await;
    }

    // Load or create agent config
    let mut config = match crate::pairing::load_config(&config_path) {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to load agent config from {}: {:?}", config_path, e);
            std::process::exit(1);
        }
    };

    if args.direct || args.direct_port.is_some() {
        config.direct_enabled = true;
        if let Some(port) = args.direct_port {
            config.direct_port = port;
        }
        if crate::pairing::ensure_direct_password(&mut config) {
            if let Err(e) = crate::pairing::save_config(&config, &config_path) {
                warn!("Could not save the direct-access password to {}: {}", config_path, e);
            }
        }
    }
    if config.direct_enabled {
        println!("Direct connections — connect a client to one of:");
        for addr in direct::local_addresses().await {
            println!("    {}:{}", addr, config.direct_port);
        }
        // Printed, not logged, and only to a terminal: when stdout is a file
        // or a pipe (the Windows service's log, the app) it would be stored.
        use std::io::IsTerminal;
        if std::io::stdout().is_terminal() {
            println!("Access password: {}", config.direct_password);
        } else {
            println!("Access password: not shown here (output is not a terminal); it is in {}", config_path);
        }
    }

    run_agent(config, name).await
}

pub async fn run_agent(
    mut config: AgentConfig,
    name: String,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    CONNECTED_TO_SERVER.store(false, std::sync::atomic::Ordering::SeqCst);

    let hub = hub::Hub::new(config.client_unique_id.clone());
    let (agent_tx, router) = hub::spawn_router(hub.clone());
    let (inbound_tx, inbound_rx) = mpsc::unbounded_channel::<SignalingMessage>();
    let mut tasks = AbortOnDrop(vec![router]);
    // A set-up of a computer being added (a pairing that waits for its PIN)
    // does not outlive the agent that started it.
    let _setups = crate::connector::sources::StopSetupsOnDrop(crate::pairing::config_path().to_string());

    if config.direct_enabled {
        if crate::pairing::ensure_direct_password(&mut config) {
            if let Err(e) = crate::pairing::save_config(&config, crate::pairing::config_path()) {
                warn!("Could not save the new direct-access password: {}", e);
            }
        }
        let port = config.direct_port;
        let direct_config = direct::DirectConfig {
            port,
            password: config.direct_password.clone(),
            approve_mode: approval::ApproveMode::parse(&config.approve_mode),
            name: name.clone(),
        };
        let hub = hub.clone();
        let inbound_tx = inbound_tx.clone();
        tasks.0.push(tokio::spawn(async move {
            if let Err(e) = direct::run_listener(direct_config, hub, inbound_tx).await {
                let msg = format!("Direct connections unavailable: cannot listen on port {}: {}", port, e);
                error!("{}", msg);
                if let Ok(mut last) = LAST_ERROR.lock() {
                    *last = Some(msg);
                }
            }
        }));
    }

    if let Some(port) = config.rustdesk_port {
        // The RustDesk client logs in with the access password.
        if crate::pairing::ensure_direct_password(&mut config) {
            if let Err(e) = crate::pairing::save_config(&config, crate::pairing::config_path()) {
                warn!("Could not save the new access password: {}", e);
            }
        }
        let rustdesk_config = rustdesk::RustdeskConfig {
            port,
            password: config.direct_password.clone(),
            approve_mode: approval::ApproveMode::parse(&config.approve_mode),
            allow_clipboard: config.allow_clipboard,
            agent_id: config.client_unique_id.clone(),
            name: name.clone(),
            // RustDesk's file manager: its own switch, and never when file
            // transfer is off on this computer; held to `file_access`.
            file_transfer: config.rustdesk_file_transfer && config.allow_file_transfer,
            file_access: files::Access::from_config(&file_access_setting(&config)),
            file_home: None,
        };
        tasks.0.push(tokio::spawn(async move {
            if let Err(e) = rustdesk::run_listener(rustdesk_config).await {
                error!("RustDesk compatibility unavailable: cannot listen on port {}: {}", port, e);
            }
        }));
    }

    if config.server_url.trim().is_empty() {
        info!("No Lunaris server configured: direct connections only.");
    } else {
        if crate::pairing::ensure_agent_key(&mut config) {
            let key = config.agent_key.clone();
            if let Err(e) = crate::pairing::update_config_file(crate::pairing::config_path(), &config, |c| c.agent_key = key) {
                // A key the server stores but this agent forgets on restart
                // would lock it out: register without one (as older agents do).
                error!(
                    "Could not save the agent's server key to {} ({}): connecting without a key until it can be saved",
                    crate::pairing::config_path(),
                    e
                );
                config.agent_key.clear();
            }
        }
        tasks.0.push(tokio::spawn(run_server_connection(
            config.clone(),
            name.clone(),
            hub.clone(),
            inbound_tx.clone(),
        )));
    }

    info!("Starting Host Agent: {} ({})", name, config.client_unique_id);
    // `inbound_tx` stays alive here, so the session logic runs until this
    // future is dropped (GUI stop) even when nothing is connected.
    run_session_logic(config, agent_tx, inbound_rx).await;
    drop(inbound_tx);
    drop(tasks);
    Ok(())
}

/// Aborts the agent's background tasks when `run_agent` stops.
struct AbortOnDrop(Vec<tokio::task::JoinHandle<()>>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

/// Codecs this computer encodes in real time, as the server's
/// `codec_support` bits. H.264 software is a practical fallback; H.265/AV1
/// software is too slow for interactive remote desktop, so only advertise
/// those when a realtime-capable hardware encoder exists.
pub(crate) fn codec_support_bits() -> u32 {
    // Probing the encoders takes a while: once per run.
    static BITS: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *BITS.get_or_init(probe_codec_support)
}

fn probe_codec_support() -> u32 {
    let encoders = lunaris_media::encode::list_available_encoders();
    let h264 = codec_supported_for_realtime(&encoders, lunaris_media::VideoCodec::H264, false);
    // The cached probe (1080p60, as `--probe-encoders`) decides H.265 and AV1.
    let h265 = codec_supported_for_realtime(
        &encoders,
        lunaris_media::VideoCodec::H265,
        hardware_encoder_opens_blocking(lunaris_media::VideoCodec::H265, 1920, 1080, 60),
    );
    let av1 = codec_supported_for_realtime(
        &encoders,
        lunaris_media::VideoCodec::AV1,
        hardware_encoder_opens_blocking(lunaris_media::VideoCodec::AV1, 1920, 1080, 60),
    );
    let mut bits: u32 = 0;
    if h264 {
        bits |= 262145;
    }
    if h265 {
        bits |= 1573632;
    }
    if av1 {
        bits |= 6488064;
    }
    bits
}

/// Keeps a connection to the signaling server, reconnecting every 3 s.
async fn run_server_connection(
    mut config: AgentConfig,
    name: String,
    hub: Arc<hub::Hub>,
    inbound_tx: mpsc::UnboundedSender<SignalingMessage>,
) {
    let mut last_error = String::new();
    loop {
        match connect_server_once(&mut config, &name, &hub, &inbound_tx).await {
            Ok(()) => {
                info!("Disconnected from the signaling server. Reconnecting in 3 seconds...");
                last_error.clear();
            }
            Err(e) => {
                let msg = format!("Signaling server connection failed: {}", e);
                // Report each new error once instead of every 3 seconds.
                if msg != last_error {
                    error!("{}. Retrying every 3 seconds...", msg);
                    if let Ok(mut last) = LAST_ERROR.lock() {
                        *last = Some(msg.clone());
                    }
                    last_error = msg;
                } else {
                    debug!("{}", msg);
                }
            }
        }
        hub.set_server(None);
        CONNECTED_TO_SERVER.store(false, std::sync::atomic::Ordering::SeqCst);
        tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
    }
}

async fn connect_server_once(
    config: &mut AgentConfig,
    name: &str,
    hub: &Arc<hub::Hub>,
    inbound_tx: &mpsc::UnboundedSender<SignalingMessage>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let codec_support = Some(codec_support_bits());

    let mut base_ws_url = config.server_url.clone();
    if base_ws_url.starts_with("http://") {
        base_ws_url = base_ws_url.replacen("http://", "ws://", 1);
    } else if base_ws_url.starts_with("https://") {
        base_ws_url = base_ws_url.replacen("https://", "wss://", 1);
    } else if !base_ws_url.starts_with("ws://") && !base_ws_url.starts_with("wss://") {
        base_ws_url = format!("ws://{}", base_ws_url);
    }

    // Connect to Signaling Server
    let mut server_ws_url = if let Some(support) = codec_support {
        format!(
            "{}/ws/agent?id={}&name={}&codec_support={}&token={}",
            base_ws_url.trim_end_matches('/'),
            config.client_unique_id,
            urlencoding::encode(name),
            support,
            urlencoding::encode(&config.server_token)
        )
    } else {
        format!(
            "{}/ws/agent?id={}&name={}&token={}",
            base_ws_url.trim_end_matches('/'),
            config.client_unique_id,
            urlencoding::encode(name),
            urlencoding::encode(&config.server_token)
        )
    };

    // The agent's own key (servers that don't know it ignore it).
    if !config.agent_key.is_empty() {
        server_ws_url.push_str("&key=");
        server_ws_url.push_str(&urlencoding::encode(&config.agent_key));
    }

    // Never log server_ws_url itself: it carries the agent token and key.
    debug!("Connecting to signaling server at {}", base_ws_url);
    // TLS first (the certificate is checked or pinned before the token is
    // sent), then the WebSocket handshake over it.
    let stream = server_stream(config, &base_ws_url).await?;
    let (ws_stream, _) = tokio_tungstenite::client_async(server_ws_url, stream)
        .await
        .map_err(|e| anyhow::anyhow!("WebSocket handshake with {} failed: {}", base_ws_url, e))?;
    info!("Connected to signaling server!");
    CONNECTED_TO_SERVER.store(true, std::sync::atomic::Ordering::SeqCst);

    let (mut ws_write, mut ws_read) = ws_stream.split();
    let (server_tx, mut server_rx) = mpsc::unbounded_channel::<AgentMessage>();
    hub.set_server(Some(server_tx.clone()));
    // The server is told which other computers this one shows, and which
    // kinds it can show (again whenever it changes them).
    let manageable = config.server_sessions.trim() != "ask";
    let server_tx_for_sources = server_tx.clone();
    // Again when a set-up (a pairing) has added one, whoever still asks about it.
    let _announcing = crate::connector::sources::announce_changes(server_tx.clone(), manageable);
    let _ = server_tx.send(crate::connector::sources::announcement(manageable));
    let gate = ServerGate::new(config.server_sessions.trim() == "ask", server_tx, inbound_tx.clone());

    // Outbound: the hub routes the session logic's replies for server clients here.
    let write_task = tokio::spawn(async move {
        while let Some(msg) = server_rx.recv().await {
            match serde_json::to_string(&msg) {
                Ok(json) => {
                    if let Err(e) = ws_write.send(WsMessage::Text(json.into())).await {
                        error!("WebSocket write error: {:?}", e);
                        break;
                    }
                }
                Err(e) => error!("Failed to serialize outbound message: {:?}", e),
            }
        }
    });

    while let Some(message_result) = ws_read.next().await {
        let text = match message_result {
            Ok(WsMessage::Text(text)) => text,
            Ok(_) => continue,
            Err(e) => {
                error!("WebSocket read error: {:?}", e);
                break;
            }
        };
        match serde_json::from_str::<ServerToAgentMessage>(&text) {
            Ok(ServerToAgentMessage::Registered { success }) => {
                info!("Registration success: {}", success);
            }
            Ok(ServerToAgentMessage::Signaling(sig)) => gate.pass(sig),
            Ok(ServerToAgentMessage::ChangeSource { request_id, change, requester }) => {
                // A computer that asks at its screen before each connection
                // is not changed from the server: its sources are added in
                // the Lunaris app on it. Adding can wait a few seconds for a
                // set-up's first step, so not on this loop.
                let tx = server_tx_for_sources.clone();
                tokio::task::spawn_blocking(move || {
                    for message in crate::connector::sources::reply(request_id, &change, requester.as_deref(), "the server", manageable) {
                        let _ = tx.send(message);
                    }
                });
            }
            // What a newer server may send is not written out: it can hold
            // what a person typed.
            Err(e) => error!("A message from the server was not understood: {}", e),
        }
    }

    write_task.abort();
    hub.set_server(None);
    CONNECTED_TO_SERVER.store(false, std::sync::atomic::Ordering::SeqCst);
    Ok(())
}

/// Sessions that arrive through the server: passed on as they come
/// (`server_sessions` "trusted"), or, with "ask", each client connection
/// that wants the screen or files is put to the person here first, the way
/// direct.rs does for a server linked without the password.
struct ServerGate {
    ask: bool,
    server_tx: mpsc::UnboundedSender<AgentMessage>,
    inbound_tx: mpsc::UnboundedSender<SignalingMessage>,
    /// What the person here granted each client connection (server
    /// connection id): the screen and files are asked for separately.
    accepted: Arc<std::sync::Mutex<std::collections::HashSet<(String, approval::Kind)>>>,
    /// Questions open, each withdrawn through its Notify when the client
    /// leaves first.
    asking: Arc<std::sync::Mutex<std::collections::HashMap<(String, approval::Kind), Arc<tokio::sync::Notify>>>>,
    ask_fn: AskFn,
}

/// Puts a question to the person here: None when withdrawn (the Notify).
type AskFn = Arc<
    dyn Fn(String, approval::Kind, Arc<tokio::sync::Notify>) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<approval::Decision>> + Send>>
        + Send
        + Sync,
>;

impl ServerGate {
    fn new(
        ask: bool,
        server_tx: mpsc::UnboundedSender<AgentMessage>,
        inbound_tx: mpsc::UnboundedSender<SignalingMessage>,
    ) -> Self {
        if ask {
            info!("Sessions through the server are accepted at this computer (server_sessions = \"ask\")");
        }
        let ask_fn: AskFn = Arc::new(|requester, kind, cancel| {
            Box::pin(async move { approval::ask_unless(&requester, kind, cancel.notified()).await })
        });
        Self { ask, server_tx, inbound_tx, accepted: Arc::default(), asking: Arc::default(), ask_fn }
    }

    fn pass(&self, sig: SignalingMessage) {
        if !self.ask {
            let _ = self.inbound_tx.send(sig);
            return;
        }
        let Some(client) = hub::target_of(&sig).map(str::to_owned) else {
            let _ = self.inbound_tx.send(sig);
            return;
        };
        if let SignalingMessage::EndSession { .. } = sig {
            // The client is gone: withdraw its questions, forget its grants.
            // Under the lock, so an answer arriving now is ordered before or
            // after this, never in between.
            let mut asking = self.asking.lock().unwrap();
            asking.retain(|(c, _), cancel| {
                if *c == client {
                    cancel.notify_one();
                    false
                } else {
                    true
                }
            });
            self.accepted.lock().unwrap().retain(|(c, _)| *c != client);
            let _ = self.inbound_tx.send(sig);
            return;
        }
        let kind = approval::kind_of(&sig);
        let pass = {
            let accepted = self.accepted.lock().unwrap();
            match (kind, &sig) {
                (Some(kind), _) => accepted.contains(&(client.clone(), kind)),
                // Stopping the stream is for a client allowed to see it.
                (None, SignalingMessage::StopActiveStream { .. }) => {
                    accepted.contains(&(client.clone(), approval::Kind::Control))
                }
                // Answers, candidates and the like: from a client granted something.
                (None, _) => approval::allowed_before_approval(&sig) || accepted.iter().any(|(c, _)| *c == client),
            }
        };
        if pass {
            let _ = self.inbound_tx.send(sig);
            return;
        }
        let Some(kind) = kind else {
            debug!("Server client {} not accepted yet: dropping a {:?} message", client, std::mem::discriminant(&sig));
            return;
        };
        let key = (client.clone(), kind);
        let cancel = Arc::new(tokio::sync::Notify::new());
        {
            let mut asking = self.asking.lock().unwrap();
            if asking.contains_key(&key) {
                return;
            }
            asking.insert(key.clone(), cancel.clone());
        }
        let requester = match &sig {
            SignalingMessage::IncomingSession { requester: Some(r), .. }
            | SignalingMessage::IncomingFileSession { requester: Some(r), .. }
                if !r.trim().is_empty() =>
            {
                format!("{} (through the Lunaris server)", r.trim())
            }
            _ => "a user of the Lunaris server".to_string(),
        };
        let status = |client: &str, s: &str| {
            AgentMessage::Signaling(SignalingMessage::AccessRequest { target_id: client.to_string(), status: s.to_string() })
        };
        let _ = self.server_tx.send(status(&client, "waiting"));
        let (server_tx, inbound_tx) = (self.server_tx.clone(), self.inbound_tx.clone());
        let (accepted, asking) = (self.accepted.clone(), self.asking.clone());
        let question = (self.ask_fn)(requester, kind, cancel.clone());
        tokio::spawn(async move {
            let decision = question.await;
            let mut asking = asking.lock().unwrap();
            // Gone meanwhile (its EndSession withdrew the question): nothing
            // to deliver, nobody to tell.
            let still_here = matches!(asking.get(&key), Some(c) if Arc::ptr_eq(c, &cancel));
            let Some(decision) = decision.filter(|_| still_here) else { return };
            asking.remove(&key);
            let _ = server_tx.send(status(&client, decision.status()));
            if decision == approval::Decision::Accepted {
                accepted.lock().unwrap().insert(key);
                let _ = inbound_tx.send(sig);
            } else if kind == approval::Kind::Control {
                // The server marked the computer busy for this request.
                let _ = server_tx.send(AgentMessage::Signaling(SignalingMessage::EndSession { target_id: client }));
            }
        });
    }
}

impl Drop for ServerGate {
    /// The server connection is gone: its questions are withdrawn.
    fn drop(&mut self) {
        for (_, cancel) in self.asking.lock().unwrap().drain() {
            cancel.notify_one();
        }
        self.accepted.lock().unwrap().clear();
    }
}

/// The connection to the server: plain TCP for ws://; for wss:// a TLS
/// handshake whose certificate is checked before anything is sent: valid
/// for the host by the system's roots, or else the one pinned in the config
/// (`server_cert_sha256`), pinned now on the first connection.
async fn server_stream(
    config: &mut AgentConfig,
    ws_url: &str,
) -> Result<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>, Box<dyn std::error::Error + Send + Sync>> {
    let uri: tokio_tungstenite::tungstenite::http::Uri = ws_url
        .parse()
        .map_err(|e| anyhow::anyhow!("bad server address {}: {}", ws_url, e))?;
    let tls = match uri.scheme_str() {
        Some("wss") => true,
        Some("ws") => false,
        _ => return Err(anyhow::anyhow!("bad server address {}", ws_url).into()),
    };
    let host = uri
        .host()
        .ok_or_else(|| anyhow::anyhow!("no host in the server address {}", ws_url))?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    let port = uri.port_u16().unwrap_or(if tls { 443 } else { 80 });
    let tcp = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        tokio::net::TcpStream::connect((host.as_str(), port)),
    )
    .await
    .map_err(|_| anyhow::anyhow!("connecting to {}:{} timed out", host, port))??;
    let _ = tcp.set_nodelay(true);
    if !tls {
        return Ok(tokio_tungstenite::MaybeTlsStream::Plain(tcp));
    }

    let verifier = Arc::new(server_tls::RecordingVerifier::new());
    let tls_config = server_tls::client_config(verifier.clone())?;
    let name = rustls::pki_types::ServerName::try_from(host.clone())
        .map_err(|e| anyhow::anyhow!("bad server host name {}: {}", host, e))?;
    let stream = tokio_rustls::TlsConnector::from(Arc::new(tls_config))
        .connect(name, tcp)
        .await
        .map_err(|e| anyhow::anyhow!("TLS handshake with {}:{} failed: {}", host, port, e))?;
    let seen = verifier
        .take()
        .ok_or_else(|| anyhow::anyhow!("{}:{} sent no certificate", host, port))?;
    let fingerprint = server_tls::fingerprint(&seen.der);
    let host_port = format!("{}:{}", host, port);
    match server_tls::decide(seen.valid, &fingerprint, &host_port, &config.server_cert_sha256, &config.server_cert_host) {
        server_tls::Decision::Valid | server_tls::Decision::PinMatches => {}
        server_tls::Decision::Pin => {
            warn!(
                "The Lunaris server {} has a certificate the system does not trust (self-signed?): \
                 trusting it from now on (pinned SHA-256 {}). Compare it with the server's if in doubt.",
                host_port, fingerprint
            );
            config.server_cert_sha256 = fingerprint.clone();
            config.server_cert_host = host_port.clone();
            let (sha, for_host) = (fingerprint.clone(), host_port.clone());
            if let Err(e) = crate::pairing::update_config_file(crate::pairing::config_path(), config, |c| {
                c.server_cert_sha256 = sha;
                c.server_cert_host = for_host;
            }) {
                warn!("Could not save the pinned server certificate: {}", e);
            }
        }
        server_tls::Decision::Mismatch => {
            return Err(anyhow::anyhow!(
                "the TLS certificate of the Lunaris server {} is not the one trusted before \
                 (pinned SHA-256 {}, now {}): not connecting, the connection may be intercepted. \
                 If the server's certificate was replaced on purpose, empty server_cert_sha256 in {} \
                 and restart the agent",
                host_port,
                config.server_cert_sha256,
                fingerprint,
                crate::pairing::config_path()
            )
            .into());
        }
    }
    Ok(tokio_tungstenite::MaybeTlsStream::Rustls(stream))
}

/// Trust on first use for the server's TLS certificate.
mod server_tls {
    use std::sync::{Arc, Mutex, OnceLock};

    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::client::WebPkiServerVerifier;
    use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{DigitallySignedStruct, RootCertStore, SignatureScheme};

    pub struct Seen {
        pub der: Vec<u8>,
        /// Valid for the host name by the system's roots.
        pub valid: bool,
    }

    /// Lets the handshake finish with any certificate (its signatures over
    /// the handshake are still verified, so the server holds its key) and
    /// records it; the caller decides before sending anything.
    pub struct RecordingVerifier {
        roots: Option<Arc<WebPkiServerVerifier>>,
        algorithms: WebPkiSupportedAlgorithms,
        seen: Mutex<Option<Seen>>,
    }

    impl std::fmt::Debug for RecordingVerifier {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("RecordingVerifier")
        }
    }

    fn provider() -> Arc<CryptoProvider> {
        Arc::new(rustls::crypto::ring::default_provider())
    }

    /// The system's root certificates, loaded once.
    fn system_roots() -> Option<Arc<RootCertStore>> {
        static ROOTS: OnceLock<Option<Arc<RootCertStore>>> = OnceLock::new();
        ROOTS
            .get_or_init(|| {
                let mut store = RootCertStore::empty();
                store.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
                (!store.is_empty()).then(|| Arc::new(store))
            })
            .clone()
    }

    impl RecordingVerifier {
        pub fn new() -> Self {
            let provider = provider();
            let roots = system_roots()
                .and_then(|r| WebPkiServerVerifier::builder_with_provider(r, provider.clone()).build().ok());
            Self { roots, algorithms: provider.signature_verification_algorithms, seen: Mutex::new(None) }
        }

        pub fn take(&self) -> Option<Seen> {
            self.seen.lock().unwrap().take()
        }
    }

    impl ServerCertVerifier for RecordingVerifier {
        fn verify_server_cert(
            &self,
            end_entity: &CertificateDer<'_>,
            intermediates: &[CertificateDer<'_>],
            server_name: &ServerName<'_>,
            ocsp_response: &[u8],
            now: UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            let valid = self
                .roots
                .as_ref()
                .map(|v| v.verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now).is_ok())
                .unwrap_or(false);
            *self.seen.lock().unwrap() = Some(Seen { der: end_entity.as_ref().to_vec(), valid });
            Ok(ServerCertVerified::assertion())
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

    pub fn client_config(verifier: Arc<RecordingVerifier>) -> Result<rustls::ClientConfig, rustls::Error> {
        Ok(rustls::ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()?
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth())
    }

    pub fn fingerprint(der: &[u8]) -> String {
        crate::pairing::to_hex(ring::digest::digest(&ring::digest::SHA256, der).as_ref())
    }

    #[derive(Debug, PartialEq, Eq)]
    pub enum Decision {
        /// Valid by the system's roots.
        Valid,
        /// The pinned certificate.
        PinMatches,
        /// Nothing pinned for this server: pin this one.
        Pin,
        /// Another certificate than the pinned one: refuse.
        Mismatch,
    }

    /// `pinned_host` empty: the pin holds for any server address.
    pub fn decide(valid: bool, fingerprint: &str, host: &str, pinned: &str, pinned_host: &str) -> Decision {
        let pinned = pinned.trim();
        if valid {
            Decision::Valid
        } else if pinned.is_empty() || (!pinned_host.is_empty() && pinned_host != host) {
            Decision::Pin
        } else if pinned.eq_ignore_ascii_case(fingerprint) {
            Decision::PinMatches
        } else {
            Decision::Mismatch
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn trust_on_first_use() {
            let fp = fingerprint(b"cert");
            assert_eq!(fp.len(), 64);
            assert_eq!(decide(true, &fp, "h:1", "", ""), Decision::Valid);
            assert_eq!(decide(true, &fp, "h:1", "00", "h:1"), Decision::Valid);
            assert_eq!(decide(false, &fp, "h:1", "", ""), Decision::Pin);
            assert_eq!(decide(false, &fp, "h:1", &fp, "h:1"), Decision::PinMatches);
            assert_eq!(decide(false, &fp, "h:1", &fp.to_uppercase(), ""), Decision::PinMatches);
            assert_eq!(decide(false, &fp, "h:1", &fingerprint(b"other"), "h:1"), Decision::Mismatch);
            assert_eq!(decide(false, &fp, "h:1", &fingerprint(b"other"), ""), Decision::Mismatch);
            // Pinned for another server: this one is new.
            assert_eq!(decide(false, &fp, "h:1", &fingerprint(b"other"), "g:1"), Decision::Pin);
        }
    }
}

/// `file_access` as it applies here. Not set: "home", but "full" when
/// running as LocalSystem (the Windows service's agent), whose home folder
/// is the system profile; the person used to reach every drive there.
fn file_access_setting(config: &AgentConfig) -> String {
    let value = config.file_access.trim();
    if !value.is_empty() {
        return value.to_string();
    }
    #[cfg(target_os = "windows")]
    if win_service::running_as_local_system() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            info!(
                "File transfer may reach the whole disk: this agent runs as LocalSystem (the Windows service). \
                 To restrict it, set \"file_access\" to a folder (or \"home\") in {}",
                crate::pairing::config_path()
            )
        });
        return "full".to_string();
    }
    "home".to_string()
}

/// Stream parameters from the client, bounded to what the capture and
/// encoders handle (lunaris-media divides by the frame rate, and a few
/// encoders take the bitrate in 16- or 32-bit fields).
fn clamp_stream_request(
    width: Option<u32>,
    height: Option<u32>,
    fps: Option<u32>,
    bitrate: Option<u32>,
) -> (Option<u32>, Option<u32>, Option<u32>, Option<u32>) {
    // Even sizes: 4:2:0 encoders need them.
    let size = |v: u32| v.clamp(64, 8192) & !1;
    (
        width.map(size),
        height.map(size),
        // lunaris-media keeps the frame rate within 1..=240 when it changes it.
        fps.map(|v| v.clamp(1, 240)),
        // kbps; the clients offer up to 150 Mbps.
        bitrate.map(|v| v.clamp(500, 500_000)),
    )
}

/// The agent's session logic: one streaming session at a time, driven by
/// signaling from server and direct clients alike (addressed with the client's
/// connection id). Replies go to `agent_tx`, which the hub routes.
async fn run_session_logic(
    config: AgentConfig,
    agent_tx: mpsc::UnboundedSender<AgentMessage>,
    mut inbound: mpsc::UnboundedReceiver<SignalingMessage>,
) {
    let active_session: Arc<Mutex<Option<Arc<BridgeSession>>>> = Arc::new(Mutex::new(None));
    // The client connection that owns `active_session`.
    let mut active_client: Option<String> = None;
    // File-transfer connections (files.rs), by client connection; any number,
    // beside the stream.
    let mut file_sessions: std::collections::HashMap<String, Arc<dyn PeerConnection>> =
        std::collections::HashMap::new();

    while let Some(sig) = inbound.recv().await {
        // Answers, candidates and ends of file sessions go to them.
        let sig = match sig {
            SignalingMessage::IncomingFileSession { client_id, ref source_id, .. }
                if bridge_agent::is_bridge_only() && source_id.as_deref().unwrap_or("").is_empty() =>
            {
                // The bridge-only agent has no files of its own to give
                // (the files of a source it shows are that computer's).
                warn!("File transfer asked of the bridge-only agent by {}: refused", client_id);
                continue;
            }
            SignalingMessage::IncomingFileSession { client_id, ice_servers, source_id, .. } => {
                info!("File transfer session requested by {}", client_id);
                if let Some(old) = file_sessions.remove(&client_id) {
                    let _ = old.close().await;
                }
                // The files of a source are served from its connector, never
                // from this computer; a source that is not there is an error.
                let started = match files::source_target(source_id.as_deref(), crate::connector::configured_sources) {
                    Some(target) => files::start_for_source(client_id.clone(), agent_tx.clone(), ice_servers, target).await,
                    None => {
                        let access = files::Access::from_config(&file_access_setting(&config));
                        if let files::Access::Denied(reason) = &access {
                            warn!("{}", reason);
                        }
                        files::start(client_id.clone(), agent_tx.clone(), ice_servers, config.allow_file_transfer, access).await
                    }
                };
                match started {
                    Ok((pc, sdp)) => {
                        file_sessions.insert(client_id.clone(), pc);
                        let _ = agent_tx.send(AgentMessage::Signaling(SignalingMessage::Sdp {
                            target_id: client_id,
                            sdp: RtcSessionDescription { ty: RtcSdpType::Offer, sdp },
                            ice_servers: None,
                            webtransport_port: None,
                            webtransport_cert_hash: None,
                            webtransport_token: None,
                        }));
                    }
                    Err(e) => {
                        error!("Could not start a file session: {:?}", e);
                        let _ = agent_tx.send(AgentMessage::Signaling(SignalingMessage::Error {
                            message: format!("Could not start file transfer: {}", e),
                        }));
                    }
                }
                continue;
            }
            SignalingMessage::Sdp { ref target_id, ref sdp, .. } if file_sessions.contains_key(target_id) => {
                let pc = file_sessions[target_id].clone();
                match RTCSessionDescription::answer(sdp.sdp.clone()) {
                    Ok(answer) => {
                        if let Err(e) = pc.set_remote_description(answer).await {
                            error!("File session: bad answer: {:?}", e);
                        }
                    }
                    Err(e) => error!("File session: bad answer: {:?}", e),
                }
                continue;
            }
            SignalingMessage::IceCandidate { ref target_id, ref candidate } if file_sessions.contains_key(target_id) => {
                let pc = file_sessions[target_id].clone();
                let _ = pc
                    .add_ice_candidate(RTCIceCandidateInit {
                        candidate: candidate.candidate.clone(),
                        sdp_mid: candidate.sdp_mid.clone(),
                        sdp_mline_index: candidate.sdp_mline_index,
                        username_fragment: candidate.username_fragment.clone(),
                        ..Default::default()
                    })
                    .await;
                continue;
            }
            SignalingMessage::EndSession { ref target_id } if file_sessions.contains_key(target_id) => {
                info!("File transfer session ended by {}", target_id);
                if let Some(pc) = file_sessions.remove(target_id) {
                    let _ = pc.close().await;
                }
                continue;
            }
            other => other,
        };
        match sig {
            SignalingMessage::IncomingSession {
                client_id,
                width,
                height,
                fps,
                bitrate,
                codec,
                app_id: _,
                encoder,
                display_id,
                chroma,
                encoder_csc_mode,
                microphone,
                ice_servers,
                source_id,
                decode_codecs,
                ..
            } => {
                let (width, height, fps, bitrate) = clamp_stream_request(width, height, fps, bitrate);
                // A source asked for must be one this computer has: its own
                // screen is never shown in place of one that is not there.
                let source = match source_id.as_deref().filter(|id| !id.is_empty()) {
                    // The bridge-only agent shows other computers, never this one.
                    None if bridge_agent::is_bridge_only() => {
                        warn!("Session without a source asked of the bridge-only agent: refused");
                        let _ = agent_tx.send(AgentMessage::Signaling(SignalingMessage::SourceStatus {
                            target_id: client_id.clone(),
                            status: "failed".into(),
                            reason: Some("That computer is not there any more".into()),
                        }));
                        continue;
                    }
                    None => None,
                    Some(id) => match crate::connector::configured_sources().into_iter().find(|s| s.id == id) {
                        Some(found) => {
                            info!("Session for source \"{}\" ({}) through connector {}", found.name, found.id, found.target.connector);
                            Some(found.target)
                        }
                        None => {
                            warn!("Session asked for a source this computer does not have");
                            let _ = agent_tx.send(AgentMessage::Signaling(SignalingMessage::SourceStatus {
                                target_id: client_id.clone(),
                                status: "failed".into(),
                                reason: Some("This computer no longer has that source".into()),
                            }));
                            continue;
                        }
                    },
                };
                info!(
                    "Incoming session request from client: {} codec={:?} encoder={:?} res={:?}x{:?} fps={:?} bitrate={:?} display={:?} chroma={:?} encoder_csc_mode={:?} microphone={:?}",
                    client_id, codec, encoder, width, height, fps, bitrate, display_id, chroma, encoder_csc_mode, microphone
                );

                // Clean up previous active session
                {
                    let mut lock = active_session.lock().await;
                    if let Some(old_session) = lock.take() {
                        info!("Cleaning up previous active session before setting up new one...");
                        let _ = old_session.peer_connection.close().await;
                        LAST_STREAM_STOP_TIME_MS.store(
                            get_current_time_ms(),
                            std::sync::atomic::Ordering::SeqCst,
                        );
                    }
                }

                // Wait if the stream was stopped very recently (let encoder release resources)
                let last_stop = LAST_STREAM_STOP_TIME_MS.load(std::sync::atomic::Ordering::SeqCst);
                if last_stop > 0 {
                    let now = get_current_time_ms();
                    let elapsed = now.saturating_sub(last_stop);
                    if elapsed < 1200 {
                        let sleep_ms = 1200 - elapsed;
                        info!("Previous stream was stopped recently ({}ms ago). Delaying new stream setup by {}ms to prevent encoder conflicts...", elapsed, sleep_ms);
                        tokio::time::sleep(std::time::Duration::from_millis(sleep_ms)).await;
                    }
                }

                active_client = None;

                // Initialize bridge session
                let session = match setup_bridge_session(
                    config.clone(),
                    client_id.clone(),
                    agent_tx.clone(),
                    width,
                    height,
                    fps,
                    bitrate,
                    codec,
                    encoder,
                    display_id,
                    chroma.as_deref() == Some("444"),
                    encoder_csc_mode,
                    microphone.unwrap_or(false),
                    ice_servers,
                    source,
                    decode_codecs,
                )
                .await
                {
                    Ok(s) => s,
                    Err(e) => {
                        // A source that could not be shown: the viewer is
                        // told why, in the connector's words.
                        if let Some(source_error) = e.downcast_ref::<crate::connector::SourceError>() {
                            warn!("The source could not be shown: {}", source_error);
                            let _ = agent_tx.send(AgentMessage::Signaling(SignalingMessage::SourceStatus {
                                target_id: client_id.clone(),
                                status: "failed".into(),
                                reason: Some(source_error.to_string()),
                            }));
                            continue;
                        }
                        error!("Failed to setup WebRTC bridge session: {:?}", e);
                        let _ = agent_tx.send(AgentMessage::Signaling(
                            SignalingMessage::Error {
                                message: format!(
                                    "Failed to bridge connection: {:?}",
                                    e
                                ),
                            },
                        ));
                        continue;
                    }
                };

                // Generate SDP Offer
                let offer = match session.peer_connection.create_offer(None).await {
                    Ok(o) => o,
                    Err(e) => {
                        error!("Failed to create SDP Offer: {:?}", e);
                        continue;
                    }
                };

                if let Err(e) = session
                    .peer_connection
                    .set_local_description(offer.clone())
                    .await
                {
                    error!("Failed to set local description: {:?}", e);
                    continue;
                }

                // Send Offer to Client
                let sdp_msg = AgentMessage::Signaling(SignalingMessage::Sdp {
                    target_id: client_id.clone(),
                    sdp: RtcSessionDescription {
                        ty: RtcSdpType::Offer,
                        sdp: offer.sdp,
                    },
                    ice_servers: None,
                    webtransport_port: session.webtransport_port,
                    webtransport_cert_hash: session.webtransport_cert_hash.clone(),
                    webtransport_token: session.webtransport_token.clone(),
                });

                // CRITICAL: Store session BEFORE sending the offer to avoid a race
                // condition where the client's SDP answer arrives before the session
                // is stored, causing it to be silently dropped (=> no ICE candidate
                // pairs => connection timeout after ~7s).
                {
                    let mut lock = active_session.lock().await;
                    *lock = Some(session);
                }
                active_client = Some(client_id.clone());

                let _ = agent_tx.send(sdp_msg);
                warn!("[SIGNALING] SDP Offer sent to client: {}", client_id);
            }
            SignalingMessage::Sdp { target_id, .. } if active_client.as_deref() != Some(target_id.as_str()) => {
                // Only the client the offer went to answers it.
                warn!("[SIGNALING] Ignoring an SDP answer from {} (not the session's client)", target_id);
            }
            SignalingMessage::Sdp { target_id, sdp, .. } => {
                warn!("[SIGNALING] Received SDP answer from client: {}", target_id);
                let lock = active_session.lock().await;
                if let Some(session) = lock.as_ref() {
                    // Force Level 4.2 in SDP Answer to prevent browser decoder from silently rejecting Level 4.0/4.2 AMF GPU streams.
                    let forced_sdp = sdp
                        .sdp
                        .replace("profile-level-id=42001f", "profile-level-id=42002a")
                        // Same for H.264 High 4:4:4 (YUV 4:4:4 sessions).
                        .replace("profile-level-id=f4001f", "profile-level-id=f4002a");
                    match RTCSessionDescription::answer(forced_sdp.clone()) {
                        Ok(rtc_sdp) => {
                            if let Err(e) = session
                                .peer_connection
                                .set_remote_description(rtc_sdp)
                                .await
                            {
                                error!("[SIGNALING] Failed to set remote description: {:?}", e);
                            } else {
                                warn!("[SIGNALING] SDP Answer set successfully! WebRTC negotiation in progress.");
                                match bridge::flexfec_in_answer(&forced_sdp) {
                                    Some(pt) => info!("FlexFEC-03 (20%): accepted by the client's answer (payload type {}), the video is protected by forward error correction", pt),
                                    None => info!("FlexFEC-03: not in use (offered only with LUNARIS_FEC=1, and the client has to accept it); lost packets are repaired by NACK"),
                                }
                            }
                        }
                        Err(e) => {
                            error!("[SIGNALING] Failed to parse SDP Answer: {:?}", e);
                        }
                    }
                } else {
                    warn!("[SIGNALING] *** BUG: Received SDP answer but active_session is None! Answer dropped.");
                }
            }
            SignalingMessage::IceCandidate { target_id, .. } if active_client.as_deref() != Some(target_id.as_str()) => {
                debug!("Ignoring an ICE candidate from {} (not the session's client)", target_id);
            }
            SignalingMessage::IceCandidate {
                target_id: _,
                candidate,
            } => {
                let lock = active_session.lock().await;
                if let Some(session) = lock.as_ref() {
                    let rtc_cand = RTCIceCandidateInit {
                        candidate: candidate.candidate,
                        sdp_mid: candidate.sdp_mid,
                        sdp_mline_index: candidate.sdp_mline_index,
                        username_fragment: candidate.username_fragment,
                        ..Default::default()
                    };
                    if let Err(e) =
                        session.peer_connection.add_ice_candidate(rtc_cand).await
                    {
                        debug!("Failed to add ICE candidate: {:?}", e);
                    }
                }
            }
            SignalingMessage::EndSession { target_id } => {
                // Only the client that owns the session ends it: another
                // client (e.g. a direct connection that only asked for the
                // display list) disconnecting must not cut the stream.
                if active_client.as_deref() != Some(target_id.as_str()) {
                    debug!("Ignoring EndSession from {} (not the session's client)", target_id);
                    continue;
                }
                info!("Session ended by client: {}", target_id);
                active_client = None;
                let mut lock = active_session.lock().await;
                if let Some(session) = lock.take() {
                    let _ = session.peer_connection.close().await;
                    LAST_STREAM_STOP_TIME_MS.store(get_current_time_ms(), std::sync::atomic::Ordering::SeqCst);
                }
            }
            SignalingMessage::GetAppList { target_id } => {
                info!("Received GetAppList request from target: {}", target_id);
                let agent_tx_clone = agent_tx.clone();
                let apps = vec![common::AppInfo {
                    id: 0,
                    title: "Desktop".to_string(),
                    icon_base64: None,
                }];
                let resp = AgentMessage::Signaling(
                    SignalingMessage::AppListResponse {
                        target_id,
                        apps,
                        current_game_id: 0,
                    },
                );
                let _ = agent_tx_clone.send(resp);
            }
            SignalingMessage::GetCapabilities { target_id } => {
                info!("Received GetCapabilities request from target: {}", target_id);
                let agent_tx_clone = agent_tx.clone();

                // 1. List displays. Uses the lightweight enumeration
                // (NvFBC status only, no CUDA context) so opening the
                // client settings no longer allocates/leaks GPU memory.
                let mut displays = Vec::new();
                // The bridge-only agent has nothing of this computer to tell:
                // no displays, no encoders; only its sources.
                // Nor while another computer is being shown: the viewer asks
                // about what it sees, and that computer's displays reach it
                // with the picture.
                let own_screen = !bridge_agent::is_bridge_only() && !crate::connector::media::source_active();
                match if own_screen { lunaris_media::capture::list_displays_for_capabilities().await } else { Ok(Vec::new()) } {
                    Ok(display_list) => {
                        info!("[GetCapabilities] Discovered {} display(s)", display_list.len());
                        for d in &display_list {
                            info!("  Display: {} ({}) {}x{} @{}Hz primary={}", d.id, d.name, d.width, d.height, d.refresh_rate, d.is_primary);
                            displays.push(common::DisplayInfoMsg {
                                id: d.id.clone(),
                                name: d.name.clone(),
                                width: d.width,
                                height: d.height,
                                refresh_rate: d.refresh_rate,
                                is_primary: d.is_primary,
                            });
                        }
                    }
                    Err(e) => error!("Failed to list displays: {:?}", e),
                }

                // 3. Query encoders (fast, non-blocking)
                info!("[GetCapabilities] Querying encoders...");
                let mut encoders = Vec::new();
                encoders.push("native".to_string());
                encoders.push("ffmpeg".to_string());
                let encoder_list = if own_screen { lunaris_media::encode::list_available_encoders() } else { Vec::new() };
                for enc in encoder_list {
                    if !encoders.contains(&enc.name) {
                        encoders.push(enc.name.clone());
                    }
                    let hw_name = format!("{}", enc.hw_type).to_lowercase();
                    if !encoders.contains(&hw_name) {
                        encoders.push(hw_name);
                    }
                }
                if !encoders.contains(&"software".to_string()) {
                    encoders.push("software".to_string());
                }

                // 4. Send response
                info!("[GetCapabilities] Sending response: {} displays, {} encoders", displays.len(), encoders.len());
                let resp = AgentMessage::Signaling(
                    SignalingMessage::CapabilitiesResponse {
                        target_id,
                        displays,
                        encoders,
                        gpu_info: if own_screen { lunaris_media::encode::describe_host_gpu(None) } else { None },
                        // While another computer is shown, the viewer treats the
                        // pointer by what that computer is.
                        host_os: Some(crate::connector::media::source_os().unwrap_or_else(|| std::env::consts::OS.to_string())),
                        // The other computers this one shows: id and name only.
                        // (Not their groups: a viewer picks one by its name.)
                        sources: crate::connector::configured_sources()
                            .into_iter()
                            .map(|s| common::SourceInfoMsg { id: s.id, name: s.name, connector: s.target.connector, group: None })
                            .collect(),
                    },
                );
                if let Err(e) = agent_tx_clone.send(resp) {
                    error!("Failed to send CapabilitiesResponse: {:?}", e);
                } else {
                    info!("[GetCapabilities] Response sent!");
                }
            }
            SignalingMessage::StopActiveStream { target_id } => {
                info!(
                    "Received StopActiveStream request from target: {}",
                    target_id
                );
                // Clean up active session
                active_client = None;
                let success = {
                    let mut lock = active_session.lock().await;
                    if let Some(session) = lock.take() {
                        let _ = session.peer_connection.close().await;
                        LAST_STREAM_STOP_TIME_MS.store(get_current_time_ms(), std::sync::atomic::Ordering::SeqCst);
                        true
                    } else {
                        false
                    }
                };
                let resp = AgentMessage::Signaling(
                    SignalingMessage::StopActiveStreamResponse {
                        target_id,
                        success,
                        error: if success { None } else { Some("No active session".to_string()) },
                    },
                );
                let _ = agent_tx.send(resp);
            }
            _ => {}
        }
    }

    let mut session_lock = active_session.lock().await;
    if let Some(session) = session_lock.take() {
        info!("Stopping active remote streaming session on agent exit...");
        let _ = session.peer_connection.close().await;
    }
}

#[cfg(test)]
mod stream_limit_tests {
    use super::clamp_stream_request;

    #[test]
    fn stream_parameters_are_bounded() {
        assert_eq!(clamp_stream_request(None, None, None, None), (None, None, None, None));
        assert_eq!(
            clamp_stream_request(Some(1920), Some(1080), Some(60), Some(20_000)),
            (Some(1920), Some(1080), Some(60), Some(20_000))
        );
        assert_eq!(
            clamp_stream_request(Some(0), Some(u32::MAX), Some(0), Some(u32::MAX)),
            (Some(64), Some(8192), Some(1), Some(500_000))
        );
        assert_eq!(clamp_stream_request(Some(1365), Some(767), Some(100_000), Some(0)), (Some(1364), Some(766), Some(240), Some(500)));
    }
}

#[cfg(test)]
mod server_gate_tests {
    use super::*;

    fn incoming(client: &str) -> SignalingMessage {
        SignalingMessage::IncomingFileSession { client_id: client.into(), ice_servers: None, requester: Some("bob".into()), source_id: None }
    }

    fn answer(client: &str) -> SignalingMessage {
        SignalingMessage::IceCandidate {
            target_id: client.into(),
            candidate: common::RtcIceCandidate {
                candidate: String::new(),
                sdp_mid: None,
                sdp_mline_index: None,
                username_fragment: None,
            },
        }
    }

    #[tokio::test]
    async fn trusted_passes_everything_ask_waits_for_the_person_here() {
        let (server_tx, mut server_rx) = mpsc::unbounded_channel();
        let (inbound_tx, mut inbound_rx) = mpsc::unbounded_channel();
        let gate = ServerGate::new(false, server_tx.clone(), inbound_tx.clone());
        gate.pass(incoming("c1"));
        assert!(matches!(inbound_rx.try_recv(), Ok(SignalingMessage::IncomingFileSession { .. })));

        let gate = ServerGate::new(true, server_tx, inbound_tx);
        gate.pass(SignalingMessage::GetCapabilities { target_id: "c1".into() });
        assert!(matches!(inbound_rx.try_recv(), Ok(SignalingMessage::GetCapabilities { .. })));
        // Nothing from a client that was not accepted reaches the session.
        gate.pass(answer("c1"));
        gate.pass(incoming("c1"));
        assert!(inbound_rx.try_recv().is_err());
        match server_rx.try_recv() {
            Ok(AgentMessage::Signaling(SignalingMessage::AccessRequest { target_id, status })) => {
                assert_eq!((target_id.as_str(), status.as_str()), ("c1", "waiting"));
            }
            other => panic!("{:?}", other),
        }
        // Once granted, that kind and the messages that follow pass.
        gate.accepted.lock().unwrap().insert(("c1".into(), approval::Kind::Files));
        gate.pass(answer("c1"));
        assert!(matches!(inbound_rx.try_recv(), Ok(SignalingMessage::IceCandidate { .. })));
        gate.pass(answer("c2"));
        assert!(inbound_rx.try_recv().is_err());
    }

    fn session(client: &str) -> SignalingMessage {
        serde_json::from_value(serde_json::json!({
            "type": "IncomingSession",
            "payload": {"client_id": client, "width": null, "height": null, "fps": null, "bitrate": null,
                        "codec": null, "app_id": null, "encoder": null, "display_id": null, "ice_servers": null}
        }))
        .unwrap()
    }

    /// A gate whose questions are answered "accept" when `answer` is notified.
    fn gate_with_answer() -> (
        ServerGate,
        Arc<tokio::sync::Notify>,
        mpsc::UnboundedReceiver<AgentMessage>,
        mpsc::UnboundedReceiver<SignalingMessage>,
    ) {
        let (server_tx, server_rx) = mpsc::unbounded_channel();
        let (inbound_tx, inbound_rx) = mpsc::unbounded_channel();
        let mut gate = ServerGate::new(true, server_tx, inbound_tx);
        let answer = Arc::new(tokio::sync::Notify::new());
        let a = answer.clone();
        gate.ask_fn = Arc::new(move |_, _, cancel| {
            let a = a.clone();
            Box::pin(async move {
                tokio::select! {
                    _ = a.notified() => Some(approval::Decision::Accepted),
                    _ = cancel.notified() => None,
                }
            })
        });
        (gate, answer, server_rx, inbound_rx)
    }

    async fn settle() {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn a_client_that_left_while_asked_gets_nothing() {
        let (gate, answer, mut server_rx, mut inbound_rx) = gate_with_answer();
        gate.pass(session("c1"));
        assert!(matches!(server_rx.try_recv(), Ok(AgentMessage::Signaling(SignalingMessage::AccessRequest { .. }))));
        gate.pass(SignalingMessage::EndSession { target_id: "c1".into() });
        assert!(matches!(inbound_rx.try_recv(), Ok(SignalingMessage::EndSession { .. })));
        assert!(gate.asking.lock().unwrap().is_empty());
        // Accepted after the client left: the request is not delivered.
        answer.notify_waiters();
        settle().await;
        assert!(inbound_rx.try_recv().is_err());
        assert!(server_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn grants_end_with_the_client_and_stop_needs_control() {
        let (gate, answer, _server_rx, mut inbound_rx) = gate_with_answer();
        gate.pass(session("c1"));
        settle().await;
        answer.notify_waiters();
        settle().await;
        assert!(matches!(inbound_rx.try_recv(), Ok(SignalingMessage::IncomingSession { .. })));
        assert!(gate.accepted.lock().unwrap().contains(&("c1".to_string(), approval::Kind::Control)));
        // Only a client with screen control stops the stream.
        gate.accepted.lock().unwrap().insert(("c2".into(), approval::Kind::Files));
        gate.pass(SignalingMessage::StopActiveStream { target_id: "c2".into() });
        assert!(inbound_rx.try_recv().is_err());
        gate.pass(SignalingMessage::StopActiveStream { target_id: "c1".into() });
        assert!(matches!(inbound_rx.try_recv(), Ok(SignalingMessage::StopActiveStream { .. })));
        // Its end prunes the grants: later messages from it are dropped.
        gate.pass(SignalingMessage::EndSession { target_id: "c1".into() });
        assert!(matches!(inbound_rx.try_recv(), Ok(SignalingMessage::EndSession { .. })));
        assert!(!gate.accepted.lock().unwrap().iter().any(|(c, _)| c == "c1"));
        gate.pass(answer_msg("c1"));
        assert!(inbound_rx.try_recv().is_err());
    }

    fn answer_msg(client: &str) -> SignalingMessage {
        answer(client)
    }
}
