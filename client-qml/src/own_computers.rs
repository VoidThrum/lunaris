//! The computers of other types the app added for itself ("Add computer"
//! with a type that is not Lunaris): where they are kept, and the helper
//! that shows them.
//!
//! They are sources of a bridge-only agent (`agent::bridge_agent`) the app
//! runs as a child process: `<this program> agent --cli --bridge-only ...`.
//! It listens on 127.0.0.1 only, shows sources only (never this computer's
//! screen) and takes one password, made here for each run and handed over
//! in the environment. A session to such a computer is then an ordinary
//! direct session to that port; the user interface never sees the address
//! or the password (it asks for the server "bridge:").
//!
//! Their fields (address, password, ...) are in a config file of their
//! own, which the agent that shares this computer never reads: they are
//! not offered to the people who view this computer.

use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use agent::connector::{self, sources, Description};
use agent::pairing::{self, AgentConfig};

/// The "server" of a session to one of these computers, as the user
/// interface names it (with host "direct" and the computer as the source).
pub const BRIDGE_SERVER: &str = "bridge:";

/// How long the helper may take to say which port it listens on.
const START_TIMEOUT: Duration = Duration::from_secs(15);

pub fn is_bridge(server: &str) -> bool {
    server == BRIDGE_SERVER
}

// ---- The store ----

fn load_or_create(path: &Path) -> Result<AgentConfig, String> {
    let name = path.to_string_lossy();
    match pairing::load_config(&name) {
        Ok(config) => Ok(config),
        Err(_) if !path.exists() => {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let config = pairing::new_config();
            // Owner-only from the start (save_config): it will hold passwords.
            pairing::save_config(&config, &name).map_err(|e| format!("cannot save {}: {}", name, e))?;
            Ok(config)
        }
        Err(e) => Err(format!("cannot read {}: {}", name, e)),
    }
}

fn save(config: &AgentConfig, path: &Path) -> Result<(), String> {
    pairing::save_config(config, &path.to_string_lossy()).map_err(|e| e.to_string())
}

fn installed_connectors() -> Vec<Description> {
    connector::available(&connector::connectors_dir())
}

fn id_or_error(result: Result<String, String>) -> String {
    match result {
        Ok(id) => serde_json::json!({ "id": id }),
        Err(error) => serde_json::json!({ "error": error }),
    }
    .to_string()
}

fn sources_at(path: &Path, connectors: &[Description]) -> String {
    // Nothing added yet: no file is made just to list.
    let listed = pairing::load_config(&path.to_string_lossy()).map(|config| config.sources).unwrap_or_default();
    sources::views_json(&listed, connectors)
}

#[cfg(test)]
fn add_at(path: &Path, connectors: &[Description], name: &str, connector: &str, values_json: &str) -> Result<String, String> {
    let mut config = load_or_create(path)?;
    let id = sources::add(&mut config, connectors, name, connector, values_json)?;
    save(&config, path)?;
    Ok(id)
}

/// What the user interface says of a group when it says nothing: adding
/// then groups by address, and changing keeps the group. (No group can be
/// this word; "" is "no group".)
pub const GROUP_UNSAID: &str = "*";

fn group_said(group: &str) -> Option<&str> {
    (group != GROUP_UNSAID).then_some(group)
}

fn update_at(path: &Path, connectors: &[Description], id: &str, name: &str, values_json: &str, group: Option<&str>) -> Result<String, String> {
    let mut config = load_or_create(path)?;
    sources::update_in_group(&mut config, connectors, id, name, values_json, group)?;
    save(&config, path)?;
    Ok(id.to_string())
}

fn remove_at(path: &Path, id: &str) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    let mut config = load_or_create(path)?;
    if sources::remove(&mut config, id) {
        save(&config, path)?;
    }
    Ok(())
}

fn has_source(path: &Path, id: &str) -> bool {
    pairing::load_config(&path.to_string_lossy())
        .map(|config| config.sources.iter().any(|source| source.id == id))
        .unwrap_or(false)
}

/// The types to choose from, as JSON: `[{id, name, fields}]`.
pub fn connectors_json() -> String {
    sources::connectors_json(&installed_connectors())
}

