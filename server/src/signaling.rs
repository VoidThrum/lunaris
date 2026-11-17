use axum::{
    extract::{
        connect_info::Connected,
        ws::{Message, WebSocket, WebSocketUpgrade},
        ConnectInfo, Query, State,
    },
    http::StatusCode,
    response::IntoResponse,
};
use common::{
    AgentMessage, ClientMessage, HostStatus, ServerToAgentMessage, ServerToClientMessage,
    SignalingMessage,
};
use futures_util::{SinkExt, StreamExt};
use sqlx::SqlitePool;
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::{Arc, RwLock},
};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::auth::verify_jwt;
use crate::queue::{Inbox, Outbox};
use crate::security::{self, AgentKeyDecision, FailureLimiter};

/// Largest WebSocket message accepted from a client: SDP answers and ICE
/// candidates are a few KiB, so this leaves ample headroom.
pub const CLIENT_MAX_MESSAGE: usize = 1024 * 1024;
/// From an agent: app lists may carry base64 icons (none today), so more room.
pub const AGENT_MAX_MESSAGE: usize = 8 * 1024 * 1024;
/// Open client WebSockets per user.
pub const MAX_CLIENT_SOCKETS_PER_USER: usize = 32;
use webrtc::peer_connection::RTCIceCandidateInit;
use webrtc::peer_connection::RTCSessionDescription;

/// Both ends of an HTTP/WebSocket connection: the local address is the server
/// address that peer uses, which is where it can reach the TURN relay.
#[derive(Clone, Copy, Debug)]
pub struct ConnAddrs {
    pub local: Option<SocketAddr>,
    pub remote: SocketAddr,
}

impl Connected<axum::serve::IncomingStream<'_, tokio::net::TcpListener>> for ConnAddrs {
    fn connect_info(target: axum::serve::IncomingStream<'_, tokio::net::TcpListener>) -> Self {
        ConnAddrs {
            local: target.io().local_addr().ok(),
            remote: *target.remote_addr(),
        }
    }
}

pub struct SignalingState {
    pub db: SqlitePool,
    /// Built-in TURN relay (None when disabled).
    pub turn: Option<Arc<crate::turn::TurnService>>,
    // agent_id / client conn_id -> server address that peer connected to
    pub agent_local_ips: RwLock<HashMap<String, IpAddr>>,
    pub client_local_ips: RwLock<HashMap<String, IpAddr>>,
    pub agent_token: String,
    // agent_id -> sender to agent WS
    pub agents: RwLock<HashMap<String, Outbox<ServerToAgentMessage>>>,
    // conn_id -> sender to client WS
    pub clients: RwLock<HashMap<String, Outbox<ServerToClientMessage>>>,
    // conn_id -> agent_id of the streaming session started on that connection
    // (agents address clients by conn_id)
    pub client_to_agent: RwLock<HashMap<String, String>>,
    // agent_id -> active connection Uuid
    pub agent_connections: RwLock<HashMap<String, Uuid>>,
    // conn_id (String) -> client_id — tracks which client owns each connection
    pub client_connections: RwLock<HashMap<String, String>>,
    // conn_id -> host_id of file-transfer connections (agent/src/files.rs)
    pub file_sessions: RwLock<HashMap<String, String>>,
    // client_id -> BridgeSession managed by server
    pub local_sessions: RwLock<HashMap<String, Arc<crate::bridge::BridgeSession>>>,
    /// Computers added by address (the server connects to them).
    pub links: crate::links::Links,
    /// Computers of other types that agents show (sources.rs).
    pub sources: crate::sources::Registry,
    /// Failed sign-ins per client address and per (username, address).
    pub login_ip_limit: FailureLimiter,
    pub login_user_limit: FailureLimiter,
    /// Wrong agent tokens per client address.
    pub agent_token_limit: FailureLimiter,
}

impl SignalingState {
    pub fn new(db: SqlitePool, agent_token: String, turn: Option<Arc<crate::turn::TurnService>>) -> Self {
        Self {
            db,
            turn,
            agent_local_ips: RwLock::new(HashMap::new()),
            client_local_ips: RwLock::new(HashMap::new()),
            agent_token,
            agents: RwLock::new(HashMap::new()),
            clients: RwLock::new(HashMap::new()),
            client_to_agent: RwLock::new(HashMap::new()),
            agent_connections: RwLock::new(HashMap::new()),
            client_connections: RwLock::new(HashMap::new()),
            file_sessions: RwLock::new(HashMap::new()),
            local_sessions: RwLock::new(HashMap::new()),
            links: crate::links::Links::default(),
            sources: crate::sources::Registry::default(),
            login_ip_limit: FailureLimiter::standard(),
            login_user_limit: FailureLimiter::standard(),
            // Higher: a stale agent retrying every few seconds must not lock
            // out every agent behind its address; the token is long and random.
            agent_token_limit: FailureLimiter::new(300, std::time::Duration::from_secs(300), 10_000),
        }
    }

    /// Closes every client WebSocket of a user (deactivated or deleted).
    pub fn close_user_connections(&self, user_id: &str) {
        let conn_ids: Vec<String> = self
            .client_connections
            .read()
            .unwrap()
            .iter()
            .filter(|(_, uid)| uid.as_str() == user_id)
            .map(|(conn, _)| conn.clone())
            .collect();
        let clients = self.clients.read().unwrap();
        for conn in &conn_ids {
            if let Some(tx) = clients.get(conn) {
                tx.close();
            }
        }
        if !conn_ids.is_empty() {
            info!("Closed {} connection(s) of user {}", conn_ids.len(), user_id);
        }
    }

    fn send_to_conn(&self, conn_id: &str, msg: ServerToClientMessage) {
        if let Some(tx) = self.clients.read().unwrap().get(conn_id) {
            let _ = tx.send(msg);
        }
    }

    /// A user's name, shown to the person at a computer asked to accept
    /// that user's connection.
    pub async fn username(&self, user_id: &str) -> Option<String> {
        sqlx::query_scalar("SELECT username FROM users WHERE id = ?")
            .bind(user_id)
            .fetch_optional(&self.db)
            .await
            .ok()
            .flatten()
    }

    pub async fn set_host_status(&self, host_id: &str, status: HostStatus) {
        let status_str = match status {
            HostStatus::Online => "Online",
            HostStatus::Offline => "Offline",
            HostStatus::Busy => "Busy",
        };
        let _ = sqlx::query("UPDATE hosts SET status = ? WHERE id = ?")
            .bind(status_str)
            .bind(host_id)
            .execute(&self.db)
            .await;
    }

    pub async fn register_host_db(
        &self,
        host_id: &str,
        host_name: &str,
        codec_support: Option<u32>,
    ) {
        let _ = sqlx::query(
            "INSERT INTO hosts (id, name, status, server_codec_mode_support) VALUES (?, ?, 'Online', ?)
             ON CONFLICT(id) DO UPDATE SET name = excluded.name, status = 'Online', server_codec_mode_support = excluded.server_codec_mode_support;"
        )
        .bind(host_id)
        .bind(host_name)
        .bind(codec_support.map(|c| c as i64))
        .execute(&self.db)
        .await;
    }

