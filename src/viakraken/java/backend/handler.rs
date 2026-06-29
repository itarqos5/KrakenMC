use std::sync::Arc;
use bytes::Bytes;
use tokio::net::TcpStream;
use uuid::Uuid;

use pumpkin_protocol::codec::var_int::VarInt;
use pumpkin_protocol::java::client::play::{
    CAcknowledgeBlockChange, CBlockUpdate, CGameEvent, CPlayerAbilities, CPlayerInfoUpdate,
    CPlayerPosition, CRespawn, CSystemChatMessage, GameEvent, Player, PlayerAction, PlayerInfoFlags,
};
use pumpkin_protocol::java::server::play::SChatMessage;
use pumpkin_protocol::ServerPacket;
use pumpkin_util::math::position::BlockPos;
use pumpkin_util::math::vector3::Vector3;
use pumpkin_util::text::TextComponent;
use pumpkin_util::version::MinecraftVersion;

use crate::logger::log_info;
use crate::viakraken::java::packets::encode_java_packet;
use crate::viakraken::utils::{read_varint_from_slice, write_framed_payload};
use crate::world::chunk_gen::save_block_change;
use crate::world::player_store::PlayerData;

use super::state::{block_channel, chat_channel, gamemode_abilities, online_players, player_event_channel, PlayerEvent};