/// The app's own computers of other types, as JSON:
/// `[{id, name, connector, connector_name, values}]` (no secret fields),
/// with `group` for those that are one computer with others.
pub fn sources_json() -> String {
    sources_at(&sources::bridge_config_path(), &installed_connectors())
}

/// Adds one; `{"id": ...}` or `{"error": ...}`, or, when its type has a
/// set-up (a pairing), `{"setup": ..., "message": ..., "code": ...}`: what
/// to show the person, to go on with `source_setup`. Waits up to 3 seconds
/// for a set-up's first step. The running helper reads its sources at each
/// session, so it needs no restart.
///
/// `group` says which computer it is ([`GROUP_UNSAID`]: the one of a
/// computer at the same address, if there is one; "": none).
pub fn add_source(name: &str, connector: &str, values_json: &str, group: &str) -> String {
    begin_add_at(&sources::bridge_config_path(), &connector::connectors_dir(), name, connector, values_json, group_said(group))
}

fn begin_add_at(path: &Path, connectors_dir: &Path, name: &str, connector: &str, values_json: &str, group: Option<&str>) -> String {
    // The set-up runs here, in the app: the helper only shows computers.
    let begun = load_or_create(path)
        .and_then(|_| sources::begin_add_in_group(&path.to_string_lossy(), connectors_dir, name, connector, values_json, group));
    match begun {
        Ok(sources::AddOutcome::Added { id }) => serde_json::json!({ "id": id }),
        Ok(sources::AddOutcome::Setup { setup, step }) => serde_json::json!({ "setup": setup, "message": step.message, "code": step.code }),
        Err(error) => serde_json::json!({ "error": error }),
    }
    .to_string()
}

/// Where the set-up of a computer being added is:
/// `{"state":"pending","message":...,"code":...}`, `{"state":"done","id":...}`
/// or `{"state":"failed","error":...}` (also when it is not known any more).
pub fn source_setup(setup: &str) -> String {
    match sources::setup_state(setup) {
        Some(sources::SetupState::Pending { step }) => serde_json::json!({ "state": "pending", "message": step.message, "code": step.code }),
        Some(sources::SetupState::Done { id }) => serde_json::json!({ "state": "done", "id": id }),
        Some(sources::SetupState::Failed { error }) => serde_json::json!({ "state": "failed", "error": error }),
        None => serde_json::json!({ "state": "failed", "error": sources::SETUP_GONE }),
    }
    .to_string()
}

/// Stops that set-up (the person closed the window).
pub fn cancel_setup(setup: &str) {
    sources::cancel_setup(setup);
}

/// Changes one (an empty field keeps its value); `{"id": ...}` or `{"error": ...}`.
/// `group`: [`GROUP_UNSAID`] keeps its group, "" takes it out of it.
pub fn update_source(id: &str, name: &str, values_json: &str, group: &str) -> String {
    id_or_error(update_at(&sources::bridge_config_path(), &installed_connectors(), id, name, values_json, group_said(group)))
}

/// Removes one; an error message or "".
pub fn remove_source(id: &str) -> String {
    remove_at(&sources::bridge_config_path(), id).err().unwrap_or_default()
}

// ---- Whether they are there ----

fn load_sources(path: &Path) -> Vec<connector::SourceConfig> {
    pairing::load_config(&path.to_string_lossy()).map(|config| config.sources).unwrap_or_default()
}

/// Checks the app's own computers of other types as an agent checks its
/// sources (`agent::connector::reachability`): in the background, from the
/// first time it is asked about one. The helper is not running most of the
/// time, so the app does it itself.
fn monitor() -> &'static connector::reachability::Monitor {
    static MONITOR: std::sync::OnceLock<connector::reachability::Monitor> = std::sync::OnceLock::new();
    MONITOR.get_or_init(|| {
        connector::reachability::Monitor::start(
            Box::new(|| (load_sources(&sources::bridge_config_path()), installed_connectors())),
            // The cards ask again on their own (Home.qml's timer).
            Box::new(|| {}),
        )
    })
}

