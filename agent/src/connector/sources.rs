//! The sources of this computer's agent, for the settings: other computers
//! it shows besides its own screen, each through a connector
//! (agent/src/connector). Listing, adding and removing them in the agent's
//! config; the callers load and save it.
//!
//! What a source's fields hold (address, password, ...) never goes back to
//! the window and is never logged: a listing has the names only.
//!
//! Used by both places the agent is configured (the Lunaris app's settings
//! and the stand-alone agent's window).

use std::collections::BTreeMap;

use crate::connector::{Description, SourceConfig, Target};
use crate::pairing::AgentConfig;
use serde::Serialize;

/// Keys of a source in the config that are not a connector's field.
const RESERVED_KEYS: [&str; 3] = ["id", "name", "connector"];
const MAX_NAME_CHARS: usize = 64;

#[derive(Serialize)]
struct FieldView<'a> {
    key: &'a str,
    label: &'a str,
    kind: &'a str,
    default: &'a str,
    required: bool,
}

#[derive(Serialize)]
struct ConnectorView<'a> {
    id: &'a str,
    name: &'a str,
    fields: Vec<FieldView<'a>>,
}

#[derive(Serialize)]
struct SourceView<'a> {
    id: &'a str,
    name: &'a str,
    connector: &'a str,
    /// The connector's name, or its id when it is not installed (any more).
    connector_name: &'a str,
}

#[derive(Serialize)]
struct Listing<'a> {
    connectors: Vec<ConnectorView<'a>>,
    sources: Vec<SourceView<'a>>,
}

/// The connectors to choose from and the sources there are, as JSON for
/// the settings. Of a source only its id, its name and its connector.
pub fn listing_json(sources: &[SourceConfig], connectors: &[Description]) -> String {
    let listing = Listing {
        connectors: connectors
            .iter()
            .map(|c| ConnectorView {
                id: &c.id,
                name: &c.name,
                fields: c
                    .fields
                    .iter()
                    .map(|f| FieldView { key: &f.key, label: &f.label, kind: &f.kind, default: &f.default, required: f.required })
                    .collect(),
            })
            .collect(),
        sources: sources
            .iter()
            .map(|s| SourceView {
                id: &s.id,
                name: &s.name,
                connector: &s.target.connector,
                connector_name: connectors
                    .iter()
                    .find(|c| c.id == s.target.connector)
                    .map(|c| c.name.as_str())
                    .unwrap_or(&s.target.connector),
            })
            .collect(),
    };
    serde_json::to_string(&listing).unwrap_or_else(|_| "{\"connectors\":[],\"sources\":[]}".to_string())
}

/// The fields of a form, checked: only the connector's own fields are
/// kept. `stored` is what a source being edited has: a field left empty
/// keeps its stored value. The error is a text for the person and names
/// fields, never what was typed in them.
fn read_values(description: &Description, values_json: &str, stored: Option<&BTreeMap<String, String>>) -> Result<BTreeMap<String, String>, String> {
    let typed: BTreeMap<String, String> =
        serde_json::from_str(values_json).map_err(|_| "The form could not be read".to_string())?;
    let mut values = BTreeMap::new();
    for field in &description.fields {
        if RESERVED_KEYS.contains(&field.key.as_str()) {
            continue;
        }
        let value = typed.get(&field.key).map(String::as_str).unwrap_or("");
        // A password is kept as typed; anything else without the spaces around it.
        let value = if field.kind == "secret" { value } else { value.trim() };
        if value.is_empty() {
            if let Some(kept) = stored.and_then(|stored| stored.get(&field.key)) {
                values.insert(field.key.clone(), kept.clone());
            } else if field.required {
                return Err(format!("{} is required", field.label));
            }
            continue;
        }
        if field.kind == "port" && !value.parse::<u16>().is_ok_and(|port| port != 0) {
            return Err(format!("{} must be a port from 1 to 65535", field.label));
        }
        values.insert(field.key.clone(), value.to_string());
    }
    Ok(values)
}

fn checked_name(name: &str) -> Result<&str, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("Give the computer a name".into());
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(format!("The name can have {} characters at most", MAX_NAME_CHARS));
    }
    Ok(name)
}

/// Adds a source to `config` and returns its new id. `values_json` is
/// `{"<field key>": "<value>"}` from the form.
pub fn add(
    config: &mut AgentConfig,
    connectors: &[Description],
    name: &str,
    connector: &str,
    values_json: &str,
) -> Result<String, String> {
    let name = checked_name(name)?;
    let description = connectors
        .iter()
        .find(|c| c.id == connector)
        .ok_or_else(|| "That connector is not installed".to_string())?;
    let values = read_values(description, values_json, None)?;
    let id = new_id(name, &config.sources);
    config.sources.push(SourceConfig {
        id: id.clone(),
        name: name.to_string(),
        target: Target { connector: description.id.clone(), values },
    });
    Ok(id)
}