pub async fn handle_play_packet(
    stream: &mut TcpStream,
    version: MinecraftVersion,
    pkt_data: &[u8],
    player: &mut PlayerData,
    username: &str,
    uuid: Uuid,
    my_entity_id: i32,
    db: &Arc<sled::Db>,
) -> std::io::Result<()> {
    if pkt_data.is_empty() {
        return Ok(());
    }

    let mut offset = 0usize;
    let pid = read_varint_from_slice(pkt_data, &mut offset)?;
    let payload = &pkt_data[offset..];

    let sv_use_item_on = pumpkin_data::packet::serverbound::PLAY_USE_ITEM_ON.to_id(version);
    let sv_change_gm = pumpkin_data::packet::serverbound::PLAY_CHANGE_GAME_MODE.to_id(version);
    let sv_chat_cmd = pumpkin_data::packet::serverbound::PLAY_CHAT_COMMAND.to_id(version);
    let sv_pos_rot = pumpkin_data::packet::serverbound::PLAY_MOVE_PLAYER_POS_ROT.to_id(version);
    let sv_pos = pumpkin_data::packet::serverbound::PLAY_MOVE_PLAYER_POS.to_id(version);
    let sv_chat = pumpkin_data::packet::serverbound::PLAY_CHAT.to_id(version);
    let sv_creative_slot = pumpkin_data::packet::serverbound::PLAY_SET_CREATIVE_MODE_SLOT.to_id(version);
    let sv_held_item = pumpkin_data::packet::serverbound::PLAY_SET_CARRIED_ITEM.to_id(version);
    let sv_player_action = pumpkin_data::packet::serverbound::PLAY_PLAYER_ACTION.to_id(version);
    let sv_client_cmd = pumpkin_data::packet::serverbound::PLAY_CLIENT_COMMAND.to_id(version);
    let sv_interact = pumpkin_data::packet::serverbound::PLAY_INTERACT.to_id(version);

    let mut moved = false;

    if pid == sv_use_item_on {
        let mut o = 0usize;
        let _hand = read_varint_from_slice(payload, &mut o).unwrap_or(0);
        if o + 8 <= payload.len() {
            let packed = i64::from_be_bytes(payload[o..o+8].try_into().unwrap_or_default());
            o += 8;
            let face = read_varint_from_slice(payload, &mut o).unwrap_or(0);
            o += 14;
            let sequence = read_varint_from_slice(payload, &mut o).unwrap_or(0);

            let x = (packed >> 38) as i32;
            let y = ((packed << 52) >> 52) as i32;
            let z = ((packed << 26) >> 38) as i32;

            let (nx, ny, nz) = match face {
                0 => (x, y - 1, z),
                1 => (x, y + 1, z),
                2 => (x, y, z - 1),
                3 => (x, y, z + 1),
                4 => (x - 1, y, z),
                5 => (x + 1, y, z),
                _ => (x, y + 1, z),
            };
            let place_pos = BlockPos(Vector3 { x: nx, y: ny, z: nz });
            
            let mut block_id = 265;
            if player.held_slot < 9 {
                let inventory_idx = player.held_slot as usize + 36;
                if inventory_idx < player.inventory.len() && !player.inventory[inventory_idx].is_empty() {
                    let mut cur = 0;
                    if let Ok(item_count) = crate::viakraken::utils::read_varint_from_slice(&player.inventory[inventory_idx], &mut cur) {
                        if item_count > 0 {
                            if let Ok(item_id) = crate::viakraken::utils::read_varint_from_slice(&player.inventory[inventory_idx], &mut cur) {
                                if let Some(block) = pumpkin_data::Block::from_item_id(item_id as u16) {
                                    block_id = block.default_state.id as i32;
                                }
                            }
                        }
                    }
                }
            }

            save_block_change(db, nx, ny, nz, block_id as u16);

            let block_update = CBlockUpdate::new(place_pos, VarInt(block_id as i32));
            let block_update_payload = encode_java_packet(&block_update, version)?;
            let _ = block_channel().send(Bytes::from(block_update_payload.as_slice().to_vec()));

            if sequence > 0 {
                let ack = CAcknowledgeBlockChange::new(VarInt(sequence));
                let ack_payload = encode_java_packet(&ack, version)?;
                write_framed_payload(stream, ack_payload.as_slice()).await?;
            }
        }
    } else if pid == sv_interact {
        use pumpkin_protocol::java::server::play::SInteract;
        if let Ok(pkt) = SInteract::read(&mut std::io::Cursor::new(payload), &version) {
            if pkt.r#type.0 == 1 {
                let target_entity_id = pkt.entity_id.0;
                let target_info = {
                    let guard = online_players().lock().unwrap();
                    guard.values().find(|op| op.entity_id == target_entity_id).map(|op| (op.uuid, op.x, op.y, op.z, op.gamemode))
                };
                if let Some((vic_uuid, vx, vy, vz, vic_gm)) = target_info {
                    if vic_gm != 1 && vic_gm != 3 {
                        let _ = player_event_channel().send(PlayerEvent::Hurt {
                            entity_id: target_entity_id,
                            uuid: vic_uuid,
                            damage: 1.0,
                            x: vx,
                            y: vy,
                            z: vz,
                            attacker_x: Some(player.x),
                            attacker_z: Some(player.z),
                        });
                    }
                }
            }
        }
    } else if pid == sv_player_action {
        let mut o = 0usize;
        let status = read_varint_from_slice(payload, &mut o).unwrap_or(0);
        if o + 8 <= payload.len() {
            let pos_val = i64::from_be_bytes(payload[o..o+8].try_into().unwrap_or_default());
            let x = (pos_val >> 38) as i32;
            let y = ((pos_val << 52) >> 52) as i32;
            let z = ((pos_val << 26) >> 38) as i32;
            let mut seq_o = o + 8 + 1; // skip pos (8 bytes) and face (1 byte)
            let sequence = if seq_o < payload.len() {
                read_varint_from_slice(payload, &mut seq_o).unwrap_or(0)
            } else {
                0
            };

            if status == 2 || (status == 0 && player.gamemode == 1) {
                save_block_change(db, x, y, z, 0);
                let block_update = CBlockUpdate::new(BlockPos(Vector3 { x, y, z }), VarInt(0));
                if let Ok(block_update_payload) = encode_java_packet(&block_update, version) {
                    let _ = block_channel().send(Bytes::from(block_update_payload.as_slice().to_vec()));
                }

                use super::state::{ItemEvent, NEXT_ENTITY_ID, item_event_channel};
                let item_entity_id = NEXT_ENTITY_ID.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let _ = item_event_channel().send(ItemEvent::Spawn {
                    entity_id: item_entity_id,
                    item_id: 1,
                    x: x as f64 + 0.5,
                    y: y as f64 + 0.5,
                    z: z as f64 + 0.5,
                    vx: 0.0,
                    vy: 0.2,
                    vz: 0.0,
                });
            } else if status == 3 || status == 4 {
                use super::state::{ItemEvent, NEXT_ENTITY_ID, item_event_channel};
                let item_entity_id = NEXT_ENTITY_ID.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let yaw_rad = (player.yaw + 90.0) * (std::f32::consts::PI / 180.0);
                let pitch_rad = -player.pitch * (std::f32::consts::PI / 180.0);
                let vx = (yaw_rad.cos() * pitch_rad.cos() * 0.3) as f64;
                let vy = (pitch_rad.sin() * 0.3 + 0.1) as f64;
                let vz = (yaw_rad.sin() * pitch_rad.cos() * 0.3) as f64;

                let _ = item_event_channel().send(ItemEvent::Spawn {
                    entity_id: item_entity_id,
                    item_id: 1,
                    x: player.x,
                    y: player.y + 1.5,
                    z: player.z,
                    vx,
                    vy,
                    vz,
                });
            }

            if sequence > 0 {
                let ack = CAcknowledgeBlockChange::new(VarInt(sequence));
                if let Ok(ack_payload) = encode_java_packet(&ack, version) {
                    let _ = write_framed_payload(stream, ack_payload.as_slice()).await;
                }
            }
        }
    } else if pid == sv_creative_slot {
        use pumpkin_protocol::java::server::play::SSetCreativeSlot;
        if let Ok(pkt) = SSetCreativeSlot::read(&mut std::io::Cursor::new(payload), &version) {
            let slot = pkt.slot;
            if slot >= 0 && slot < 46 {
                let mut buf = Vec::new();
                if pkt.clicked_item.write_with_version(&mut buf, &version).is_ok() {
                    player.inventory[slot as usize] = buf;
                }
            }
        }
    } else if pid == sv_held_item {
        if payload.len() >= 2 {
            player.held_slot = payload[1];
        }
    } else if pid == sv_change_gm && sv_change_gm >= 0 {
        let mut o = 0usize;
        let gm_id = read_varint_from_slice(payload, &mut o).unwrap_or(0);
        change_gamemode(stream, version, player, uuid, gm_id as u8).await?;
    } else if pid == sv_chat_cmd {
        let mut o = 0usize;
        let cmd_len = read_varint_from_slice(payload, &mut o).unwrap_or(0) as usize;
        if o + cmd_len <= payload.len() {
            if let Ok(cmd) = std::str::from_utf8(&payload[o..o + cmd_len]) {
                handle_command(stream, version, player, uuid, cmd, username).await?;
            }
        }
    } else if pid == sv_pos_rot {
        if payload.len() >= 33 {
            player.x = f64::from_be_bytes(payload[0..8].try_into().unwrap_or_default());
            player.y = f64::from_be_bytes(payload[8..16].try_into().unwrap_or_default());
            player.z = f64::from_be_bytes(payload[16..24].try_into().unwrap_or_default());
            player.yaw = f32::from_be_bytes(payload[24..28].try_into().unwrap_or_default());
            player.pitch = f32::from_be_bytes(payload[28..32].try_into().unwrap_or_default());
            
            let on_ground = payload[32] != 0;
            process_fall_damage(stream, player, on_ground, my_entity_id, uuid).await?;
            moved = true;
        }
    } else if pid == sv_pos {
        if payload.len() >= 25 {
            player.x = f64::from_be_bytes(payload[0..8].try_into().unwrap_or_default());
            player.y = f64::from_be_bytes(payload[8..16].try_into().unwrap_or_default());
            player.z = f64::from_be_bytes(payload[16..24].try_into().unwrap_or_default());
            
            let on_ground = payload[24] != 0;
            process_fall_damage(stream, player, on_ground, my_entity_id, uuid).await?;
            moved = true;
        }
    } else if pid == pumpkin_data::packet::serverbound::PLAY_MOVE_PLAYER_ROT.to_id(version) {
        if payload.len() >= 8 {
            player.yaw = f32::from_be_bytes(payload[0..4].try_into().unwrap_or_default());
            player.pitch = f32::from_be_bytes(payload[4..8].try_into().unwrap_or_default());
            moved = true;
        }
    } else if pid == sv_chat {
        if let Ok(msg) = SChatMessage::read(&mut std::io::Cursor::new(payload), &version) {
            let _ = chat_channel().send(format!("<{}> {}", username, msg.message));
        }
    } else if pid == sv_client_cmd {
        use pumpkin_protocol::java::server::play::SClientCommand;
        if let Ok(pkt) = SClientCommand::read(&mut std::io::Cursor::new(payload), &version) {
            if pkt.action_id.0 == 0 {
                player.health = 20.0;
                player.x = 0.0;
                player.y = 70.0;
                player.z = 0.0;
                player.highest_y = 70.0;

                let respawn_pkt = CRespawn::new(
                    VarInt(pumpkin_data::dimension::Dimension::OVERWORLD.id as i32),
                    "minecraft:overworld".to_string(),
                    123456789i64,
                    player.gamemode,
                    -1,
                    false,
                    true,
                    None,
                    VarInt(0),
                    VarInt(63),
                    0,
                );
                if let Ok(respawn_payload) = encode_java_packet(&respawn_pkt, version) {
                    let _ = write_framed_payload(stream, respawn_payload.as_slice()).await;
                }

                use pumpkin_protocol::java::client::play::CSetHealth;
                let hp = CSetHealth::new(player.health, VarInt(20), 20.0);
                if let Ok(hp_payload) = encode_java_packet(&hp, version) {
                    let _ = write_framed_payload(stream, hp_payload.as_slice()).await;
                }

                let pos_pkt = CPlayerPosition::new(
                    VarInt(1),
                    Vector3 { x: 0.0, y: 70.0, z: 0.0 },
                    Vector3 { x: 0.0, y: 0.0, z: 0.0 },
                    0.0,
                    0.0,
                    vec![],
                );
                if let Ok(pos_payload) = encode_java_packet(&pos_pkt, version) {
                    let _ = write_framed_payload(stream, pos_payload.as_slice()).await;
                }

                let waiting = CGameEvent::new(GameEvent::StartWaitingChunks, 0.0);
                if let Ok(payload) = encode_java_packet(&waiting, version) {
                    let _ = write_framed_payload(stream, payload.as_slice()).await;
                }

                use pumpkin_protocol::java::client::play::{CCenterChunk, CChunkBatchStart, CChunkBatchEnd};
                let center = CCenterChunk { chunk_x: VarInt(0), chunk_z: VarInt(0) };
                if let Ok(payload) = encode_java_packet(&center, version) {
                    let _ = write_framed_payload(stream, payload.as_slice()).await;
                }

                let batch_start = CChunkBatchStart;
                if let Ok(payload) = encode_java_packet(&batch_start, version) {
                    let _ = write_framed_payload(stream, payload.as_slice()).await;
                }

                let mut chunk_count = 0u16;
                let proto_ver = version.protocol_version();
                for dz in -3i32..=3 {
                    for dx in -3i32..=3 {
                        let chunk_data = crate::world::chunk_gen::encode_chunk_packet(dx, dz, proto_ver, db);
                        let _ = write_framed_payload(stream, &chunk_data).await;
                        chunk_count += 1;
                    }
                }

                let batch_end = CChunkBatchEnd::new(chunk_count);
                if let Ok(payload) = encode_java_packet(&batch_end, version) {
                    let _ = write_framed_payload(stream, payload.as_slice()).await;
                }

                moved = true;
            }
        }
    }

    if player.y < -64.0 {
        player.x = 0.0;
        player.y = 70.0;
        player.z = 0.0;
        
        let pos_pkt = CPlayerPosition::new(
            VarInt(1),
            Vector3 { x: player.x, y: player.y, z: player.z },
            Vector3 { x: 0.0, y: 0.0, z: 0.0 },
            player.yaw,
            player.pitch,
            vec![],
        );
        let payload = encode_java_packet(&pos_pkt, version)?;
        write_framed_payload(stream, payload.as_slice()).await?;
        moved = true;
    }

    if moved {
        {
            let mut players_guard = online_players().lock().unwrap();
            if let Some(op) = players_guard.get_mut(&uuid) {
                op.x = player.x;
                op.y = player.y;
                op.z = player.z;
                op.yaw = player.yaw;
                op.pitch = player.pitch;
            }
        }
        let _ = player_event_channel().send(PlayerEvent::Move {
            entity_id: my_entity_id,
            uuid,
            x: player.x,
            y: player.y,
            z: player.z,
            yaw: player.yaw,
            pitch: player.pitch,
        });
    }

    Ok(())
}