fn online_at(path: &Path, connectors: &[Description], id: &str, monitor: &connector::reachability::Monitor) -> i32 {
    let Some(source) = load_sources(path).into_iter().find(|source| source.id == id) else { return -1 };
    match monitor.status(&source.target, connectors.iter().find(|c| c.id == source.target.connector)) {
        Some(true) => 1,
        Some(false) => 0,
        None => -1,
    }
}

/// Whether the computer `id` answers: 1 yes, 0 no (it is shut down or
/// cannot be reached), -1 not known (not checked yet, or its type has no
/// address to check).
pub fn source_online(id: &str) -> i32 {
    online_at(&sources::bridge_config_path(), &installed_connectors(), id, monitor())
}

// ---- The helper ----

/// The helper's password for one run: 48 hex characters from the system's
/// random source. Only ever in this process's memory and the child's
/// environment.
fn new_password() -> String {
    pairing::to_hex(&pairing::random_bytes(24))
}

struct Helper {
    child: Child,
    /// Held open: the helper ends when its standard input closes.
    _stdin: Option<ChildStdin>,
    port: u16,
    password: String,
}

static HELPER: Mutex<Option<Helper>> = Mutex::new(None);

fn log_file() -> Option<std::fs::File> {
    let path = sources::bridge_config_path().with_file_name("bridge.log");
    if path.exists() {
        let mut previous = path.clone().into_os_string();
        previous.push(".1");
        let _ = std::fs::rename(&path, previous);
    }
    std::fs::File::create(&path).ok()
}

fn start_helper(config_path: &Path) -> Result<Helper, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let name = hostname::get().ok().and_then(|h| h.into_string().ok()).unwrap_or_default();
    let mut cmd = Command::new(exe);
    cmd.arg("agent")
        .args(agent::bridge_agent::spawn_args(&config_path.to_string_lossy(), &name))
        // What it logs is kept beside its config, as the sharing agent's is.
        .stderr(log_file().map(Stdio::from).unwrap_or_else(Stdio::null));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    spawn_helper(cmd)
}

/// Starts `cmd` as the helper: hands it a new password, holds its standard
/// input and waits for the port it says it listens on.
fn spawn_helper(mut cmd: Command) -> Result<Helper, String> {
    let password = new_password();
    // In the environment, not on the command line, where other users of
    // this computer could read it.
    cmd.env(agent::bridge_agent::PASSWORD_ENV, &password).stdin(Stdio::piped()).stdout(Stdio::piped());
    crate::host::stop_with_parent(&mut cmd);
    let mut child = crate::host::spawn_child(cmd).map_err(|e| format!("cannot start the helper for other computers: {}", e))?;
    let stdin = child.stdin.take();

    // The helper says its port on its first line; the thread then keeps
    // reading so that the pipe never fills.
    let (port_tx, port_rx) = std::sync::mpsc::channel::<u16>();
    if let Some(stdout) = child.stdout.take() {
        std::thread::Builder::new()
            .name("lunaris-bridge-port".into())
            .spawn(move || {
                use std::io::BufRead;
                for line in std::io::BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    if let Some(port) = agent::bridge_agent::port_of_line(&line) {
                        let _ = port_tx.send(port);
                    }
                }
            })
            .ok();
    }
    match port_rx.recv_timeout(START_TIMEOUT) {
        Ok(port) => Ok(Helper { child, _stdin: stdin, port, password }),
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            Err("The helper for other computers did not start".into())
        }
    }
}

/// The running helper's port and password; starts it when there is none or
/// the last one died. Waits for it to listen: not for the UI thread.
fn helper_endpoint() -> Result<(u16, String), String> {
    let mut lock = HELPER.lock().unwrap();
    if let Some(helper) = lock.as_mut() {
        if matches!(helper.child.try_wait(), Ok(None)) {
            return Ok((helper.port, helper.password.clone()));
        }
        *lock = None;
    }
    let path = sources::bridge_config_path();
    load_or_create(&path)?;
    let helper = start_helper(&path)?;
    let endpoint = (helper.port, helper.password.clone());
    *lock = Some(helper);
    Ok(endpoint)
}

