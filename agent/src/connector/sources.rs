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

/// Adds a source to `config`. `values_json` is `{"<field key>": "<value>"}`
/// from the form; only the connector's own fields are kept. The error is a
/// text for the person and names fields, never what was typed in them.
pub fn add(
    config: &mut AgentConfig,
    connectors: &[Description],
    name: &str,
    connector: &str,
    values_json: &str,
) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("Give the computer a name".into());
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(format!("The name can have {} characters at most", MAX_NAME_CHARS));
    }
    let description = connectors
        .iter()
        .find(|c| c.id == connector)
        .ok_or_else(|| "That connector is not installed".to_string())?;
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
            if field.required {
                return Err(format!("{} is required", field.label));
            }
            continue;
        }
        if field.kind == "port" && !value.parse::<u16>().is_ok_and(|port| port != 0) {
            return Err(format!("{} must be a port from 1 to 65535", field.label));
        }
        values.insert(field.key.clone(), value.to_string());
    }

    let id = new_id(name, &config.sources);
    config.sources.push(SourceConfig {
        id,
        name: name.to_string(),
        target: Target { connector: description.id.clone(), values },
    });
    Ok(())
}

/// Removes the source `id`; whether there was one.
pub fn remove(config: &mut AgentConfig, id: &str) -> bool {
    let before = config.sources.len();
    config.sources.retain(|source| source.id != id);
    config.sources.len() != before
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

        let id = config.sources[0].id.clone();
        assert!(remove(&mut config, &id));
        assert!(!remove(&mut config, &id));
        assert_eq!(config.sources.len(), 1);
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