pub async fn change_gamemode(
    stream: &mut TcpStream,
    version: MinecraftVersion,
    player: &mut PlayerData,
    uuid: Uuid,
    gm: u8,
) -> std::io::Result<()> {
    player.gamemode = gm;

    {
        let mut players_guard = online_players().lock().unwrap();
        if let Some(op) = players_guard.get_mut(&uuid) {
            op.gamemode = gm;
        }
    }

    let _ = player_event_channel().send(PlayerEvent::GamemodeChange {
        uuid,
        gamemode: gm,
    });

    let ge = CGameEvent::new(GameEvent::ChangeGameMode, gm as f32);
    let payload = encode_java_packet(&ge, version)?;
    write_framed_payload(stream, payload.as_slice()).await?;

    let (flags, fly_speed) = gamemode_abilities(gm);
    let abilities = CPlayerAbilities::new(flags, fly_speed, 0.1);
    let payload = encode_java_packet(&abilities, version)?;
    write_framed_payload(stream, payload.as_slice()).await?;

    let actions = vec![
        PlayerAction::UpdateGameMode(VarInt(gm as i32)),
    ];
    let players = vec![Player {
        uuid,
        actions: &actions,
    }];
    let info_update = CPlayerInfoUpdate::new(PlayerInfoFlags::UPDATE_GAME_MODE.bits(), &players);
    let payload = encode_java_packet(&info_update, version)?;
    write_framed_payload(stream, payload.as_slice()).await?;
    Ok(())
}