/// A "bridge:" session is to a computer the app added: it names it as its
/// source and goes to the helper as a direct connection. Anything else is
/// refused before the helper is started.
fn check_session(config_path: &Path, host_id: &str, source_id: Option<&str>) -> Result<(), String> {
    if host_id != "direct" {
        return Err("This computer cannot be reached that way".into());
    }
    let Some(id) = source_id.filter(|id| !id.is_empty()) else {
        return Err("No computer was chosen".into());
    };
    if !has_source(config_path, id) {
        return Err("That computer is not there any more".into());
    }
    Ok(())
}

/// Where a "bridge:" session really goes: the helper's address
/// ("127.0.0.1:<port>") and its password. Starts the helper when needed
/// and waits for it, so call it off the UI thread.
pub fn session_endpoint(host_id: &str, source_id: Option<&str>) -> Result<(String, String), String> {
    check_session(&sources::bridge_config_path(), host_id, source_id)?;
    let (port, password) = helper_endpoint()?;
    Ok((format!("127.0.0.1:{}", port), password))
}

/// Stops the helper when the app exits, and any set-up still waiting.
pub fn shutdown() {
    sources::cancel_setups_at(&sources::bridge_config_path().to_string_lossy());
    if let Some(mut helper) = HELPER.lock().unwrap().take() {
        let _ = helper.child.kill();
        let _ = helper.child.wait();
    }
}

// ---- The displays of the computer being shown ----

/// The displays of the source being shown, when `data` is the message that
/// lists them on the general data channel
/// (`{"type":"source_displays","displays":[{id,name,width,height}],"current":"<id>"}`):
/// the list as JSON for the stream settings, and the current display's id.
pub fn source_displays(data: &[u8]) -> Option<(String, String)> {
    let text = std::str::from_utf8(data).ok()?;
    // Cursor updates come on the same channel many times a second.
    if !text.contains("source_displays") {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    if value.get("type").and_then(|v| v.as_str()) != Some("source_displays") {
        return None;
    }
    let text_of = |v: Option<&serde_json::Value>| match v {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Number(n)) => n.to_string(),
        _ => String::new(),
    };
    let displays: Vec<serde_json::Value> = value
        .get("displays")?
        .as_array()?
        .iter()
        .filter_map(|d| {
            let id = text_of(d.get("id"));
            if id.is_empty() {
                return None;
            }
            Some(serde_json::json!({
                "id": id,
                "name": text_of(d.get("name")),
                "width": d.get("width").and_then(|v| v.as_u64()).unwrap_or(0),
                "height": d.get("height").and_then(|v| v.as_u64()).unwrap_or(0),
            }))
        })
        .collect();
    Some((serde_json::Value::Array(displays).to_string(), text_of(value.get("current"))))
}

