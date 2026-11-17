//! Sources: computers of other types (RustDesk by IP address, ...) that a
//! Lunaris computer shows through a connector, listed as computers of their
//! own.
//!
//! What it takes to reach one (its address, its password) is kept by that
//! Lunaris computer's agent and nowhere else: when one is added or changed
//! here, the fields pass through once, in the message to the agent, and are
//! neither stored nor logged. The server keeps the id, the name, the type
//! and the Lunaris computer it is reached through, as the agent announces
//! them (`AgentMessage::Sources`), to list it while that computer is away.
//!
//! Who may do what: listing and connecting need the right to use the
//! Lunaris computer (`security::host_access`, as for the computer itself);
//! adding, changing and removing are for administrators, as adding and
//! removing computers is. A computer that asks at its screen before each
//! connection does not take changes from here (its agent says so and
//! refuses them); connecting to its sources is asked for there like any
//! other connection.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use common::{ConnectorInfoMsg, ServerToAgentMessage, SourceChange, SourceInfoMsg};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use tracing::{error, info, warn};

use crate::signaling::SignalingState;

/// How long an agent has to answer a change.
const CHANGE_TIMEOUT: Duration = Duration::from_secs(15);
/// The most sources and connectors taken from one agent's announcement.
const MAX_ANNOUNCED: usize = 200;

/// What a connected agent announced.
#[derive(Clone, Default)]
pub struct Announced {
    pub connectors: Vec<ConnectorInfoMsg>,
    pub manageable: bool,
}

/// The answer to a change: the source's id, or why not.
type Answer = Result<String, String>;

#[derive(Default)]
pub struct Registry {
    /// agent id -> what it announced, while it is connected.
    announced: RwLock<HashMap<String, Announced>>,
    /// request id -> (the agent asked, who waits for its answer).
    pending: Mutex<HashMap<String, (String, oneshot::Sender<Answer>)>>,
}

/// An agent told its sources and connectors: they are kept (the sources in
/// the database, to list them while it is away).
pub(crate) async fn announced(
    state: &Arc<SignalingState>,
    agent_id: &str,
    mut sources: Vec<SourceInfoMsg>,
    mut connectors: Vec<ConnectorInfoMsg>,
    manageable: bool,
) {
    sources.truncate(MAX_ANNOUNCED);
    connectors.truncate(MAX_ANNOUNCED);
    let mut tx = match state.db.begin().await {
        Ok(tx) => tx,
        Err(e) => {
            error!("Database error: {}", e);
            return;
        }
    };
    let saved: Result<(), sqlx::Error> = async {
        sqlx::query("DELETE FROM host_sources WHERE host_id = ?").bind(agent_id).execute(&mut *tx).await?;
        for source in &sources {
            let connector_name = connectors.iter().find(|c| c.id == source.connector).map(|c| c.name.as_str()).unwrap_or(&source.connector);
            sqlx::query("INSERT OR REPLACE INTO host_sources (host_id, source_id, name, connector, connector_name) VALUES (?, ?, ?, ?, ?)")
                .bind(agent_id)
                .bind(&source.id)
                .bind(&source.name)
                .bind(&source.connector)
                .bind(connector_name)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await
    }
    .await;
    if let Err(e) = saved {
        // A host that is not in the list (yet) has no sources either.
        warn!("The sources of {} could not be saved: {}", agent_id, e);
    }
    state.sources.announced.write().unwrap().insert(agent_id.to_string(), Announced { connectors, manageable });
}

/// An agent answered a change. Only the agent that was asked is believed.
pub(crate) fn changed(state: &Arc<SignalingState>, agent_id: &str, request_id: &str, id: Option<String>, error: Option<String>) {
    let mut pending = state.sources.pending.lock().unwrap();
    if !pending.get(request_id).is_some_and(|(asked, _)| asked == agent_id) {
        return;
    }
    if let Some((_, waiting)) = pending.remove(request_id) {
        let answer = match (id, error) {
            (Some(id), None) => Ok(id),
            (_, Some(error)) => Err(error),
            (None, None) => Err("That computer did not say whether it worked".to_string()),
        };
        let _ = waiting.send(answer);
    }
}

/// The agent went away: its connectors are not offered any more. Its
/// sources stay listed (offline).
pub(crate) fn agent_gone(state: &Arc<SignalingState>, agent_id: &str) {
    state.sources.announced.write().unwrap().remove(agent_id);
}

#[derive(Serialize)]
struct SourceEntry {
    host_id: String,
    host_name: String,
    id: String,
    name: String,
    connector: String,
    connector_name: String,
    online: bool,
}

/// GET /api/sources: the sources of the computers the user may use.
pub async fn list_sources(user: crate::auth::AuthenticatedUser, State(state): State<Arc<SignalingState>>) -> impl IntoResponse {
    let access = match crate::security::host_access(&state.db, &user.user_id).await {
        Ok(access) => access,
        Err(e) => {
            error!("Database error: {}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": "Internal Database Error" }))).into_response();
        }
    };
    let rows: Vec<(String, String, String, String, String, String)> = match sqlx::query_as(
        "SELECT s.host_id, h.name, s.source_id, s.name, s.connector, s.connector_name
         FROM host_sources s INNER JOIN hosts h ON h.id = s.host_id
         ORDER BY s.name COLLATE NOCASE, s.source_id",
    )
    .fetch_all(&state.db)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            error!("Database error: {}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": "Internal Database Error" }))).into_response();
        }
    };
    let online = state.agents.read().unwrap();
    let entries: Vec<SourceEntry> = rows
        .into_iter()
        .filter(|row| access.allows(&row.0))
        .map(|(host_id, host_name, id, name, connector, connector_name)| SourceEntry {
            online: online.contains_key(&host_id),
            host_id,
            host_name,
            id,
            name,
            connector,
            connector_name,
        })
        .collect();
    Json(entries).into_response()
}