    /// ICE servers for an agent: the configured ones plus the built-in relay
    /// at the address the agent reaches the server on.
    pub async fn ice_servers_for_agent(&self, agent_id: &str) -> Vec<common::RtcIceServer> {
        let mut servers = self.fetch_ice_servers().await;
        if let Some(turn) = &self.turn {
            let agent_ip = self.agent_local_ips.read().unwrap().get(agent_id).copied();
            servers.extend(turn.ice_server_for(agent_ip, None));
        }
        servers
    }

    /// ICE servers for a client connection; falls back to the agent's
    /// address for the relay when the client came in on loopback.
    pub async fn ice_servers_for_client(&self, conn_id: &str, agent_id: &str) -> Vec<common::RtcIceServer> {
        let mut servers = self.fetch_ice_servers().await;
        if let Some(turn) = &self.turn {
            let client_ip = self.client_local_ips.read().unwrap().get(conn_id).copied();
            let agent_ip = self.agent_local_ips.read().unwrap().get(agent_id).copied();
            servers.extend(turn.ice_server_for(client_ip, agent_ip));
        }
        servers
    }

    pub async fn fetch_ice_servers(&self) -> Vec<common::RtcIceServer> {
        let rows: Result<Vec<(String, Option<String>, Option<String>)>, _> =
            sqlx::query_as("SELECT urls, username, credential FROM turn_servers")
                .fetch_all(&self.db)
                .await;

        match rows {
            Ok(rows) if !rows.is_empty() => rows
                .into_iter()
                .map(|(urls, username, credential)| {
                    let urls_vec = urls
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                    common::RtcIceServer {
                        urls: urls_vec,
                        username,
                        credential,
                    }
                })
                .collect(),
            _ => {
                vec![
                    common::RtcIceServer {
                        urls: vec!["stun:stun.l.google.com:19302".to_string()],
                        username: None,
                        credential: None,
                    },
                    common::RtcIceServer {
                        urls: vec!["stun:stun1.l.google.com:19302".to_string()],
                        username: None,
                        credential: None,
                    },
                ]
            }
        }
    }
}

// WS Agent query params
#[derive(serde::Deserialize)]
pub struct AgentParams {
    pub id: String,
    pub name: String,
    pub codec_support: Option<u32>,
    pub token: Option<String>,
    /// The agent's own key (hex): once a host id has one, only that agent may
    /// register with the id.
    pub key: Option<String>,
}

// WS Client query params
#[derive(serde::Deserialize)]
pub struct ClientParams {
    pub token: String,
}