/// Changes the source `id`: its name and its fields. A field left empty in
/// `values_json` keeps its value (so a password need not be typed again);
/// its id and its connector stay.
pub fn update(
    config: &mut AgentConfig,
    connectors: &[Description],
    id: &str,
    name: &str,
    values_json: &str,
) -> Result<(), String> {
    let name = checked_name(name)?;
    let source = config.sources.iter_mut().find(|source| source.id == id).ok_or_else(|| "That computer is not there any more".to_string())?;
    let description = connectors
        .iter()
        .find(|c| c.id == source.target.connector)
        .ok_or_else(|| "That connector is not installed".to_string())?;
    source.target.values = read_values(description, values_json, Some(&source.target.values))?;
    source.name = name.to_string();
    Ok(())
}

/// Removes the source `id`; whether there was one.
pub fn remove(config: &mut AgentConfig, id: &str) -> bool {
    let before = config.sources.len();
    config.sources.retain(|source| source.id != id);
    config.sources.len() != before
}

/// The connectors to choose from, as JSON: `[{id, name, fields}]`.
pub fn connectors_json(connectors: &[Description]) -> String {
    serde_json::to_string(&connector_infos(connectors)).unwrap_or_else(|_| "[]".to_string())
}

/// The sources as JSON for a list and an edit form:
/// `[{id, name, connector, connector_name, values}]`, where `values` has the
/// fields that are not secrets (a password never leaves the config).
pub fn views_json(sources: &[SourceConfig], connectors: &[Description]) -> String {
    let views: Vec<serde_json::Value> = sources
        .iter()
        .map(|source| {
            let description = connectors.iter().find(|c| c.id == source.target.connector);
            let values: BTreeMap<&str, &str> = description
                .map(|d| {
                    d.fields
                        .iter()
                        .filter(|field| field.kind != "secret")
                        .filter_map(|field| source.target.values.get(&field.key).map(|value| (field.key.as_str(), value.as_str())))
                        .collect()
                })
                .unwrap_or_default();
            serde_json::json!({
                "id": source.id,
                "name": source.name,
                "connector": source.target.connector,
                "connector_name": description.map(|d| d.name.as_str()).unwrap_or(&source.target.connector),
                "values": values,
            })
        })
        .collect();
    serde_json::to_string(&views).unwrap_or_else(|_| "[]".to_string())
}

/// Why a computer's sources cannot be changed from the server.
pub const NOT_MANAGEABLE: &str = "That computer asks at its screen before each connection, so computers cannot be added to it from here. Add it in the Lunaris app on that computer.";

/// Does what a server asked with this agent's sources, in its config file,
/// and returns the id of the source it was about.
pub fn apply(change: &common::SourceChange) -> Result<String, String> {
    let connectors = crate::connector::available(&crate::connector::connectors_dir());
    apply_at(crate::pairing::config_path(), &connectors, change)
}

/// `apply`, on the config file at `path`.
pub fn apply_at(path: &str, connectors: &[Description], change: &common::SourceChange) -> Result<String, String> {
    let mut config = crate::pairing::load_config(path).map_err(|_| "That computer's settings could not be read".to_string())?;
    let json = |values: &BTreeMap<String, String>| serde_json::to_string(values).unwrap_or_else(|_| "{}".to_string());
    let id = match change {
        common::SourceChange::Add { name, connector, values } => add(&mut config, connectors, name, connector, &json(values))?,
        common::SourceChange::Update { id, name, values } => {
            update(&mut config, connectors, id, name, &json(values))?;
            id.clone()
        }
        common::SourceChange::Remove { id } => {
            if !remove(&mut config, id) {
                return Err("That computer is not there any more".into());
            }
            id.clone()
        }
    };
    crate::pairing::save_config(&config, path).map_err(|_| "That computer's settings could not be saved".to_string())?;
    Ok(id)
}

/// What a server is told when the agent connects and when its sources
/// change: the sources (id, name, connector) and the connectors there are.
pub fn announcement(manageable: bool) -> common::AgentMessage {
    common::AgentMessage::Sources {
        sources: source_infos(&crate::connector::configured_sources()),
        connectors: connector_infos(&crate::connector::available(&crate::connector::connectors_dir())),
        manageable,
    }
}

/// The connectors as the server and the viewers are told them.
pub fn connector_infos(connectors: &[Description]) -> Vec<common::ConnectorInfoMsg> {
    connectors
        .iter()
        .map(|c| common::ConnectorInfoMsg {
            id: c.id.clone(),
            name: c.name.clone(),
            fields: c
                .fields
                .iter()
                .map(|f| common::ConnectorFieldMsg {
                    key: f.key.clone(),
                    label: f.label.clone(),
                    kind: f.kind.clone(),
                    default: f.default.clone(),
                    required: f.required,
                })
                .collect(),
        })
        .collect()
}