#[derive(Serialize)]
struct HostConnectors {
    host_id: String,
    host_name: String,
    connectors: Vec<ConnectorInfoMsg>,
}

/// GET /api/connectors: the kinds of computers that can be added, by the
/// Lunaris computer they would be reached through (connected ones that
/// take changes from here).
pub async fn list_connectors(user: crate::auth::AuthenticatedUser, State(state): State<Arc<SignalingState>>) -> impl IntoResponse {
    let access = match crate::security::host_access(&state.db, &user.user_id).await {
        Ok(access) => access,
        Err(e) => {
            error!("Database error: {}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": "Internal Database Error" }))).into_response();
        }
    };
    let announced: Vec<(String, Announced)> = state
        .sources
        .announced
        .read()
        .unwrap()
        .iter()
        .filter(|(host_id, a)| a.manageable && !a.connectors.is_empty() && access.allows(host_id))
        .map(|(host_id, a)| (host_id.clone(), a.clone()))
        .collect();
    let mut hosts = Vec::new();
    for (host_id, a) in announced {
        if !state.agents.read().unwrap().contains_key(&host_id) {
            continue;
        }
        let host_name: Option<String> = sqlx::query_scalar("SELECT name FROM hosts WHERE id = ?").bind(&host_id).fetch_optional(&state.db).await.ok().flatten();
        let Some(host_name) = host_name else { continue };
        hosts.push(HostConnectors { host_id, host_name, connectors: a.connectors });
    }
    hosts.sort_by(|a, b| a.host_name.to_lowercase().cmp(&b.host_name.to_lowercase()).then(a.host_id.cmp(&b.host_id)));
    Json(hosts).into_response()
}

#[derive(Deserialize)]
pub struct SourceForm {
    name: String,
    #[serde(default)]
    connector: String,
    #[serde(default)]
    values: std::collections::BTreeMap<String, String>,
}

fn refused(status: StatusCode, text: &str) -> axum::response::Response {
    (status, Json(serde_json::json!({ "error": text }))).into_response()
}

