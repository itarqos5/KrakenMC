use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::logger::{log_error, log_info};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct OperatorEntry {
    uuid: Uuid,
    name: String,
    #[serde(default = "default_operator_level")]
    level: u8,
    #[serde(default)]
    bypasses_player_limit: bool,
}

fn default_operator_level() -> u8 {
    4
}

fn operators_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("ops.json")
}

fn load_entries() -> Result<Vec<OperatorEntry>, String> {
    let path = operators_path();
    if !path.exists() {
        fs::write(&path, "[]\n")
            .map_err(|error| format!("failed to create {}: {error}", path.display()))?;
    }
    let contents = fs::read_to_string(&path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    serde_json::from_str(&contents)
        .map_err(|error| format!("failed to parse {}: {error}", path.display()))
}

fn save_entries(entries: &[OperatorEntry]) -> Result<(), String> {
    let path = operators_path();
    let json = serde_json::to_string_pretty(entries)
        .map_err(|error| format!("failed to serialize operator list: {error}"))?;
    fs::write(&path, format!("{json}\n"))
        .map_err(|error| format!("failed to write {}: {error}", path.display()))
}

/// Returns whether a player appears in the standard Minecraft-style `ops.json` list.
/// UUID is authoritative; the name comparison keeps offline-mode lists convenient.
pub fn operator_level(uuid: Uuid, username: &str) -> u8 {
    let path = operators_path();
    if !path.exists() {
        if let Err(error) = fs::write(&path, "[]\n") {
            log_error!("Failed to create {}: {}", path.display(), error);
        } else {
            log_info!("Created empty operator list at {}", path.display());
        }
        return 0;
    }

    let entries = match load_entries() {
        Ok(entries) => entries,
        Err(error) => {
            log_error!("{}", error);
            return 0;
        }
    };

    entries
        .iter()
        .find(|entry| entry.uuid == uuid || entry.name.eq_ignore_ascii_case(username))
        .map(|entry| entry.level.clamp(1, 4))
        .unwrap_or(0)
}

pub fn set_operator(uuid: Uuid, username: &str, level: u8) -> Result<(), String> {
    let mut entries = load_entries()?;
    let level = level.clamp(1, 4);
    if let Some(entry) = entries
        .iter_mut()
        .find(|entry| entry.uuid == uuid || entry.name.eq_ignore_ascii_case(username))
    {
        entry.uuid = uuid;
        entry.name = username.to_string();
        entry.level = level;
    } else {
        entries.push(OperatorEntry {
            uuid,
            name: username.to_string(),
            level,
            bypasses_player_limit: false,
        });
    }
    save_entries(&entries)
}

pub fn remove_operator(uuid: Uuid, username: &str) -> Result<bool, String> {
    let mut entries = load_entries()?;
    let old_len = entries.len();
    entries.retain(|entry| entry.uuid != uuid && !entry.name.eq_ignore_ascii_case(username));
    if entries.len() == old_len {
        return Ok(false);
    }
    save_entries(&entries)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_standard_operator_entry() {
        let uuid = Uuid::parse_str("8667ba71-b85a-4004-af54-457a9734eed7").unwrap();
        let entries: Vec<OperatorEntry> = serde_json::from_str(&format!(
            r#"[{{"uuid":"{uuid}","name":"Player","level":4,"bypassesPlayerLimit":false}}]"#
        ))
        .unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].uuid, uuid);
        assert_eq!(entries[0].level, 4);
    }
}