/// The sources as the server and the viewers are told them: id, name and
/// connector, nothing of their fields.
pub fn source_infos(sources: &[SourceConfig]) -> Vec<common::SourceInfoMsg> {
    sources
        .iter()
        .map(|s| common::SourceInfoMsg { id: s.id.clone(), name: s.name.clone(), connector: s.target.connector.clone() })
        .collect()
}

/// Where the Lunaris app keeps the computers of other types it added for
/// itself: beside the agent's config, in a file of its own. The agent that
/// shares this computer never reads it, so they are not offered to the
/// people who view this computer.
pub fn bridge_config_path() -> std::path::PathBuf {
    std::path::Path::new(crate::pairing::config_path()).with_file_name("bridge_config.json")
}

/// The name as an id: lower case letters, digits and `-`.
fn slug(name: &str) -> String {
    let mut slug = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
        if slug.len() >= 24 {
            break;
        }
    }
    let slug = slug.trim_end_matches('-');
    if slug.is_empty() { "source".to_string() } else { slug.to_string() }
}

/// An id no other source has: the name's slug and a few random characters
/// (so that a source removed and added again under the same name is not
/// taken for the old one by a viewer that remembered it).
fn new_id(name: &str, existing: &[SourceConfig]) -> String {
    let slug = slug(name);
    loop {
        // The access password's generator: lower case letters and digits in groups.
        let suffix: String = crate::pairing::generate_direct_password()
            .chars()
            .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            .take(4)
            .collect();
        let id = format!("{}-{}", slug, suffix);
        if !existing.iter().any(|source| source.id == id) {
            return id;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector::{Capabilities, Field};

    fn connectors() -> Vec<Description> {
        let field = |key: &str, label: &str, kind: &str, default: &str, required: bool| Field {
            key: key.into(),
            label: label.into(),
            kind: kind.into(),
            default: default.into(),
            required,
        };
        vec![Description {
            id: "demo".into(),
            name: "Demo".into(),
            fields: vec![
                field("host", "Address", "text", "", true),
                field("port", "Port", "port", "21118", false),
                field("password", "Password", "secret", "", true),
            ],
            capabilities: Capabilities::default(),
        }]
    }

    #[test]
    fn adds_lists_and_removes_without_showing_values() {
        let mut config = AgentConfig::default();
        let values = r#"{"host": " 192.168.1.20 ", "port": "", "password": " s3cret ", "id": "x", "other": "y"}"#;
        add(&mut config, &connectors(), "  Living room PC ", "demo", values).unwrap();
        add(&mut config, &connectors(), "Living room PC", "demo", values).unwrap();

        let first = &config.sources[0];
        assert_eq!(first.name, "Living room PC");
        assert_eq!(first.target.connector, "demo");
        assert_eq!(first.target.get("host"), "192.168.1.20");
        // The password as typed; an empty port and keys the connector does not have are left out.
        assert_eq!(first.target.get("password"), " s3cret ");
        assert_eq!(first.target.values.len(), 2);

        // Ids: of the name, in the characters allowed, and no two alike.
        for source in &config.sources {
            assert!(source.id.starts_with("living-room-pc-"), "{}", source.id);
            assert!(source.id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'));
        }
        assert_ne!(config.sources[0].id, config.sources[1].id);

        let listing = listing_json(&config.sources, &connectors());
        assert!(listing.contains("\"connector_name\":\"Demo\""));
        assert!(listing.contains("\"label\":\"Password\""));
        assert!(!listing.contains("s3cret") && !listing.contains("192.168.1.20"));

        // For a list and an edit form: what is not a secret, and no more.
        let views = views_json(&config.sources, &connectors());
        assert!(views.contains("\"values\":{\"host\":\"192.168.1.20\"}"), "{}", views);
        assert!(!views.contains("s3cret"));
        assert!(connectors_json(&connectors()).starts_with("[{\"id\":\"demo\""));
        assert_eq!(source_infos(&config.sources)[0].name, "Living room PC");

        let id = config.sources[0].id.clone();
        assert!(remove(&mut config, &id));
        assert!(!remove(&mut config, &id));
        assert_eq!(config.sources.len(), 1);
    }

    #[test]
    fn a_servers_changes_are_made_in_the_config_file() {
        use common::SourceChange;
        let dir = std::env::temp_dir().join(format!("lunaris-sources-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent_config.json").to_string_lossy().into_owned();
        crate::pairing::save_config(&AgentConfig::default(), &path).unwrap();
        let all = connectors();
        let values = |pairs: &[(&str, &str)]| pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<BTreeMap<_, _>>();

        let add = SourceChange::Add { name: "Lab".into(), connector: "demo".into(), values: values(&[("host", "10.1.1.1"), ("password", "pw-one")]) };
        // What is printed of a change has the names of its fields, not what is in them.
        let printed = format!("{:?}", add);
        assert!(printed.contains("host") && !printed.contains("10.1.1.1") && !printed.contains("pw-one"), "{}", printed);
        let id = apply_at(&path, &all, &add).unwrap();
        let saved = crate::pairing::load_config(&path).unwrap();
        assert_eq!(saved.sources.len(), 1);
        assert_eq!((saved.sources[0].id.as_str(), saved.sources[0].target.get("password")), (id.as_str(), "pw-one"));
        // The file is the owner's alone.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }

        let update = SourceChange::Update { id: id.clone(), name: "Lab 2".into(), values: values(&[("host", "10.1.1.2")]) };
        assert_eq!(apply_at(&path, &all, &update).unwrap(), id);
        let saved = crate::pairing::load_config(&path).unwrap();
        assert_eq!((saved.sources[0].name.as_str(), saved.sources[0].target.get("host"), saved.sources[0].target.get("password")), ("Lab 2", "10.1.1.2", "pw-one"));

        // What is refused changes nothing.
        let bad = SourceChange::Add { name: "X".into(), connector: "nope".into(), values: values(&[]) };
        assert_eq!(apply_at(&path, &all, &bad).unwrap_err(), "That connector is not installed");
        assert_eq!(crate::pairing::load_config(&path).unwrap().sources.len(), 1);

        assert_eq!(apply_at(&path, &all, &SourceChange::Remove { id: id.clone() }).unwrap(), id);
        assert!(crate::pairing::load_config(&path).unwrap().sources.is_empty());
        assert_eq!(apply_at(&path, &all, &SourceChange::Remove { id }).unwrap_err(), "That computer is not there any more");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn editing_keeps_what_is_left_empty() {
        let mut config = AgentConfig::default();
        let all = connectors();
        let id = add(&mut config, &all, "Office", "demo", r#"{"host": "10.0.0.5", "port": "5000", "password": "first"}"#).unwrap();
        assert_eq!(config.sources[0].id, id);
        // A new name and address; the password and the port are not typed again.
        update(&mut config, &all, &id, " Office 2 ", r#"{"host": "10.0.0.6", "password": ""}"#).unwrap();
        let source = &config.sources[0];
        assert_eq!((source.id.as_str(), source.name.as_str()), (id.as_str(), "Office 2"));
        assert_eq!((source.target.get("host"), source.target.get("port"), source.target.get("password")), ("10.0.0.6", "5000", "first"));
        // A new password replaces the old one; a wrong port is refused and nothing changes.
        update(&mut config, &all, &id, "Office 2", r#"{"password": "second"}"#).unwrap();
        assert_eq!(config.sources[0].target.get("password"), "second");
        assert_eq!(update(&mut config, &all, &id, "Office 2", r#"{"port": "0"}"#).unwrap_err(), "Port must be a port from 1 to 65535");
        assert_eq!(config.sources[0].target.get("port"), "5000");
        assert_eq!(update(&mut config, &all, &id, " ", "{}").unwrap_err(), "Give the computer a name");
        assert_eq!(update(&mut config, &all, "nope", "X", "{}").unwrap_err(), "That computer is not there any more");
    }

    #[test]
    fn refuses_what_is_missing_or_wrong() {
        let mut config = AgentConfig::default();
        let all = connectors();
        let error = |name: &str, connector: &str, values: &str| {
            add(&mut AgentConfig::default(), &all, name, connector, values).unwrap_err()
        };
        assert_eq!(error(" ", "demo", "{}"), "Give the computer a name");
        assert_eq!(error("A", "nope", "{}"), "That connector is not installed");
        assert_eq!(error("A", "demo", r#"{"host": "h"}"#), "Password is required");
        assert_eq!(error("A", "demo", r#"{"host": "  ", "password": "p"}"#), "Address is required");
        let bad_port = error("A", "demo", r#"{"host": "h", "password": "hunter2", "port": "70000"}"#);
        assert_eq!(bad_port, "Port must be a port from 1 to 65535");
        assert!(config.sources.is_empty());
        add(&mut config, &all, "Ünïcødé!!", "demo", r#"{"host": "h", "password": "p", "port": "5900"}"#).unwrap();
        assert!(config.sources[0].id.starts_with("n-c-d-"), "{}", config.sources[0].id);
        add(&mut config, &all, "日本", "demo", r#"{"host": "h", "password": "p"}"#).unwrap();
        assert!(config.sources[1].id.starts_with("source-"), "{}", config.sources[1].id);
    }
}