/// Asks the agent of `host_id` to make `change` and waits for its answer.
async fn change(state: &Arc<SignalingState>, host_id: &str, change: SourceChange, requester: &str) -> axum::response::Response {
    let agent = state.agents.read().unwrap().get(host_id).cloned();
    let Some(agent) = agent else {
        return refused(StatusCode::CONFLICT, "That computer is offline");
    };
    // An agent that did not announce its sources is from before this: the
    // message would be nothing it knows (and it would write it to its log).
    let announced = state.sources.announced.read().unwrap().get(host_id).cloned();
    let Some(announced) = announced else {
        return refused(StatusCode::CONFLICT, "The Lunaris on that computer is too old for this. Update it first.");
    };
    if !announced.manageable {
        return refused(
            StatusCode::CONFLICT,
            "That computer asks at its screen before each connection, so computers cannot be added to it from here. Add it in the Lunaris app on that computer.",
        );
    }
    let request_id = uuid::Uuid::new_v4().to_string();
    let (answer_tx, answer_rx) = oneshot::channel();
    state.sources.pending.lock().unwrap().insert(request_id.clone(), (host_id.to_string(), answer_tx));
    info!("Sources of {}: {:?} asked for by {}", host_id, change, requester);
    let sent = agent.send(ServerToAgentMessage::ChangeSource { request_id: request_id.clone(), change, requester: Some(requester.to_string()) });
    let answer = match sent {
        Ok(()) => tokio::time::timeout(CHANGE_TIMEOUT, answer_rx).await,
        Err(()) => {
            state.sources.pending.lock().unwrap().remove(&request_id);
            return refused(StatusCode::CONFLICT, "That computer is offline");
        }
    };
    state.sources.pending.lock().unwrap().remove(&request_id);
    match answer {
        Ok(Ok(Ok(id))) => Json(serde_json::json!({ "id": id })).into_response(),
        Ok(Ok(Err(text))) => refused(StatusCode::UNPROCESSABLE_ENTITY, &text),
        Ok(Err(_)) => refused(StatusCode::CONFLICT, "That computer went offline"),
        Err(_) => refused(StatusCode::GATEWAY_TIMEOUT, "That computer did not answer"),
    }
}

/// POST /api/hosts/{host_id}/sources
pub async fn add_source(
    admin: crate::auth::AdminUser,
    Path(host_id): Path<String>,
    State(state): State<Arc<SignalingState>>,
    Json(form): Json<SourceForm>,
) -> impl IntoResponse {
    change(&state, &host_id, SourceChange::Add { name: form.name, connector: form.connector, values: form.values }, &admin.username).await
}

/// PUT /api/hosts/{host_id}/sources/{id}
pub async fn update_source(
    admin: crate::auth::AdminUser,
    Path((host_id, id)): Path<(String, String)>,
    State(state): State<Arc<SignalingState>>,
    Json(form): Json<SourceForm>,
) -> impl IntoResponse {
    change(&state, &host_id, SourceChange::Update { id, name: form.name, values: form.values }, &admin.username).await
}