/// The delay between the agent and the computer it shows, there and back,
/// in milliseconds (with a fraction: the agent tells tenths), when `data`
/// is the message that tells it on the general data channel
/// (`{"type":"source_latency","ms":12.3}`, agent connector/media.rs
/// `latency_message`). Only sessions that show another computer through a
/// connector send it.
pub fn source_latency_ms(data: &[u8]) -> Option<f64> {
    let text = std::str::from_utf8(data).ok()?;
    // Cursor updates come on the same channel many times a second.
    if !text.contains("source_latency") {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    if value.get("type").and_then(|v| v.as_str()) != Some("source_latency") {
        return None;
    }
    let ms = value.get("ms")?.as_f64()?;
    // A delay: not negative, and not beyond what the bridge keeps (microseconds in 32 bits).
    (ms.is_finite() && ms >= 0.0).then_some(ms.min(4_000_000.0))
}

/// The ping shown: the viewer's round trip to the agent plus the agent's
/// to the computer it shows (0 when the agent shows its own screen).
pub fn shown_ping_ms(agent_round_trip_ms: f64, source_latency_ms: f64) -> f64 {
    agent_round_trip_ms + source_latency_ms
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_config(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lunaris-own-computers-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("bridge_config.json")
    }

    fn builtin() -> Vec<Description> {
        // An empty folder: the built-in connectors only.
        connector::available(Path::new("/nonexistent-lunaris-connectors"))
    }

    /// Values for every field of `description`, by its kind.
    fn values_for(description: &Description, secret: &str) -> String {
        let values: serde_json::Map<String, serde_json::Value> = description
            .fields
            .iter()
            .map(|field| {
                let value = match field.kind.as_str() {
                    "port" => "21118",
                    "secret" => secret,
                    _ => "192.0.2.7",
                };
                (field.key.clone(), serde_json::Value::String(value.to_string()))
            })
            .collect();
        serde_json::Value::Object(values).to_string()
    }

    #[test]
    fn add_list_update_remove() {
        let path = temp_config("store");
        let connectors = builtin();
        let description = connectors.first().expect("a built-in connector").clone();
        assert_eq!(sources_at(&path, &connectors), "[]");
        assert!(!path.exists(), "listing makes no file");

        let id = add_at(&path, &connectors, "Office PC", &description.id, &values_for(&description, "first-secret")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let listed: serde_json::Value = serde_json::from_str(&sources_at(&path, &connectors)).unwrap();
        assert_eq!(listed[0]["id"], id.as_str());
        assert_eq!(listed[0]["name"], "Office PC");
        assert_eq!(listed[0]["connector"], description.id.as_str());
        assert_eq!(listed[0]["connector_name"], description.name.as_str());
        // A listing never has a secret.
        assert!(!sources_at(&path, &connectors).contains("first-secret"));
        for field in &description.fields {
            assert_eq!(listed[0]["values"].get(&field.key).is_some(), field.kind != "secret", "{}", field.key);
        }

        // Empty fields keep what is stored, the secret included.
        assert_eq!(update_at(&path, &connectors, &id, "Office", "{}", None).unwrap(), id);
        let stored = std::fs::read_to_string(&path).unwrap();
        assert!(stored.contains("first-secret"));
        let listed: serde_json::Value = serde_json::from_str(&sources_at(&path, &connectors)).unwrap();
        assert_eq!(listed[0]["name"], "Office");

        assert!(has_source(&path, &id));
        remove_at(&path, &id).unwrap();
        assert!(!has_source(&path, &id));
        assert_eq!(sources_at(&path, &connectors), "[]");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// A computer that is shut down shows offline: the app checks its own
    /// computers as an agent checks its sources (here on this computer only).
    #[test]
    fn a_computer_that_does_not_answer_is_offline() {
        let path = temp_config("online");
        let connectors = builtin();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let open = listener.local_addr().unwrap().port();
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let values = |port: u16| serde_json::json!({ "host": "127.0.0.1", "port": port.to_string(), "password": "pw" }).to_string();
        let on = add_at(&path, &connectors, "On", "rustdesk", &values(open)).unwrap();
        let off = add_at(&path, &connectors, "Off", "rustdesk", &values(closed)).unwrap();
        let loaded = path.clone();
        let described = connectors.clone();
        let monitor = connector::reachability::Monitor::start_with(
            Box::new(move || (load_sources(&loaded), described.clone())),
            Box::new(|| {}),
            Duration::from_millis(100),
            Duration::from_millis(500),
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while online_at(&path, &connectors, &off, &monitor) < 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(online_at(&path, &connectors, &on, &monitor), 1);
        assert_eq!(online_at(&path, &connectors, &off, &monitor), 0);
        assert_eq!(online_at(&path, &connectors, "no-such-computer", &monitor), -1);
        drop(listener);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// The app's own computers are grouped as an agent's are: by address
    /// when nothing is said, and as the user interface says.
    #[test]
    fn computers_at_one_address_are_one_computer() {
        let path = temp_config("groups");
        let connectors = builtin();
        let (first, second) = (connectors[0].clone(), connectors.get(1).cloned().unwrap_or_else(|| connectors[0].clone()));
        let add = |name: &str, description: &Description, group: Option<&str>| {
            let mut config = load_or_create(&path).unwrap();
            let id = sources::add_in_group(&mut config, &connectors, name, &description.id, &values_for(description, "secret"), group).unwrap();
            save(&config, &path).unwrap();
            id
        };
        let listed = || serde_json::from_str::<serde_json::Value>(&sources_at(&path, &connectors)).unwrap();
        let a = add("Den PC", &first, None);
        assert!(listed()[0].get("group").is_none(), "one alone has no group");
        let b = add("Den PC (2)", &second, None);
        let all = listed();
        let group = all[0]["group"].as_str().expect("a group").to_string();
        assert_eq!(all[1]["group"], group.as_str());
        // The group is no secret and no address.
        assert!(!group.contains("192.0.2.7"));

        // Said by the user interface: kept ("*"), left (""), set.
        assert_eq!(group_said(GROUP_UNSAID), None);
        assert_eq!((group_said(""), group_said("g-1")), (Some(""), Some("g-1")));
        assert!(!common_group_ok(GROUP_UNSAID));
        update_at(&path, &connectors, &b, "Den PC (2)", "{}", group_said(GROUP_UNSAID)).unwrap();
        assert_eq!(listed()[1]["group"], group.as_str());
        update_at(&path, &connectors, &b, "Den PC (2)", "{}", group_said("")).unwrap();
        assert!(listed()[1].get("group").is_none());
        assert_eq!(listed()[0]["group"], group.as_str(), "the other stays");
        update_at(&path, &connectors, &b, "Den PC (2)", "{}", group_said("peer-0a1b2c3d")).unwrap();
        assert_eq!(listed()[1]["group"], "peer-0a1b2c3d");
        let refused = update_at(&path, &connectors, &a, "Den PC", "{}", Some("not a group")).unwrap_err();
        assert_eq!(refused, sources::GROUP_NOT_VALID);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    fn common_group_ok(group: &str) -> bool {
        // As the agent checks it: through an edit that would set it.
        let mut config = AgentConfig::default();
        let connectors = builtin();
        let id = sources::add(&mut config, &connectors, "X", &connectors[0].id, &values_for(&connectors[0], "s")).unwrap();
        sources::update_in_group(&mut config, &connectors, &id, "X", "{}", Some(group)).is_ok()
    }

    #[test]
    fn errors_are_reported_as_json() {
        let path = temp_config("errors");
        let connectors = builtin();
        let description = connectors.first().unwrap().clone();
        let refused: serde_json::Value =
            serde_json::from_str(&id_or_error(add_at(&path, &connectors, "", &description.id, "{}"))).unwrap();
        assert!(refused["error"].as_str().is_some_and(|e| !e.is_empty()));
        let unknown: serde_json::Value =
            serde_json::from_str(&id_or_error(add_at(&path, &connectors, "PC", "no-such-connector", "{}"))).unwrap();
        assert!(unknown["error"].as_str().is_some());
        let gone: serde_json::Value =
            serde_json::from_str(&id_or_error(update_at(&path, &connectors, "nobody", "PC", "{}", None))).unwrap();
        assert!(gone["error"].as_str().is_some());
        // Removing what is not there is not an error.
        assert!(remove_at(&path, "nobody").is_ok());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// A connectors folder with one external connector whose manifest says
    /// it has a set-up, and whose program is this shell script.
    #[cfg(unix)]
    fn install_pairing(dir: &Path, script: &str) {
        use std::os::unix::fs::PermissionsExt;
        let folder = dir.join("pairing");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join("connector.json"),
            r#"{"id":"pairing","name":"Pairing","command":"run.sh","fields":[{"key":"host","label":"Address","kind":"text","required":true}],"capabilities":{"video":["vp8"],"setup":true}}"#,
        )
        .unwrap();
        std::fs::write(folder.join("run.sh"), script).unwrap();
        std::fs::set_permissions(folder.join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// One message of the connectors' protocol, written by a shell script:
    /// both lengths (u32, little endian), then the JSON header.
    #[cfg(unix)]
    fn say(header: &str) -> String {
        let octal = |n: usize| (n as u32).to_le_bytes().iter().map(|b| format!("\\{:03o}", b)).collect::<String>();
        format!("printf '{}{}'; printf '%s' '{}'\n", octal(header.len()), octal(0), header)
    }

    /// The app's own computers with a type that has a set-up: the step, the
    /// state while it waits, the saved computer, and cancelling.
    #[cfg(unix)]
    #[test]
    fn a_computer_whose_type_has_a_set_up() {
        let path = temp_config("setup");
        let dir = path.parent().unwrap().join("connectors");
        // Says hello, shows a PIN, and is "paired" when a file appears.
        let script = format!(
            "#!/bin/sh\n{}{}while [ ! -e paired ]; do sleep 0.05; done\n{}",
            say(r#"{"type":"hello","version":1}"#),
            say(r#"{"type":"setup_step","message":"Enter this PIN at the other computer.","code":"2468"}"#),
            say(r#"{"type":"setup_done","values":{"client_key":"KEY-FROM-PAIRING"}}"#),
        );
        install_pairing(&dir, &script);
        let json = |text: String| serde_json::from_str::<serde_json::Value>(&text).unwrap();

        // What is wrong with the form is said at once: no set-up is started for it.
        let refused = json(begin_add_at(&path, &dir, "Den PC", "pairing", "{}", None));
        assert_eq!(refused["error"], "Address is required");

        let begun = json(begin_add_at(&path, &dir, "Den PC", "pairing", r#"{"host": "192.0.2.7"}"#, None));
        assert_eq!((begun["message"].as_str(), begun["code"].as_str()), (Some("Enter this PIN at the other computer."), Some("2468")), "{}", begun);
        let setup = begun["setup"].as_str().expect("a set-up").to_string();
        assert!(begun.get("id").is_none() && begun.get("error").is_none());
        let waiting = json(source_setup(&setup));
        assert_eq!((waiting["state"].as_str(), waiting["code"].as_str()), (Some("pending"), Some("2468")));
        let connectors = connector::available(&dir);
        assert_eq!(sources_at(&path, &connectors), "[]", "nothing is added while it waits");

        // The "other computer" accepts.
        std::fs::write(dir.join("pairing").join("paired"), b"").unwrap();
        let done = (0..200)
            .find_map(|_| {
                std::thread::sleep(Duration::from_millis(25));
                let state = json(source_setup(&setup));
                (state["state"] != "pending").then_some(state)
            })
            .expect("the set-up ends");
        assert_eq!(done["state"], "done");
        let id = done["id"].as_str().unwrap().to_string();
        assert!(has_source(&path, &id));
        // What the pairing returned is in the app's own file (the owner's alone), and in no listing.
        assert!(std::fs::read_to_string(&path).unwrap().contains("KEY-FROM-PAIRING"));
        let listed = sources_at(&path, &connectors);
        assert!(listed.contains("192.0.2.7") && !listed.contains("KEY-FROM-PAIRING") && !listed.contains("client_key"), "{}", listed);
        // Editing keeps it.
        update_at(&path, &connectors, &id, "Den", r#"{"host": "192.0.2.8"}"#, None).unwrap();
        let stored = std::fs::read_to_string(&path).unwrap();
        assert!(stored.contains("KEY-FROM-PAIRING") && stored.contains("192.0.2.8"));
        std::fs::remove_file(dir.join("pairing").join("paired")).unwrap();

        // The window is closed while it waits: stopped, forgotten, nothing added.
        let begun = json(begin_add_at(&path, &dir, "Other", "pairing", r#"{"host": "192.0.2.9"}"#, None));
        let setup = begun["setup"].as_str().expect("a set-up").to_string();
        cancel_setup(&setup);
        let gone = json(source_setup(&setup));
        assert_eq!(gone["state"], "failed");
        assert!(gone["error"].as_str().is_some_and(|e| !e.is_empty()));
        std::fs::write(dir.join("pairing").join("paired"), b"").unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(serde_json::from_str::<serde_json::Value>(&sources_at(&path, &connectors)).unwrap().as_array().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_password_per_run() {
        let first = new_password();
        let second = new_password();
        assert!(first.chars().count() >= 24, "the helper refuses a shorter one");
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(first, second);
    }

    #[test]
    fn a_bridge_session_names_a_computer_that_exists() {
        let path = temp_config("session");
        let connectors = builtin();
        let description = connectors.first().unwrap().clone();
        let id = add_at(&path, &connectors, "PC", &description.id, &values_for(&description, "secret")).unwrap();

        assert!(is_bridge("bridge:"));
        assert!(!is_bridge("ws://127.0.0.1:8091"));
        // No source: this would be the helper's own screen, which it never shows.
        assert!(check_session(&path, "direct", None).is_err());
        assert!(check_session(&path, "direct", Some("")).is_err());
        assert!(check_session(&path, "direct", Some("removed")).is_err());
        assert!(check_session(&path, "some-host-id", Some(&id)).is_err());
        assert!(check_session(&path, "direct", Some(&id)).is_ok());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// A stand-in for the helper (a shell script): it gets the password in
    /// its environment only, its port line is read, and it lives until its
    /// standard input closes.
    #[cfg(unix)]
    #[test]
    fn the_helper_gets_a_password_and_says_its_port() {
        let script = format!(
            "test ${{#{env}}} -ge 24 || exit 3; echo starting; echo '{line}40123'; cat >/dev/null",
            env = agent::bridge_agent::PASSWORD_ENV,
            line = agent::bridge_agent::PORT_LINE,
        );
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg(&script).stderr(Stdio::null());
        let mut helper = spawn_helper(cmd).expect("the stand-in starts");
        assert_eq!(helper.port, 40123);
        assert!(helper.password.chars().count() >= 24);
        assert!(matches!(helper.child.try_wait(), Ok(None)), "it stays while its input is held");
        // The app is gone (its end of the pipe closes): so is the helper.
        helper._stdin = None;
        let ended = (0..100).any(|_| {
            std::thread::sleep(Duration::from_millis(50));
            !matches!(helper.child.try_wait(), Ok(None))
        });
        assert!(ended, "it ends when its input closes");
    }

    /// A helper that never says its port is given up on.
    #[cfg(unix)]
    #[test]
    fn a_helper_that_ends_at_once_is_an_error() {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("exit 1").stderr(Stdio::null());
        assert!(spawn_helper(cmd).is_err());
    }

    #[test]
    fn the_displays_of_a_source() {
        let message = br#"{"type":"source_displays","current":"1","displays":[{"id":"0","name":"Display 1","width":1920,"height":1080},{"id":"1","name":"DP-2","width":2560,"height":1440}]}"#;
        let (displays, current) = source_displays(message).unwrap();
        assert_eq!(current, "1");
        let displays: serde_json::Value = serde_json::from_str(&displays).unwrap();
        assert_eq!(displays.as_array().unwrap().len(), 2);
        assert_eq!(displays[1]["name"], "DP-2");
        assert_eq!(displays[1]["width"], 2560);
        assert!(source_displays(br#"{"type":"host_cursor","x":1,"y":2}"#).is_none());
        assert!(source_displays(b"not json source_displays").is_none());
    }

    #[test]
    fn the_delay_to_a_source_is_added_to_the_ping() {
        assert_eq!(source_latency_ms(br#"{"type":"source_latency","ms":12}"#), Some(12.0));
        assert_eq!(source_latency_ms(br#"{"type":"source_latency","ms":0.4}"#), Some(0.4));
        assert_eq!(source_latency_ms(br#"{"ms":0,"type":"source_latency"}"#), Some(0.0));
        // Other messages of the channel, and what is not a count of milliseconds.
        assert_eq!(source_latency_ms(br#"{"type":"host_cursor","x":1,"y":2}"#), None);
        assert_eq!(source_latency_ms(br#"{"type":"source_displays","displays":[],"current":"0"}"#), None);
        assert_eq!(source_latency_ms(br#"{"type":"source_latency"}"#), None);
        assert_eq!(source_latency_ms(br#"{"type":"source_latency","ms":-3}"#), None);
        assert_eq!(source_latency_ms(br#"{"type":"source_latency","ms":"12"}"#), None);
        assert_eq!(source_latency_ms(b"source_latency not json"), None);
        // And it is not taken for the list of displays.
        assert!(source_displays(br#"{"type":"source_latency","ms":12}"#).is_none());

        assert_eq!(shown_ping_ms(0.1, 12.0), 12.1);
        assert_eq!(shown_ping_ms(0.25, 0.5), 0.75);
        // A session of the agent's own screen: its ping as before.
        assert_eq!(shown_ping_ms(7.5, 0.0), 7.5);
    }
}
