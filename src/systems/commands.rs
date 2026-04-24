use valence::prelude::*;
use valence_command::CommandExecutionEvent;

#[derive(Component, Default)]
pub struct OpLevel(pub u8);

pub fn in_game_commands(
    mut commands_reader: EventReader<CommandExecutionEvent>,
    mut clients: Query<(&mut Client, &mut GameMode)>,
) {
    for ev in commands_reader.read() {
        if let Ok((mut client, mut gamemode)) = clients.get_mut(ev.executor) {
            let cmd_full = &ev.command;
            let mut parts = cmd_full.split_whitespace();
            let cmd_name = parts.next().unwrap_or("");
            
            // Temporary op bypass to just make it work
            let level = 4;

            match cmd_name {
                "help" => {
                    let msg = "Kraken Commands: /help, /gm, /say";
                    client.send_chat_message(msg.italic());
                }
                "gm" | "gamemode" => {
                    if level >= 2 {
                        let mode = parts.next().unwrap_or("creative");
                        *gamemode = match mode {
                            "0" | "survival" | "s" => GameMode::Survival,
                            "1" | "creative" | "c" => GameMode::Creative,
                            "2" | "adventure" | "a" => GameMode::Adventure,
                            "3" | "spectator" | "sp" => GameMode::Spectator,
                            _ => GameMode::Creative,
                        };
                        client.send_chat_message(format!("Gamemode set to {:?}", *gamemode).into_text());
                    } else {
                        client.send_chat_message("You do not have permission (Level 2 required).".color(Color::RED));
                    }
                }
                "say" => {
                    let msg = parts.collect::<Vec<_>>().join(" ");
                    client.send_chat_message(format!("[Server] {}", msg).color(Color::GOLD));
                }
                "op" => {
                    // For testing, self-op
                    client.send_chat_message("You are now OP!".color(Color::GREEN));
                }
                _ => {
                    client.send_chat_message(format!("Unknown command: /{}", cmd_name).color(Color::RED));
                }
            }
        }
    }
}