/// DELETE /api/hosts/{host_id}/sources/{id}
pub async fn remove_source(
    admin: crate::auth::AdminUser,
    Path((host_id, id)): Path<(String, String)>,
    State(state): State<Arc<SignalingState>>,
) -> impl IntoResponse {
    // The agent is away: the card is taken off the list here, and the
    // agent's next announcement decides (it still has the source).
    if !state.agents.read().unwrap().contains_key(&host_id) {
        return refused(StatusCode::CONFLICT, "That computer is offline. It can be removed when it is back.");
    }
    let response = change(&state, &host_id, SourceChange::Remove { id }, &admin.username).await;
    if response.status().is_success() {
        return Json(serde_json::json!({})).into_response();
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::{AgentMessage, ConnectorFieldMsg};

    async fn state() -> Arc<SignalingState> {
        let db = crate::security::tests::test_db().await;
        let state = Arc::new(SignalingState::new(db, "t".into(), None));
        for (id, role) in [("admin", "admin"), ("alice", "user"), ("bob", "user")] {
            sqlx::query("INSERT INTO users (id, username, password_hash, role, is_active) VALUES (?, ?, 'x', ?, 1)")
                .bind(id)
                .bind(format!("{}-test", id))
                .bind(role)
                .execute(&state.db)
                .await
                .unwrap();
        }
        for (id, name) in [("h1", "Office PC"), ("h2", "Lab PC")] {
            sqlx::query("INSERT INTO hosts (id, name, status) VALUES (?, ?, 'Online')").bind(id).bind(name).execute(&state.db).await.unwrap();
        }
        // alice may use h1 (through a group); bob may use nothing.
        sqlx::query("INSERT INTO groups (id, name) VALUES ('g', 'G')").execute(&state.db).await.unwrap();
        sqlx::query("INSERT INTO user_groups (user_id, group_id) VALUES ('alice', 'g')").execute(&state.db).await.unwrap();
        sqlx::query("INSERT INTO host_groups (host_id, group_id) VALUES ('h1', 'g')").execute(&state.db).await.unwrap();
        state
    }

    fn user(id: &str) -> crate::auth::AuthenticatedUser {
        crate::auth::AuthenticatedUser { user_id: id.into(), username: id.into(), role: if id == "admin" { "admin".into() } else { "user".into() } }
    }

    fn admin() -> crate::auth::AdminUser {
        crate::auth::AdminUser { user_id: "admin".into(), username: "admin".into() }
    }

    fn rustdesk() -> ConnectorInfoMsg {
        let field = |key: &str, kind: &str, required: bool| ConnectorFieldMsg { key: key.into(), label: key.into(), kind: kind.into(), default: String::new(), required };
        ConnectorInfoMsg { id: "rustdesk".into(), name: "RustDesk (by IP address)".into(), fields: vec![field("host", "text", true), field("password", "secret", true)] }
    }

    fn source(id: &str, name: &str) -> SourceInfoMsg {
        SourceInfoMsg { id: id.into(), name: name.into(), connector: "rustdesk".into() }
    }

    async fn body(response: impl IntoResponse) -> (StatusCode, serde_json::Value) {
        let response = response.into_response();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
    }

    /// Connects a pretend agent for `host`; returns what the server sends it.
    fn connect(state: &Arc<SignalingState>, host: &str) -> crate::queue::Inbox<ServerToAgentMessage> {
        let (tx, rx) = crate::queue::channel::<ServerToAgentMessage>();
        state.agents.write().unwrap().insert(host.into(), tx);
        rx
    }

    #[tokio::test]
    async fn sources_are_listed_for_those_who_may_use_the_computer() {
        let state = state().await;
        let _h1 = connect(&state, "h1");
        announced(&state, "h1", vec![source("lab-1", "Lab server"), source("den-2", "Den PC")], vec![rustdesk()], true).await;
        announced(&state, "h2", vec![source("x-3", "Other")], vec![rustdesk()], true).await;

        // The administrator sees all of them; h2's agent is not connected.
        let (status, list) = body(list_sources(user("admin"), State(state.clone())).await).await;
        assert_eq!(status, StatusCode::OK);
        let names: Vec<(&str, &str, bool)> =
            list.as_array().unwrap().iter().map(|s| (s["name"].as_str().unwrap(), s["host_name"].as_str().unwrap(), s["online"].as_bool().unwrap())).collect();
        assert_eq!(names, [("Den PC", "Office PC", true), ("Lab server", "Office PC", true), ("Other", "Lab PC", false)]);
        let first = &list[0];
        assert_eq!((first["host_id"].as_str(), first["id"].as_str(), first["connector"].as_str()), (Some("h1"), Some("den-2"), Some("rustdesk")));
        assert_eq!(first["connector_name"], "RustDesk (by IP address)");

        // alice may use h1 only; bob nothing.
        let (_, list) = body(list_sources(user("alice"), State(state.clone())).await).await;
        assert_eq!(list.as_array().unwrap().len(), 2);
        assert!(list.as_array().unwrap().iter().all(|s| s["host_id"] == "h1"));
        let (_, list) = body(list_sources(user("bob"), State(state.clone())).await).await;
        assert!(list.as_array().unwrap().is_empty());

        // A new announcement replaces the list; the agent going away keeps
        // it (offline) and takes its connectors off the offer.
        announced(&state, "h1", vec![source("lab-1", "Lab server renamed")], vec![rustdesk()], true).await;
        let (_, list) = body(list_sources(user("alice"), State(state.clone())).await).await;
        assert_eq!(list.as_array().unwrap().len(), 1);
        assert_eq!(list[0]["name"], "Lab server renamed");
        let (_, offer) = body(list_connectors(user("alice"), State(state.clone())).await).await;
        assert_eq!(offer.as_array().unwrap().len(), 1);
        assert_eq!((offer[0]["host_id"].as_str(), offer[0]["connectors"][0]["id"].as_str()), (Some("h1"), Some("rustdesk")));
        assert_eq!(offer[0]["connectors"][0]["fields"][1]["kind"], "secret");
        state.agents.write().unwrap().remove("h1");
        agent_gone(&state, "h1");
        let (_, list) = body(list_sources(user("alice"), State(state.clone())).await).await;
        assert_eq!(list[0]["online"], false);
        let (_, offer) = body(list_connectors(user("alice"), State(state.clone())).await).await;
        assert!(offer.as_array().unwrap().is_empty());

        // Removing the Lunaris computer removes its sources from the list.
        sqlx::query("DELETE FROM hosts WHERE id = 'h1'").execute(&state.db).await.unwrap();
        let (_, list) = body(list_sources(user("admin"), State(state.clone())).await).await;
        assert_eq!(list.as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_change_goes_to_the_agent_once_and_nothing_of_it_is_kept() {
        let state = state().await;
        let mut to_agent = connect(&state, "h1");
        announced(&state, "h1", vec![], vec![rustdesk()], true).await;

        // The pretend agent: answers as a new agent does (its new list
        // first, then the answer).
        let agent_state = state.clone();
        let agent = tokio::spawn(async move {
            let Some(ServerToAgentMessage::ChangeSource { request_id, change, requester }) = to_agent.recv().await else { panic!("no ChangeSource") };
            assert_eq!(requester.as_deref(), Some("admin"));
            let SourceChange::Add { name, connector, values } = &change else { panic!("not an add") };
            assert_eq!((name.as_str(), connector.as_str()), ("Den PC", "rustdesk"));
            assert_eq!(values.get("password").map(String::as_str), Some("s3cret-pw"));
            crate::signaling::agent_message(&agent_state, "h1", AgentMessage::Sources { sources: vec![source("den-pc-ab12", "Den PC")], connectors: vec![rustdesk()], manageable: true }).await;
            // An answer from another agent with the same request id is not believed.
            crate::signaling::agent_message(&agent_state, "h2", AgentMessage::SourceChanged { request_id: request_id.clone(), id: None, error: Some("forged".into()) }).await;
            crate::signaling::agent_message(&agent_state, "h1", AgentMessage::SourceChanged { request_id, id: Some("den-pc-ab12".into()), error: None }).await;
            to_agent
        });
        let form = SourceForm {
            name: "Den PC".into(),
            connector: "rustdesk".into(),
            values: [("host".to_string(), "192.168.1.50".to_string()), ("password".to_string(), "s3cret-pw".to_string())].into_iter().collect(),
        };
        let (status, answer) = body(add_source(admin(), Path("h1".into()), State(state.clone()), Json(form)).await).await;
        assert_eq!((status, answer["id"].as_str()), (StatusCode::OK, Some("den-pc-ab12")));
        let mut to_agent = agent.await.unwrap();
        assert!(to_agent.rx_try().is_none(), "one message, once");
        assert!(state.sources.pending.lock().unwrap().is_empty());

        // The list has it at once; the database has the name and the type and
        // nothing of the address or the password, in any table.
        let (_, list) = body(list_sources(user("alice"), State(state.clone())).await).await;
        assert_eq!(list[0]["id"], "den-pc-ab12");
        let tables: Vec<String> = sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table'").fetch_all(&state.db).await.unwrap();
        for table in tables {
            let rows: Vec<sqlx::sqlite::SqliteRow> = sqlx::query(sqlx::AssertSqlSafe(format!("SELECT * FROM \"{}\"", table))).fetch_all(&state.db).await.unwrap();
            for row in rows {
                use sqlx::Row;
                for i in 0..row.len() {
                    let text: Option<String> = row.try_get(i).ok().flatten();
                    assert!(!text.as_deref().unwrap_or("").contains("s3cret-pw") && !text.as_deref().unwrap_or("").contains("192.168.1.50"), "{}", table);
                }
            }
        }

        // What the agent refuses comes back as its words.
        let agent_state = state.clone();
        let agent = tokio::spawn(async move {
            let Some(ServerToAgentMessage::ChangeSource { request_id, change, .. }) = to_agent.recv().await else { panic!("no ChangeSource") };
            assert!(matches!(change, SourceChange::Update { ref id, .. } if id == "den-pc-ab12"));
            crate::signaling::agent_message(&agent_state, "h1", AgentMessage::SourceChanged { request_id, id: None, error: Some("host is required".into()) }).await;
            to_agent
        });
        let form = SourceForm { name: "Den".into(), connector: String::new(), values: Default::default() };
        let (status, answer) = body(update_source(admin(), Path(("h1".into(), "den-pc-ab12".into())), State(state.clone()), Json(form)).await).await;
        assert_eq!((status, answer["error"].as_str()), (StatusCode::UNPROCESSABLE_ENTITY, Some("host is required")));
        let mut to_agent = agent.await.unwrap();

        // Removing.
        let agent_state = state.clone();
        let agent = tokio::spawn(async move {
            let Some(ServerToAgentMessage::ChangeSource { request_id, change, .. }) = to_agent.recv().await else { panic!("no ChangeSource") };
            assert_eq!(change, SourceChange::Remove { id: "den-pc-ab12".into() });
            crate::signaling::agent_message(&agent_state, "h1", AgentMessage::Sources { sources: vec![], connectors: vec![rustdesk()], manageable: true }).await;
            crate::signaling::agent_message(&agent_state, "h1", AgentMessage::SourceChanged { request_id, id: Some("den-pc-ab12".into()), error: None }).await;
        });
        let (status, _) = body(remove_source(admin(), Path(("h1".into(), "den-pc-ab12".into())), State(state.clone())).await).await;
        assert_eq!(status, StatusCode::OK);
        agent.await.unwrap();
        let (_, list) = body(list_sources(user("admin"), State(state.clone())).await).await;
        assert!(list.as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn nothing_is_sent_to_an_agent_that_is_old_away_or_asks_at_its_screen() {
        let state = state().await;
        let form = || SourceForm {
            name: "Den PC".into(),
            connector: "rustdesk".into(),
            values: [("password".to_string(), "s3cret-pw".to_string())].into_iter().collect(),
        };
        // Not connected.
        let (status, answer) = body(add_source(admin(), Path("h1".into()), State(state.clone()), Json(form())).await).await;
        assert_eq!((status, answer["error"].as_str()), (StatusCode::CONFLICT, Some("That computer is offline")));

        // Connected, but it never announced sources: an agent from before
        // them. It is sent nothing (it would write the message to its log).
        let mut old_agent = connect(&state, "h1");
        let (status, answer) = body(add_source(admin(), Path("h1".into()), State(state.clone()), Json(form())).await).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(answer["error"].as_str().unwrap().contains("too old"));
        assert!(old_agent.rx_try().is_none());
        // And its messages as it sends them still parse here.
        assert!(serde_json::from_str::<AgentMessage>(r#"{"event":"StatusUpdate","data":{"status":"Online"}}"#).is_ok());

        // An agent that asks at its screen before each connection says so,
        // and is not asked to change anything.
        announced(&state, "h1", vec![source("a-1", "A")], vec![rustdesk()], false).await;
        let (status, answer) = body(add_source(admin(), Path("h1".into()), State(state.clone()), Json(form())).await).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(answer["error"].as_str().unwrap().contains("asks at its screen"));
        let (status, _) = body(remove_source(admin(), Path(("h1".into(), "a-1".into())), State(state.clone())).await).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(old_agent.rx_try().is_none());
        // Its sources are listed all the same (using one is asked for there
        // like any connection), and its connectors are not on offer.
        let (_, list) = body(list_sources(user("alice"), State(state.clone())).await).await;
        assert_eq!(list.as_array().unwrap().len(), 1);
        let (_, offer) = body(list_connectors(user("admin"), State(state.clone())).await).await;
        assert!(offer.as_array().unwrap().is_empty());

        // The messages as they are on the wire; a change is never printed with its values.
        let wire = serde_json::to_string(&ServerToAgentMessage::ChangeSource {
            request_id: "r".into(),
            change: SourceChange::Add { name: "n".into(), connector: "rustdesk".into(), values: form().values },
            requester: None,
        })
        .unwrap();
        assert!(wire.starts_with(r#"{"event":"ChangeSource","data":{"request_id":"r","change":{"op":"add""#), "{}", wire);
        let printed = format!("{:?}", serde_json::from_str::<ServerToAgentMessage>(&wire).unwrap());
        assert!(!printed.contains("s3cret-pw"), "{}", printed);
        // An agent from before sources announces nothing; one from after says `Sources`.
        let announce: AgentMessage =
            serde_json::from_str(r#"{"event":"Sources","data":{"sources":[{"id":"a","name":"A","connector":"rustdesk"}],"connectors":[{"id":"rustdesk","name":"RustDesk"}]}}"#).unwrap();
        assert!(matches!(announce, AgentMessage::Sources { manageable: false, ref connectors, .. } if connectors[0].fields.is_empty()));
    }
}
