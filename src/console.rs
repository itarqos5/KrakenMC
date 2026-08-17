use std::io::{self, BufRead};

use crate::logger::{log_error, log_info, log_warn};
use crate::operator_store::{remove_operator, set_operator};
use crate::viakraken::java::backend::state::{
    console_command_channel, online_players, register_summoned_entity, ConsoleCommand,
    OnlinePlayer, NEXT_ENTITY_ID,
};

pub fn spawn_console() {
    std::thread::Builder::new()
        .name("kraken-console".to_string())
        .spawn(|| {
            print_help();
            for line in io::stdin().lock().lines() {
                match line {
                    Ok(line) => execute(line.trim()),
                    Err(error) => {
                        log_error!("Console input error: {}", error);
                        break;
                    }
                }
            }
        })
        .expect("failed to start console input thread");
}

fn execute(input: &str) {
    let input = input.strip_prefix('/').unwrap_or(input).trim();
    if input.is_empty() {
        return;
    }

    let parts: Vec<_> = input.split_whitespace().collect();
    match parts[0].to_ascii_lowercase().as_str() {
        "help" => print_help(),
        "list" => list_players(),
        "op" if (2..=3).contains(&parts.len()) => {
            let level = parts
                .get(2)
                .and_then(|level| level.parse::<u8>().ok())
                .unwrap_or(4)
                .clamp(1, 4);
            let Some(player) = find_online_player(parts[1]) else {
                log_warn!("Player '{}' must be online to be opped.", parts[1]);
                return;
            };
            match set_operator(player.uuid, &player.username, level) {
                Ok(()) => {
                    let _ = console_command_channel().send(ConsoleCommand::OperatorLevel {
                        uuid: player.uuid,
                        level,
                    });
                    log_info!("Made {} an operator at level {}.", player.username, level);
                }
                Err(error) => log_error!("Could not op {}: {}", player.username, error),
            }
        }
        "deop" if parts.len() == 2 => {
            let Some(player) = find_online_player(parts[1]) else {
                log_warn!("Player '{}' must be online to be deopped.", parts[1]);
                return;
            };
            match remove_operator(player.uuid, &player.username) {
                Ok(true) => {
                    let _ = console_command_channel().send(ConsoleCommand::OperatorLevel {
                        uuid: player.uuid,
                        level: 0,
                    });
                    log_info!("Removed operator status from {}.", player.username);
                }
                Ok(false) => log_warn!("{} is not an operator.", player.username),
                Err(error) => log_error!("Could not deop {}: {}", player.username, error),
            }
        }
        "kill" if parts.len() == 2 => {
            send_to_player(parts[1], |uuid| ConsoleCommand::Kill { uuid })
        }
        "gamemode" if parts.len() == 3 => {
            let Some(gamemode) = parse_gamemode(parts[1]) else {
                log_warn!("Unknown game mode '{}'.", parts[1]);
                return;
            };
            send_to_player(parts[2], |uuid| ConsoleCommand::Gamemode { uuid, gamemode });
        }
        "summon" if parts.len() == 2 || parts.len() == 5 => {
            let entity_name = parts[1].strip_prefix("minecraft:").unwrap_or(parts[1]);
            let Some(entity_type) = pumpkin_data::entity::EntityType::from_name(entity_name) else {
                log_warn!("Unknown entity type '{}'.", parts[1]);
                return;
            };
            if !entity_type.summonable || entity_type == &pumpkin_data::entity::EntityType::PLAYER {
                log_warn!("Entity '{}' cannot be summoned.", entity_name);
                return;
            }
            let coordinates = if parts.len() == 5 {
                let parsed = parts[2..5]
                    .iter()
                    .map(|value| value.parse::<f64>())
                    .collect::<Result<Vec<_>, _>>();
                let Ok(values) = parsed else {
                    log_warn!("Summon coordinates must be numbers.");
                    return;
                };
                (values[0], values[1], values[2])
            } else {
                (0.0, 0.0, 0.0)
            };
            let entity_id = NEXT_ENTITY_ID.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            register_summoned_entity(
                entity_id,
                entity_type.id,
                coordinates.0,
                coordinates.1,
                coordinates.2,
            );
            let _ = console_command_channel().send(ConsoleCommand::Summon {
                entity_id,
                entity_type: entity_type.id,
                x: coordinates.0,
                y: coordinates.1,
                z: coordinates.2,
            });
            log_info!(
                "Summoned {} at {:.1}, {:.1}, {:.1}.",
                entity_name,
                coordinates.0,
                coordinates.1,
                coordinates.2
            );
        }
        _ => log_warn!("Unknown or incomplete command. Type /help for usage."),
    }
}

fn send_to_player(name: &str, command: impl FnOnce(uuid::Uuid) -> ConsoleCommand) {
    let Some(player) = find_online_player(name) else {
        log_warn!("Player '{}' is not online.", name);
        return;
    };
    if console_command_channel()
        .send(command(player.uuid))
        .is_err()
    {
        log_warn!("No active player session accepted the command.");
    }
}

fn find_online_player(name: &str) -> Option<OnlinePlayer> {
    online_players()
        .lock()
        .ok()?
        .values()
        .find(|player| player.username.eq_ignore_ascii_case(name))
        .cloned()
}

fn parse_gamemode(value: &str) -> Option<u8> {
    match value.to_ascii_lowercase().as_str() {
        "survival" | "0" => Some(0),
        "creative" | "1" => Some(1),
        "adventure" | "2" => Some(2),
        "spectator" | "3" => Some(3),
        _ => None,
    }
}

fn list_players() {
    let Ok(players) = online_players().lock() else {
        log_error!("Could not read the online player list.");
        return;
    };
    if players.is_empty() {
        log_info!("No players are online.");
        return;
    }
    let mut names: Vec<_> = players
        .values()
        .map(|player| player.username.as_str())
        .collect();
    names.sort_unstable();
    log_info!("Online ({}): {}", names.len(), names.join(", "));
}

fn print_help() {
    log_info!("Console commands:");
    log_info!("  /help                         Show this command list");
    log_info!("  /list                         List online players");
    log_info!("  /op <player> [level]          Grant operator level 1-4");
    log_info!("  /deop <player>                Remove operator status");
    log_info!("  /kill <player>                Kill an online player");
    log_info!("  /gamemode <mode> <player>     Set survival/creative/adventure/spectator");
    log_info!("  /summon <entity> [x y z]      Summon an entity (default: 0 0 0)");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_named_and_numeric_gamemodes() {
        assert_eq!(parse_gamemode("creative"), Some(1));
        assert_eq!(parse_gamemode("3"), Some(3));
        assert_eq!(parse_gamemode("invalid"), None);
    }

    #[test]
    fn entity_registry_resolves_summon_names() {
        let zombie = pumpkin_data::entity::EntityType::from_name("zombie").unwrap();
        assert!(zombie.summonable);
        assert_eq!(zombie.resource_name, "zombie");
    }
}