pub async fn agent_ws_handler(
    ws: WebSocketUpgrade,
    Query(params): Query<AgentParams>,
    ConnectInfo(addrs): ConnectInfo<ConnAddrs>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<SignalingState>>,
) -> impl IntoResponse {
    let now = std::time::Instant::now();
    let client_ip = security::client_ip(addrs.remote.ip(), &headers);
    let ip_key = client_ip.to_string();
    if state.agent_token_limit.is_blocked(&ip_key, now) {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    let incoming_token = params.token.as_deref().unwrap_or("").trim();
    if !security::ct_eq(incoming_token, state.agent_token.trim()) {
        state.agent_token_limit.record_failure(&ip_key, now);
        warn!(
            "Unauthorized agent connection attempt from {} (invalid or missing token)",
            client_ip
        );
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if let Err(why) = security::validate_agent_identity(&params.id, &params.name) {
        warn!("Agent connection from {} refused: {}", client_ip, why);
        return StatusCode::BAD_REQUEST.into_response();
    }
    let key = params.key.as_deref().map(str::trim).filter(|k| !k.is_empty());
    if key.is_some_and(|k| !security::valid_agent_key(k)) {
        warn!("Agent {} sent a malformed key", params.id);
        return StatusCode::BAD_REQUEST.into_response();
    }
    match check_agent_key(&state, &params.id, &params.name, key).await {
        Ok(true) => {}
        Ok(false) => {
            warn!(
                "Agent connection from {} refused: wrong key for host id {} (or the id is in use)",
                client_ip,
                params.id
            );
            return StatusCode::UNAUTHORIZED.into_response();
        }
        Err(e) => {
            error!("Database error checking agent key: {}", e);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }

    if let Some(local) = addrs.local {
        state.agent_local_ips.write().unwrap().insert(params.id.clone(), local.ip());
    }
    ws.max_message_size(AGENT_MAX_MESSAGE)
        .on_upgrade(move |socket| handle_agent_socket(socket, params, state))
}

/// Whether an agent may register as `host_id`: a host that has a key accepts
/// only that key; the first key given for a host is stored.
async fn check_agent_key(
    state: &SignalingState,
    host_id: &str,
    name: &str,
    key: Option<&str>,
) -> Result<bool, sqlx::Error> {
    let stored: Option<String> = sqlx::query_scalar("SELECT agent_key_hash FROM hosts WHERE id = ?")
        .bind(host_id)
        .fetch_optional(&state.db)
        .await?
        .flatten();
    match security::agent_key_decision(stored.as_deref(), key) {
        AgentKeyDecision::Accept => Ok(true),
        AgentKeyDecision::Reject => Ok(false),
        AgentKeyDecision::Store => {
            // An agent without a key is online with this id: a newcomer with
            // the shared token must not claim the id and lock it out.
            if state.agents.read().unwrap().contains_key(host_id) {
                return Ok(false);
            }
            let hash = security::sha256_hex(key.unwrap_or_default());
            // Only fills an empty slot, so two agents racing for a new id
            // can't both win: re-read what was kept.
            sqlx::query(
                "INSERT INTO hosts (id, name, status, agent_key_hash) VALUES (?, ?, 'Offline', ?)
                 ON CONFLICT(id) DO UPDATE SET agent_key_hash = excluded.agent_key_hash
                 WHERE hosts.agent_key_hash IS NULL",
            )
            .bind(host_id)
            .bind(name)
            .bind(&hash)
            .execute(&state.db)
            .await?;
            let kept: Option<String> = sqlx::query_scalar("SELECT agent_key_hash FROM hosts WHERE id = ?")
                .bind(host_id)
                .fetch_optional(&state.db)
                .await?
                .flatten();
            if kept.as_deref().is_some_and(|k| security::ct_eq(k, &hash)) {
                info!("Stored the key of agent {}", host_id);
                Ok(true)
            } else {
                Ok(false)
            }
        }
    }
}

pub async fn client_ws_handler(
    ws: WebSocketUpgrade,
    Query(params): Query<ClientParams>,
    ConnectInfo(addrs): ConnectInfo<ConnAddrs>,
    State(state): State<Arc<SignalingState>>,
) -> impl IntoResponse {
    let Ok(claims) = verify_jwt(&params.token) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    // A valid token is not enough: the account must still exist and be active.
    match security::user_status(&state.db, &claims.sub).await {
        Ok(Some((_, true))) => {}
        Ok(_) => return StatusCode::UNAUTHORIZED.into_response(),
        Err(e) => {
            error!("Database error checking user: {}", e);
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }
    let open = state
        .client_connections
        .read()
        .unwrap()
        .values()
        .filter(|uid| **uid == claims.sub)
        .count();
    if open >= MAX_CLIENT_SOCKETS_PER_USER {
        warn!("User {} has too many open connections", claims.sub);
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    let local_ip = addrs.local.map(|a| a.ip());
    ws.max_message_size(CLIENT_MAX_MESSAGE)
        .on_upgrade(move |socket| handle_client_socket(socket, claims.sub, local_ip, state))
}

// --- Agent Connection Handler ---

/// An agent connected (to /ws/agent, or the server to it: links.rs): it
/// becomes the host's connection and shows online. Messages for it arrive on
/// the returned queue; the id tells `agent_disconnected` which connection ended.
pub(crate) async fn agent_connected(
    state: &Arc<SignalingState>,
    agent_id: &str,
    agent_name: &str,
    codec_support: Option<u32>,
) -> (Uuid, Inbox<ServerToAgentMessage>) {
    let (tx, rx) = crate::queue::channel::<ServerToAgentMessage>();
    let conn_id = Uuid::new_v4();
    state.agents.write().unwrap().insert(agent_id.to_string(), tx);
    state.agent_connections.write().unwrap().insert(agent_id.to_string(), conn_id);
    state.register_host_db(agent_id, agent_name, codec_support).await;
    info!("Agent registered: {} ({})", agent_name, agent_id);
    (conn_id, rx)
}

/// One message from a connected agent.
pub(crate) async fn agent_message(state: &Arc<SignalingState>, agent_id: &str, msg: AgentMessage) {
    match msg {
        AgentMessage::Register { .. } => {
            // Handled upon connection
        }
        AgentMessage::StatusUpdate { status } => {
            state.set_host_status(agent_id, status).await;
        }
        AgentMessage::Signaling(sig) => {
            handle_agent_signaling(sig, agent_id, state.clone()).await;
        }
        AgentMessage::Sources { sources, connectors, manageable } => {
            crate::sources::announced(state, agent_id, sources, connectors, manageable).await;
        }
        AgentMessage::SourceChanged { request_id, id, error } => {
            crate::sources::changed(state, agent_id, &request_id, id, error);
        }
    }
}

/// The agent connection `conn_id` ended: unless a newer one replaced it, the
/// host goes offline and its clients are told.
pub(crate) async fn agent_disconnected(state: &Arc<SignalingState>, agent_id: &str, conn_id: Uuid) {
    info!("Agent disconnected: {}", agent_id);
    let mut should_cleanup = false;
    {
        let mut conn_map = state.agent_connections.write().unwrap();
        if conn_map.get(agent_id) == Some(&conn_id) {
            conn_map.remove(agent_id);
            state.agents.write().unwrap().remove(agent_id);
            state.agent_local_ips.write().unwrap().remove(agent_id);
            crate::sources::agent_gone(state, agent_id);
            should_cleanup = true;
        }
    }
    if !should_cleanup {
        return;
    }
    state.set_host_status(agent_id, HostStatus::Offline).await;

    // Tell the connections that had a session with this agent.
    let conns: Vec<String> = {
        let mut map = state.client_to_agent.write().unwrap();
        let conns: Vec<String> = map
            .iter()
            .filter(|(_, a_id)| a_id.as_str() == agent_id)
            .map(|(conn, _)| conn.clone())
            .collect();
        for conn in &conns {
            map.remove(conn);
        }
        conns
    };
    for conn_id in conns {
        state.send_to_conn(
            &conn_id,
            ServerToClientMessage::Signaling(SignalingMessage::Error {
                message: "Host disconnected".to_string(),
            }),
        );
    }
}

async fn handle_agent_socket(socket: WebSocket, params: AgentParams, state: Arc<SignalingState>) {
    let (mut ws_sender, mut ws_receiver) = socket.split();
    let agent_id = params.id.clone();
    let (conn_id, mut rx) = agent_connected(&state, &agent_id, &params.name, params.codec_support).await;

    // Spawn a task to handle outbound messages to the agent (with heartbeat to prevent idle timeouts)
    let agent_id_clone = agent_id.clone();
    let mut send_task = tokio::spawn(async move {
        let mut ping_interval = tokio::time::interval(std::time::Duration::from_secs(15));
        ping_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                maybe_msg = rx.recv() => {
                    match maybe_msg {
                        Some(msg) => {
                            let json = match serde_json::to_string(&msg) {
                                Ok(j) => j,
                                Err(e) => {
                                    error!("Failed to serialize message to agent: {}", e);
                                    continue;
                                }
                            };
                            if let Err(e) = ws_sender.send(Message::Text(json.into())).await {
                                debug!("Error sending to agent {}: {}", agent_id_clone, e);
                                break;
                            }
                        }
                        None => break,
                    }
                }
                _ = ping_interval.tick() => {
                    if let Err(e) = ws_sender.send(Message::Ping(Vec::new().into())).await {
                        debug!("Error sending ping to agent {}: {}", agent_id_clone, e);
                        break;
                    }
                }
            }
        }
    });

    // Handle inbound messages from agent, until it or the sender (queue full) stops
    loop {
        let next = tokio::select! {
            next = ws_receiver.next() => next,
            _ = &mut send_task => break,
        };
        let Some(result) = next else { break };
        let msg = match result {
            Ok(m) => m,
            Err(e) => {
                warn!("WebSocket read error from agent {}: {}", agent_id, e);
                break;
            }
        };
        if let Message::Text(text) = msg {
            match serde_json::from_str::<AgentMessage>(&text) {
                Ok(agent_msg) => agent_message(&state, &agent_id, agent_msg).await,
                Err(e) => error!("Failed to parse agent message: {}", e),
            }
        }
    }

    agent_disconnected(&state, &agent_id, conn_id).await;
    send_task.abort();
}

// --- Client Connection Handler ---
async fn handle_client_socket(
    socket: WebSocket,
    client_id: String,
    local_ip: Option<IpAddr>,
    state: Arc<SignalingState>,
) {
    let (mut ws_sender, mut ws_receiver) = socket.split();
    let (tx, mut rx) = crate::queue::channel::<ServerToClientMessage>();
    let conn_id = Uuid::new_v4();

    // Register active client sender (keyed by conn_id to support multiple connections per client)
    let conn_id_str = conn_id.to_string();
    {
        state.clients.write().unwrap().insert(conn_id_str.clone(), tx);
        state
            .client_connections
            .write()
            .unwrap()
            .insert(conn_id_str.clone(), client_id.clone());
    }
    if let Some(ip) = local_ip {
        state.client_local_ips.write().unwrap().insert(conn_id_str.clone(), ip);
    }
    info!("Client WebSocket connected: {} (conn: {})", client_id, conn_id_str);

    // Spawn a task to handle outbound messages to the client (with heartbeat to prevent idle timeouts)
    let client_id_clone = client_id.clone();
    let mut send_task = tokio::spawn(async move {
        let mut ping_interval = tokio::time::interval(std::time::Duration::from_secs(15));
        ping_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                maybe_msg = rx.recv() => {
                    match maybe_msg {
                        Some(msg) => {
                            let json = match serde_json::to_string(&msg) {
                                Ok(j) => j,
                                Err(e) => {
                                    error!("Failed to serialize message to client: {}", e);
                                    continue;
                                }
                            };
                            if let Err(e) = ws_sender.send(Message::Text(json.into())).await {
                                debug!("Error sending to client {}: {}", client_id_clone, e);
                                break;
                            }
                        }
                        None => break,
                    }
                }
                _ = ping_interval.tick() => {
                    if let Err(e) = ws_sender.send(Message::Ping(Vec::new().into())).await {
                        debug!("Error sending ping to client {}: {}", client_id_clone, e);
                        break;
                    }
                }
            }
        }
    });

    // Handle inbound messages from client, until it or the sender (queue full,
    // user deactivated) stops
    loop {
        let next = tokio::select! {
            next = ws_receiver.next() => next,
            _ = &mut send_task => break,
        };
        let Some(result) = next else { break };
        let msg = match result {
            Ok(m) => m,
            Err(e) => {
                warn!("WebSocket read error from client {} (conn: {}): {}", client_id, conn_id, e);
                break;
            }
        };

        match &msg {
            Message::Text(_) => {},
            Message::Close(frame) => {
                info!("Client {} (conn: {}) sent close frame: {:?}", client_id, conn_id, frame);
                break;
            }
            _ => {
                debug!("Client {} (conn: {}) sent non-text message: {:?}", client_id, conn_id, msg);
            }
        }

        if let Message::Text(text) = msg {
            let client_msg: ClientMessage = match serde_json::from_str(&text) {
                Ok(m) => m,
                Err(e) => {
                    error!("Failed to parse client message from {} (conn: {}): {}", client_id, conn_id, e);
                    continue;
                }
            };

            match client_msg {
                ClientMessage::Signaling(sig) => {
                    handle_client_signaling(sig, &client_id, &conn_id.to_string(), state.clone()).await;
                }
            }
        }
    }

    // Connection closed
    info!("Client WebSocket disconnected: {} (conn: {})", client_id, conn_id);
    // Remove this specific connection
    state.clients.write().unwrap().remove(&conn_id_str);
    state.client_connections.write().unwrap().remove(&conn_id_str);
    state.client_local_ips.write().unwrap().remove(&conn_id_str);

    // A file-transfer connection: its agent closes that connection only.
    let file_host = state.file_sessions.write().unwrap().remove(&conn_id_str);
    if let Some(host_id) = file_host {
        if let Some(agent_tx) = state.agents.read().unwrap().get(&host_id).cloned() {
            let _ = agent_tx.send(ServerToAgentMessage::Signaling(SignalingMessage::EndSession {
                target_id: conn_id_str.clone(),
            }));
        }
        return;
    }

    // The session started on this connection ends with it (agents know the
    // client by its conn_id).
    {
        let host_id_opt = state.client_to_agent.write().unwrap().remove(&conn_id_str);
        if let Some(host_id) = host_id_opt {
            info!(
                "Cleaning up active session for host {} due to client disconnect",
                host_id
            );
            // Send EndSession to the Agent if agent is registered
            let agent_tx_opt = {
                let agents = state.agents.read().unwrap();
                agents.get(&host_id).cloned()
            };
            if let Some(agent_tx) = agent_tx_opt {
                let _ = agent_tx.send(ServerToAgentMessage::Signaling(
                    SignalingMessage::EndSession {
                        target_id: conn_id_str.clone(),
                    },
                ));
            }

            let session = {
                let mut sessions = state.local_sessions.write().unwrap();
                sessions.remove(&host_id)
            };
            if let Some(session) = session {
                let _ = session.peer_connection.close().await;
            }
            state.set_host_status(&host_id, HostStatus::Online).await;
        }
    }

    send_task.abort();
}

// --- Route Signaling from Agent -> Client ---
async fn handle_agent_signaling(sig: SignalingMessage, agent_id: &str, state: Arc<SignalingState>) {
    match sig {
        SignalingMessage::Sdp {
            target_id,
            sdp,
            ice_servers: _,
            webtransport_port,
            webtransport_cert_hash,
            webtransport_token,
        } => {
            let client_tx_opt = state.clients.read().unwrap().get(&target_id).cloned();
            if let Some(client_tx) = client_tx_opt {
                let ice_servers = state.ice_servers_for_client(&target_id, agent_id).await;
                let _ = client_tx.send(ServerToClientMessage::Signaling(SignalingMessage::Sdp {
                    target_id: agent_id.to_string(),
                    sdp,
                    ice_servers: Some(ice_servers),
                    webtransport_port,
                    webtransport_cert_hash,
                    webtransport_token,
                }));
            }
        }
        SignalingMessage::IceCandidate {
            target_id,
            candidate,
        } => {
            if let Some(client_tx) = state.clients.read().unwrap().get(&target_id) {
                let _ = client_tx.send(ServerToClientMessage::Signaling(
                    SignalingMessage::IceCandidate {
                        target_id: agent_id.to_string(),
                        candidate,
                    },
                ));
            }
        }
        SignalingMessage::EndSession { target_id } => {
            // Only for a connection that has a session with this agent.
            let stream = {
                let mut map = state.client_to_agent.write().unwrap();
                let ours = map.get(&target_id).is_some_and(|a| a == agent_id);
                if ours {
                    map.remove(&target_id);
                }
                ours
            };
            let file = {
                let mut map = state.file_sessions.write().unwrap();
                let ours = map.get(&target_id).is_some_and(|a| a == agent_id);
                if ours {
                    map.remove(&target_id);
                }
                ours
            };
            if !stream && !file {
                debug!("Agent {} ended a session it does not have ({}); ignored", agent_id, target_id);
                return;
            }
            state.send_to_conn(
                &target_id,
                ServerToClientMessage::Signaling(SignalingMessage::EndSession {
                    target_id: agent_id.to_string(),
                }),
            );
            if stream {
                state.set_host_status(agent_id, HostStatus::Online).await;
            }
        }

        SignalingMessage::AppListResponse {
            target_id,
            apps,
            current_game_id,
        } => {
            if let Some(client_tx) = state.clients.read().unwrap().get(&target_id) {
                let _ = client_tx.send(ServerToClientMessage::Signaling(
                    SignalingMessage::AppListResponse {
                        target_id: agent_id.to_string(),
                        apps,
                        current_game_id,
                    },
                ));
            }
        }
        SignalingMessage::StopActiveStreamResponse {
            target_id,
            success,
            error,
        } => {
            if let Some(client_tx) = state.clients.read().unwrap().get(&target_id) {
                let _ = client_tx.send(ServerToClientMessage::Signaling(
                    SignalingMessage::StopActiveStreamResponse {
                        target_id: agent_id.to_string(),
                        success,
                        error,
                    },
                ));
            }
        }
        SignalingMessage::CapabilitiesResponse { target_id, displays, encoders, gpu_info, host_os, sources } => {
            info!("Server: CapabilitiesResponse from agent {} for client {} ({} displays, {} encoders)", agent_id, target_id, displays.len(), encoders.len());
            let clients = state.clients.read().unwrap();
            if let Some(client_tx) = clients.get(&target_id) {
                info!("Server: Found client {} in clients map, forwarding CapabilitiesResponse", target_id);
                let msg = ServerToClientMessage::Signaling(SignalingMessage::CapabilitiesResponse {
                    target_id: agent_id.to_string(),
                    displays,
                    encoders,
                    gpu_info,
                    host_os,
                    sources,
                });
                let _ = client_tx.send(msg);
            } else {
                warn!("Server: Client {} NOT found in clients map! Cannot forward CapabilitiesResponse", target_id);
            }
        }
        SignalingMessage::SourceStatus { target_id, status, reason } => {
            if let Some(client_tx) = state.clients.read().unwrap().get(&target_id) {
                let _ = client_tx.send(ServerToClientMessage::Signaling(SignalingMessage::SourceStatus {
                    target_id: agent_id.to_string(),
                    status,
                    reason,
                }));
            }
        }
        SignalingMessage::AccessRequest { target_id, status } => {
            if let Some(client_tx) = state.clients.read().unwrap().get(&target_id) {
                let _ = client_tx.send(ServerToClientMessage::Signaling(SignalingMessage::AccessRequest {
                    target_id: agent_id.to_string(),
                    status,
                }));
            }
        }
        SignalingMessage::EncoderStatus { target_id, encoder, hw_type, gpu_info, requested_encoder, host_os, display_id, display_name } => {
            let clients = state.clients.read().unwrap();
            if let Some(client_tx) = clients.get(&target_id) {
                let msg = ServerToClientMessage::Signaling(SignalingMessage::EncoderStatus {
                    target_id: agent_id.to_string(),
                    encoder,
                    hw_type,
                    gpu_info,
                    requested_encoder,
                    host_os,
                    display_id,
                    display_name,
                });
                let _ = client_tx.send(msg);
            }
        }
        _ => {}
    }
}

// --- Route Signaling from Client -> Agent ---
async fn handle_client_signaling(
    sig: SignalingMessage,
    client_id: &str,
    conn_id: &str,
    state: Arc<SignalingState>,
) {
    if let Some(host_id) = addressed_host(&sig) {
        if !client_may_address(&state, client_id, conn_id, &sig, host_id).await {
            warn!("User {} refused access to host {}", client_id, host_id);
            state.send_to_conn(
                conn_id,
                ServerToClientMessage::Signaling(SignalingMessage::Error {
                    message: "You don't have access to this computer.".to_string(),
                }),
            );
            return;
        }
    }
    match sig {
        // A file-transfer connection (on its own client WebSocket): routed
        // like a session, without touching the host's streaming state.
        SignalingMessage::RequestFileSession { host_id } => {
            let agent_tx = state.agents.read().unwrap().get(&host_id).cloned();
            match agent_tx {
                Some(agent_tx) => {
                    info!("Client {} requested file transfer with host {}", client_id, host_id);
                    let ice_servers = state.ice_servers_for_agent(&host_id).await;
                    state.file_sessions.write().unwrap().insert(conn_id.to_string(), host_id.clone());
                    let requester = state.username(client_id).await;
                    let _ = agent_tx.send(ServerToAgentMessage::Signaling(SignalingMessage::IncomingFileSession {
                        client_id: conn_id.to_string(),
                        ice_servers: Some(ice_servers),
                        requester,
                    }));
                }
                None => {
                    if let Some(client_tx) = state.clients.read().unwrap().get(conn_id) {
                        let _ = client_tx.send(ServerToClientMessage::Signaling(SignalingMessage::Error {
                            message: "No agent connected for this host. Please ensure the agent is running.".to_string(),
                        }));
                    }
                }
            }
        }
        SignalingMessage::EndSession { .. } if state.file_sessions.read().unwrap().contains_key(conn_id) => {
            let host_id = state.file_sessions.write().unwrap().remove(conn_id).unwrap_or_default();
            if let Some(agent_tx) = state.agents.read().unwrap().get(&host_id).cloned() {
                let _ = agent_tx.send(ServerToAgentMessage::Signaling(SignalingMessage::EndSession {
                    target_id: conn_id.to_string(),
                }));
            }
        }
        SignalingMessage::RequestSession {
            host_id,
            width,
            height,
            fps,
            bitrate,
            codec,
            app_id,
            encoder,
            display_id,
            chroma,
            encoder_csc_mode,
            microphone,
            source_id,
            decode_codecs,
        } => {
            info!("Client {} requested session for host {} with settings: w={:?}, h={:?}, fps={:?}, bitrate={:?}, codec={:?}, app_id={:?}, chroma={:?}, encoder_csc_mode={:?}", client_id, host_id, width, height, fps, bitrate, codec, app_id, chroma, encoder_csc_mode);

            let previous_host = state.client_to_agent.write().unwrap().remove(conn_id);
            let mut stopped_previous_agent_session = false;
            if let Some(previous_host_id) = previous_host {
                if let Some(agent_tx) = state.agents.read().unwrap().get(&previous_host_id).cloned() {
                    let _ = agent_tx.send(ServerToAgentMessage::Signaling(
                        SignalingMessage::EndSession {
                            target_id: conn_id.to_string(),
                        },
                    ));
                    stopped_previous_agent_session = true;
                    info!(
                        "Stopped previous session for client {} on host {} before starting a new session",
                        client_id, previous_host_id
                    );
                }
                state.set_host_status(&previous_host_id, HostStatus::Online).await;
            }
            if stopped_previous_agent_session {
                tokio::time::sleep(std::time::Duration::from_millis(1300)).await;
            }

            // Check if there is an active session for host_id, if so terminate it first
            let old_session = {
                let mut sessions = state.local_sessions.write().unwrap();
                sessions.remove(&host_id)
            };
            if let Some(session) = old_session {
                let _ = session.peer_connection.close().await;
            }

            // Check if there is an online Agent registered for this host_id
            let agent_tx_opt = {
                let agents = state.agents.read().unwrap();
                agents.get(&host_id).cloned()
            };

            if let Some(agent_tx) = agent_tx_opt {
                let ice_servers = state.ice_servers_for_agent(&host_id).await;
                let requester = state.username(client_id).await;
                let incoming_msg =
                    ServerToAgentMessage::Signaling(SignalingMessage::IncomingSession {
                        client_id: conn_id.to_string(),
                        width,
                        height,
                        fps,
                        bitrate,
                        codec: codec.clone(),
                        app_id,
                        encoder: encoder.clone(),
                        display_id: display_id.clone(),
                        chroma: chroma.clone(),
                        encoder_csc_mode,
                        microphone,
                        source_id: source_id.clone(),
                        decode_codecs: decode_codecs.clone(),
                        ice_servers: Some(ice_servers),
                        requester,
                    });
                if let Err(e) = agent_tx.send(incoming_msg) {
                    error!(
                        "Failed to send IncomingSession to agent {}: {:?}",
                        host_id, e
                    );
                    // Fall through to fallback Agent-less code below
                } else {
                    // Store client-to-host routing and update status to Busy.
                    // The agent runs one session: it replaces any other
                    // connection's, which then must not end it or the status.
                    {
                        let mut map = state.client_to_agent.write().unwrap();
                        map.retain(|_, h| *h != host_id);
                        map.insert(conn_id.to_string(), host_id.clone());
                    }
                    state.set_host_status(&host_id, HostStatus::Busy).await;
                    info!(
                        "IncomingSession successfully forwarded to agent {} for client {}",
                        host_id, client_id
                    );
                    return;
                }
            }

            // No agent available for this host
            error!("No agent connected for host {}", host_id);
            if let Some(client_tx) = state.clients.read().unwrap().get(conn_id) {
                let _ = client_tx.send(ServerToClientMessage::Signaling(SignalingMessage::Error {
                    message: "No agent connected for this host. Please ensure the agent is running.".to_string(),
                }));
            }
        }
        SignalingMessage::Sdp {
            target_id,
            sdp,
            ice_servers: _,
            webtransport_port,
            webtransport_cert_hash,
            webtransport_token,
        } => {
            // target_id is the host_id
            info!(
                "Received SDP Answer from client {} for host {}",
                client_id, target_id
            );

            // Check if there is an online Agent registered for this host_id
            let agent_tx_opt = {
                let agents = state.agents.read().unwrap();
                agents.get(&target_id).cloned()
            };

            if let Some(agent_tx) = agent_tx_opt {
                // Forward the SDP Answer to the agent
                let _ = agent_tx.send(ServerToAgentMessage::Signaling(SignalingMessage::Sdp {
                    target_id: conn_id.to_string(),
                    sdp,
                    ice_servers: None,
                    webtransport_port,
                    webtransport_cert_hash,
                    webtransport_token,
                }));
                info!("SDP Answer forwarded to agent {}", target_id);
                return;
            }

            // Fallback: local session
            let session = {
                let sessions = state.local_sessions.read().unwrap();
                sessions.get(&target_id).cloned()
            };

            if let Some(session) = session {
                match RTCSessionDescription::answer(sdp.sdp) {
                    Ok(rtc_sdp) => {
                        if let Err(e) = session
                            .peer_connection
                            .set_remote_description(rtc_sdp)
                            .await
                        {
                            error!("Failed to set remote description: {:?}", e);
                        } else {
                            info!(
                                "SDP Answer set successfully on local session for host {}",
                                target_id
                            );
                        }
                    }
                    Err(e) => {
                        error!("Failed to parse SDP Answer: {:?}", e);
                    }
                }
            } else {
                warn!(
                    "Received SDP without active local session for host {}",
                    target_id
                );
            }
        }
        SignalingMessage::IceCandidate {
            target_id,
            candidate,
        } => {
            // target_id is the host_id

            // Check if there is an online Agent registered for this host_id
            let agent_tx_opt = {
                let agents = state.agents.read().unwrap();
                agents.get(&target_id).cloned()
            };

            if let Some(agent_tx) = agent_tx_opt {
                // Forward IceCandidate to the agent
                let _ = agent_tx.send(ServerToAgentMessage::Signaling(
                    SignalingMessage::IceCandidate {
                        target_id: conn_id.to_string(),
                        candidate,
                    },
                ));
                return;
            }

            // Fallback: local session
            let session = {
                let sessions = state.local_sessions.read().unwrap();
                sessions.get(&target_id).cloned()
            };

            if let Some(session) = session {
                let rtc_cand = RTCIceCandidateInit {
                    candidate: candidate.candidate,
                    sdp_mid: candidate.sdp_mid,
                    sdp_mline_index: candidate.sdp_mline_index,
                    username_fragment: candidate.username_fragment,
                    ..Default::default()
                };
                if let Err(e) = session.peer_connection.add_ice_candidate(rtc_cand).await {
                    debug!("Failed to add ICE candidate on local session: {:?}", e);
                }
            }
        }
        SignalingMessage::EndSession { target_id } => {
            // target_id is the host_id
            info!(
                "Session ended by client {} for host {}",
                client_id, target_id
            );

            // Only the connection that holds the host's session ends it;
            // otherwise another stream on that host would be marked idle.
            let ours = {
                let mut map = state.client_to_agent.write().unwrap();
                let ours = map.get(conn_id).is_some_and(|h| *h == target_id);
                if ours {
                    map.remove(conn_id);
                }
                ours
            };
            if !ours {
                debug!("EndSession from {} for {} without a session there; ignored", conn_id, target_id);
                return;
            }

            // Check if there is an online Agent registered for this host_id
            let agent_tx_opt = {
                let agents = state.agents.read().unwrap();
                agents.get(&target_id).cloned()
            };

            if let Some(agent_tx) = agent_tx_opt {
                // Forward EndSession to the agent
                let _ = agent_tx.send(ServerToAgentMessage::Signaling(
                    SignalingMessage::EndSession {
                        target_id: conn_id.to_string(),
                    },
                ));
                state.set_host_status(&target_id, HostStatus::Online).await;
                info!("EndSession forwarded to agent {}", target_id);
                return;
            }

            // Fallback: local session
            let session = {
                let mut sessions = state.local_sessions.write().unwrap();
                sessions.remove(&target_id)
            };
            if let Some(session) = session {
                let _ = session.peer_connection.close().await;
            }
            state.set_host_status(&target_id, HostStatus::Online).await;
        }

        SignalingMessage::GetAppList { target_id } => {
            let agent_tx_opt = {
                let agents = state.agents.read().unwrap();
                agents.get(&target_id).cloned()
            };
            if let Some(agent_tx) = agent_tx_opt {
                let _ = agent_tx.send(ServerToAgentMessage::Signaling(
                    SignalingMessage::GetAppList {
                        target_id: conn_id.to_string(),
                    },
                ));
            } else {
                warn!("No agent connected for host {}, cannot get app list", target_id);
                if let Some(client_tx) = state.clients.read().unwrap().get(conn_id) {
                    let _ = client_tx.send(ServerToClientMessage::Signaling(
                        SignalingMessage::Error {
                            message: "No agent connected for this host.".to_string(),
                        },
                    ));
                }
            }
        }
        SignalingMessage::StopActiveStream { target_id } => {
            let agent_tx_opt = {
                let agents = state.agents.read().unwrap();
                agents.get(&target_id).cloned()
            };
            if let Some(agent_tx) = agent_tx_opt {
                let _ = agent_tx.send(ServerToAgentMessage::Signaling(
                    SignalingMessage::StopActiveStream {
                        target_id: conn_id.to_string(),
                    },
                ));
            } else {
                warn!("No agent connected for host {}, cannot stop stream", target_id);
                if let Some(client_tx) = state.clients.read().unwrap().get(conn_id) {
                    let _ = client_tx.send(ServerToClientMessage::Signaling(
                        SignalingMessage::StopActiveStreamResponse {
                            target_id: target_id.clone(),
                            success: false,
                            error: Some("No agent connected for this host.".to_string()),
                        },
                    ));
                }
            }
        }
        SignalingMessage::GetCapabilities { target_id } => {
            info!("GetCapabilities from client {} for host {} (conn: {})", client_id, target_id, conn_id);
            let agents = state.agents.read().unwrap();
            if let Some(agent_tx) = agents.get(&target_id) {
                let msg = ServerToAgentMessage::Signaling(SignalingMessage::GetCapabilities {
                    target_id: conn_id.to_string(),
                });
                if let Err(e) = agent_tx.send(msg) {
                    error!("Failed to forward GetCapabilities to agent {}: {:?}", target_id, e);
                } else {
                    info!("GetCapabilities forwarded to agent {} with conn_id {}", target_id, conn_id);
                }
            } else {
                warn!("No agent found for host {} when handling GetCapabilities", target_id);
            }
        }
        _ => {}
    }
}

/// The host a client message is about, if any.
fn addressed_host(sig: &SignalingMessage) -> Option<&str> {
    match sig {
        SignalingMessage::RequestSession { host_id, .. }
        | SignalingMessage::RequestFileSession { host_id } => Some(host_id),
        SignalingMessage::Sdp { target_id, .. }
        | SignalingMessage::IceCandidate { target_id, .. }
        | SignalingMessage::EndSession { target_id }
        | SignalingMessage::GetAppList { target_id }
        | SignalingMessage::StopActiveStream { target_id }
        | SignalingMessage::GetCapabilities { target_id } => Some(target_id),
        _ => None,
    }
}

/// Whether a client connection may send `sig` to `host_id`: the user may use
/// the host (same rule as the host list), or it is ending its own session.
async fn client_may_address(
    state: &SignalingState,
    user_id: &str,
    conn_id: &str,
    sig: &SignalingMessage,
    host_id: &str,
) -> bool {
    if matches!(sig, SignalingMessage::EndSession { .. }) {
        let own_stream = state.client_to_agent.read().unwrap().get(conn_id).is_some_and(|h| h == host_id);
        let own_file = state.file_sessions.read().unwrap().contains_key(conn_id);
        if own_stream || own_file {
            return true;
        }
    }
    security::may_use_host(&state.db, user_id, host_id).await
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn state() -> Arc<SignalingState> {
        let db = crate::security::tests::test_db().await;
        Arc::new(SignalingState::new(db, "t".into(), None))
    }

    #[tokio::test]
    async fn closing_a_users_connections() {
        let state = state().await;
        let (tx, mut rx) = crate::queue::channel::<ServerToClientMessage>();
        let (tx2, mut rx2) = crate::queue::channel::<ServerToClientMessage>();
        state.clients.write().unwrap().insert("c1".into(), tx);
        state.clients.write().unwrap().insert("c2".into(), tx2);
        state.client_connections.write().unwrap().insert("c1".into(), "alice".into());
        state.client_connections.write().unwrap().insert("c2".into(), "bob".into());
        state.close_user_connections("alice");
        assert!(rx.recv().await.is_none());
        state.send_to_conn("c2", ServerToClientMessage::Signaling(SignalingMessage::Error { message: "x".into() }));
        assert!(rx2.recv().await.is_some());
    }

    #[tokio::test]
    async fn agent_ends_only_its_own_sessions() {
        let state = state().await;
        let (tx, mut rx) = crate::queue::channel::<ServerToClientMessage>();
        state.clients.write().unwrap().insert("c1".into(), tx);
        state.client_to_agent.write().unwrap().insert("c1".into(), "host-a".into());
        // Another agent can't end it.
        handle_agent_signaling(SignalingMessage::EndSession { target_id: "c1".into() }, "host-b", state.clone()).await;
        assert_eq!(state.client_to_agent.read().unwrap().get("c1").map(String::as_str), Some("host-a"));
        // Its own agent can, and the client hears about it.
        handle_agent_signaling(SignalingMessage::EndSession { target_id: "c1".into() }, "host-a", state.clone()).await;
        assert!(state.client_to_agent.read().unwrap().get("c1").is_none());
        assert!(matches!(
            rx.recv().await,
            Some(ServerToClientMessage::Signaling(SignalingMessage::EndSession { .. }))
        ));
    }

    #[tokio::test]
    async fn client_messages_need_host_access() {
        let state = state().await;
        sqlx::query("INSERT INTO users (id, username, password_hash, role, is_active) VALUES ('u1', 'u1', 'x', 'user', 1)")
            .execute(&state.db)
            .await
            .unwrap();
        let (agent_tx, mut agent_rx) = crate::queue::channel::<ServerToAgentMessage>();
        state.agents.write().unwrap().insert("h1".into(), agent_tx);
        let (tx, mut rx) = crate::queue::channel::<ServerToClientMessage>();
        state.clients.write().unwrap().insert("c1".into(), tx);

        handle_client_signaling(SignalingMessage::GetCapabilities { target_id: "h1".into() }, "u1", "c1", state.clone()).await;
        assert!(matches!(
            rx.recv().await,
            Some(ServerToClientMessage::Signaling(SignalingMessage::Error { .. }))
        ));

        // In a group with the host: forwarded.
        sqlx::query("INSERT INTO hosts (id, name, status) VALUES ('h1', 'H1', 'Online')").execute(&state.db).await.unwrap();
        sqlx::query("INSERT INTO groups (id, name) VALUES ('g', 'G')").execute(&state.db).await.unwrap();
        sqlx::query("INSERT INTO host_groups VALUES ('h1', 'g')").execute(&state.db).await.unwrap();
        sqlx::query("INSERT INTO user_groups VALUES ('u1', 'g')").execute(&state.db).await.unwrap();
        handle_client_signaling(SignalingMessage::GetCapabilities { target_id: "h1".into() }, "u1", "c1", state.clone()).await;
        assert!(matches!(
            agent_rx.recv().await,
            Some(ServerToAgentMessage::Signaling(SignalingMessage::GetCapabilities { .. }))
        ));
    }

    #[tokio::test]
    async fn agent_key_is_stored_then_required() {
        let state = state().await;
        let key = "00112233445566778899aabbccddeeff";
        // Old agent, new id: accepted, nothing stored.
        assert!(check_agent_key(&state, "h1", "PC", None).await.unwrap());
        // First key: stored.
        assert!(check_agent_key(&state, "h1", "PC", Some(key)).await.unwrap());
        assert!(check_agent_key(&state, "h1", "PC", Some(key)).await.unwrap());
        // Someone else with the shared token can't take the id over.
        assert!(!check_agent_key(&state, "h1", "PC", Some("ffffffffffffffffffffffffffffffff")).await.unwrap());
        assert!(!check_agent_key(&state, "h1", "PC", None).await.unwrap());
    }

    #[tokio::test]
    async fn key_cannot_claim_an_online_keyless_agent() {
        let state = state().await;
        let key = "00112233445566778899aabbccddeeff";
        // An old agent (no key) is online as h1.
        let (tx, _rx) = crate::queue::channel::<ServerToAgentMessage>();
        state.agents.write().unwrap().insert("h1".into(), tx);
        assert!(!check_agent_key(&state, "h1", "PC", Some(key)).await.unwrap());
        let hash: Option<String> = sqlx::query_scalar("SELECT agent_key_hash FROM hosts WHERE id = 'h1'")
            .fetch_optional(&state.db)
            .await
            .unwrap()
            .flatten();
        assert!(hash.is_none());
        // The old agent itself may still reconnect.
        assert!(check_agent_key(&state, "h1", "PC", None).await.unwrap());
        // Offline: the first key is stored as before.
        state.agents.write().unwrap().remove("h1");
        assert!(check_agent_key(&state, "h1", "PC", Some(key)).await.unwrap());
    }

    #[tokio::test]
    async fn agent_token_limit_tolerates_retrying_agents() {
        let state = state().await;
        let limit = &state.agent_token_limit;
        let now = std::time::Instant::now();
        // A stale agent retrying every 3 s for 5 minutes.
        for _ in 0..100 {
            limit.record_failure("10.0.0.9", now);
        }
        assert!(!limit.is_blocked("10.0.0.9", now));
        for _ in 0..200 {
            limit.record_failure("10.0.0.9", now);
        }
        assert!(limit.is_blocked("10.0.0.9", now));
    }

    async fn host_status(state: &SignalingState, id: &str) -> String {
        sqlx::query_scalar("SELECT status FROM hosts WHERE id = ?").bind(id).fetch_one(&state.db).await.unwrap()
    }

    #[tokio::test]
    async fn only_the_sessions_connection_ends_it() {
        let state = state().await;
        sqlx::query("INSERT INTO users (id, username, password_hash, role, is_active) VALUES ('a', 'a', 'x', 'admin', 1)")
            .execute(&state.db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO hosts (id, name, status) VALUES ('h1', 'H1', 'Online')").execute(&state.db).await.unwrap();
        let (agent_tx, mut agent_rx) = crate::queue::channel::<ServerToAgentMessage>();
        state.agents.write().unwrap().insert("h1".into(), agent_tx);
        for conn in ["c1", "c2"] {
            let (tx, _rx) = crate::queue::channel::<ServerToClientMessage>();
            state.clients.write().unwrap().insert(conn.into(), tx);
        }
        let request = || SignalingMessage::RequestSession {
            host_id: "h1".into(),
            width: None,
            height: None,
            fps: None,
            bitrate: None,
            codec: None,
            app_id: None,
            encoder: None,
            display_id: None,
            chroma: None,
            encoder_csc_mode: None,
            microphone: None,
            source_id: None,
            decode_codecs: None,
        };
        handle_client_signaling(request(), "a", "c1", state.clone()).await;
        // A second connection takes the host over (the agent runs one session).
        handle_client_signaling(request(), "a", "c2", state.clone()).await;
        assert!(state.client_to_agent.read().unwrap().get("c1").is_none());
        assert_eq!(host_status(&state, "h1").await, "Busy");
        while agent_rx.rx_try().is_some() {}

        // c1 ending "its" session: nothing reaches the agent, h1 stays busy.
        handle_client_signaling(SignalingMessage::EndSession { target_id: "h1".into() }, "a", "c1", state.clone()).await;
        assert!(agent_rx.rx_try().is_none());
        assert_eq!(host_status(&state, "h1").await, "Busy");

        // c2 ends it for real.
        handle_client_signaling(SignalingMessage::EndSession { target_id: "h1".into() }, "a", "c2", state.clone()).await;
        assert!(matches!(
            agent_rx.rx_try(),
            Some(ServerToAgentMessage::Signaling(SignalingMessage::EndSession { .. }))
        ));
        assert_eq!(host_status(&state, "h1").await, "Online");
    }

    /// The source asked for and the codecs the client decodes reach the
    /// agent; the agent's sources and what it says of a source reach the
    /// client. Messages from clients and agents of before these fields
    /// still parse, and mean "no source".
    #[tokio::test]
    async fn sources_pass_through_and_older_peers_still_work() {
        let state = state().await;
        sqlx::query("INSERT INTO users (id, username, password_hash, role, is_active) VALUES ('a', 'a', 'x', 'admin', 1)")
            .execute(&state.db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO hosts (id, name, status) VALUES ('h1', 'H1', 'Online')").execute(&state.db).await.unwrap();
        let (agent_tx, mut agent_rx) = crate::queue::channel::<ServerToAgentMessage>();
        state.agents.write().unwrap().insert("h1".into(), agent_tx);
        let (client_tx, mut client_rx) = crate::queue::channel::<ServerToClientMessage>();
        state.clients.write().unwrap().insert("c1".into(), client_tx);

        // A new client: the request as it is on the wire.
        let new_client: SignalingMessage = serde_json::from_str(
            r#"{"type":"RequestSession","payload":{"host_id":"h1","width":1280,"height":720,"fps":60,"bitrate":8000,"codec":"h264","app_id":null,"encoder":null,"display_id":null,"source_id":"office-pc","decode_codecs":["h264","vp8","av1"]}}"#,
        )
        .unwrap();
        handle_client_signaling(new_client, "a", "c1", state.clone()).await;
        match agent_rx.rx_try() {
            Some(ServerToAgentMessage::Signaling(SignalingMessage::IncomingSession { source_id, decode_codecs, client_id, .. })) => {
                assert_eq!(client_id, "c1");
                assert_eq!(source_id.as_deref(), Some("office-pc"));
                assert_eq!(decode_codecs, Some(vec!["h264".to_string(), "vp8".to_string(), "av1".to_string()]));
            }
            _ => panic!("the agent got no IncomingSession"),
        }

        // A client from before the fields: the same request without them.
        let old_client: SignalingMessage = serde_json::from_str(
            r#"{"type":"RequestSession","payload":{"host_id":"h1","width":1280,"height":720,"fps":60,"bitrate":8000,"codec":"h264","app_id":null,"encoder":null,"display_id":null}}"#,
        )
        .unwrap();
        handle_client_signaling(old_client, "a", "c1", state.clone()).await;
        while let Some(message) = agent_rx.rx_try() {
            if let ServerToAgentMessage::Signaling(SignalingMessage::IncomingSession { source_id, decode_codecs, .. }) = &message {
                assert!(source_id.is_none() && decode_codecs.is_none());
                // What an agent from before the fields is sent has no trace of them.
                let wire = serde_json::to_string(&message).unwrap();
                assert!(!wire.contains("source_id") && !wire.contains("decode_codecs"), "{}", wire);
            }
        }
        while client_rx.rx_try().is_some() {}

        // The agent's capabilities: with sources (new agent) and without (old agent).
        let new_agent: SignalingMessage = serde_json::from_str(
            r#"{"type":"CapabilitiesResponse","payload":{"target_id":"c1","displays":[],"encoders":["native"],"gpu_info":null,"host_os":"linux","sources":[{"id":"office-pc","name":"Office PC","connector":"rustdesk"}]}}"#,
        )
        .unwrap();
        handle_agent_signaling(new_agent, "h1", state.clone()).await;
        match client_rx.rx_try() {
            Some(ServerToClientMessage::Signaling(SignalingMessage::CapabilitiesResponse { target_id, sources, .. })) => {
                assert_eq!(target_id, "h1");
                assert_eq!(sources, vec![common::SourceInfoMsg { id: "office-pc".into(), name: "Office PC".into(), connector: "rustdesk".into() }]);
            }
            _ => panic!("the client got no CapabilitiesResponse"),
        }
        let old_agent: SignalingMessage = serde_json::from_str(
            r#"{"type":"CapabilitiesResponse","payload":{"target_id":"c1","displays":[],"encoders":["native"],"gpu_info":null,"host_os":"linux"}}"#,
        )
        .unwrap();
        handle_agent_signaling(old_agent, "h1", state.clone()).await;
        match client_rx.rx_try() {
            Some(message @ ServerToClientMessage::Signaling(SignalingMessage::CapabilitiesResponse { .. })) => {
                // A client from before the field is not sent an empty list either.
                assert!(!serde_json::to_string(&message).unwrap().contains("sources"));
            }
            _ => panic!("the client got no CapabilitiesResponse"),
        }

        // What the agent says of a source (it failed, it ended) reaches the client.
        handle_agent_signaling(
            SignalingMessage::SourceStatus { target_id: "c1".into(), status: "failed".into(), reason: Some("Wrong password".into()) },
            "h1",
            state.clone(),
        )
        .await;
        match client_rx.rx_try() {
            Some(ServerToClientMessage::Signaling(SignalingMessage::SourceStatus { target_id, status, reason })) => {
                assert_eq!((target_id.as_str(), status.as_str(), reason.as_deref()), ("h1", "failed", Some("Wrong password")));
            }
            _ => panic!("the client got no SourceStatus"),
        }

        // A message type this build does not know (a newer peer) does not parse,
        // which the socket loops log and skip.
        assert!(serde_json::from_str::<SignalingMessage>(r#"{"type":"SomethingNewer","payload":{"target_id":"c1"}}"#).is_err());
    }
}