async fn handle_command(
    stream: &mut TcpStream,
    version: MinecraftVersion,
    player: &mut PlayerData,
    uuid: Uuid,
    cmd: &str,
    username: &str,
) -> std::io::Result<()> {
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    if !parts.is_empty() {
        if parts[0] == "gamemode" && parts.len() > 1 {
            let gm_name = parts[1].to_lowercase();
            let new_gm = match gm_name.as_str() {
                "survival" | "0" => Some(0),
                "creative" | "1" => Some(1),
                "adventure" | "2" => Some(2),
                "spectator" | "3" => Some(3),
                _ => None,
            };
            if let Some(gm) = new_gm {
                change_gamemode(stream, version, player, uuid, gm).await?;
                let msg_text = format!("Set own game mode to {} Mode", parts[1]);
                send_system_message(stream, version, &msg_text).await?;
                log_info!("{}: /gamemode {}", username, parts[1]);
            } else {
                send_system_message(stream, version, "Unknown gamemode. Use: survival, creative, adventure, spectator").await?;
            }
        } else {
            send_system_message(stream, version, &format!("Unknown command: /{}", cmd)).await?;
        }
    }
    Ok(())
}

async fn process_fall_damage(
    _stream: &mut TcpStream,
    player: &mut PlayerData,
    on_ground: bool,
    my_entity_id: i32,
    uuid: Uuid,
) -> std::io::Result<()> {
    if on_ground {
        let fall_dist = player.highest_y - player.y;
        if fall_dist > 3.0 && player.gamemode == 0 {
            let damage = (fall_dist - 3.0).ceil() as f32;
            let _ = player_event_channel().send(PlayerEvent::Hurt {
                entity_id: my_entity_id,
                uuid,
                damage,
                x: player.x,
                y: player.y,
                z: player.z,
                attacker_x: None,
                attacker_z: None,
            });
        }
        player.highest_y = player.y;
    } else {
        if player.y > player.highest_y {
            player.highest_y = player.y;
        }
    }
    Ok(())
}

pub async fn send_system_message(
    stream: &mut TcpStream,
    version: MinecraftVersion,
    text: &str,
) -> std::io::Result<()> {
    let content = TextComponent::text(text.to_owned());
    let msg = CSystemChatMessage::new(&content, false);
    let payload = encode_java_packet(&msg, version)?;
    write_framed_payload(stream, payload.as_slice()).await
}
