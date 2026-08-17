use std::sync::Arc;
use tokio::net::TcpStream;
use uuid::Uuid;

use pumpkin_protocol::codec::item_stack_seralizer::ItemStackSerializer;
use pumpkin_protocol::codec::var_int::VarInt;
use pumpkin_protocol::java::client::play::{
    CAcknowledgeBlockChange, CGameEvent, CPlayerAbilities, CPlayerInfoUpdate, CPlayerPosition,
    CRespawn, CSetContainerSlot, CSetSelectedSlot, CSystemChatMessage, GameEvent, Player,
    PlayerAction, PlayerInfoFlags,
};
use pumpkin_protocol::java::server::play::SChatMessage;
use pumpkin_protocol::ServerPacket;
use pumpkin_util::math::vector3::Vector3;
use pumpkin_util::text::TextComponent;
use pumpkin_util::version::MinecraftVersion;

use crate::logger::log_info;
use crate::viakraken::java::packets::encode_java_packet;
use crate::viakraken::utils::{read_varint_from_slice, write_framed_payload};
use crate::world::chunk_gen::{get_block_state, save_block_change};
use crate::world::player_store::PlayerData;

use super::play::{send_command_tree, send_permission_status};
use super::state::{
    block_channel, chat_channel, console_command_channel, gamemode_abilities, online_players,
    player_event_channel, register_summoned_entity, BlockUpdateEvent, ConsoleCommand, PlayerEvent,
    NEXT_ENTITY_ID,
};

const PLAYER_INVENTORY_SLOTS: usize = 46;
const HOTBAR_START_SLOT: usize = 36;
const HOTBAR_SLOT_COUNT: u8 = 9;
const VOID_KILL_Y: f64 = -128.0;

fn inventory_item_id(slot: &[u8], version: MinecraftVersion) -> Option<u16> {
    let mut offset = 0;
    let item_count = read_varint_from_slice(slot, &mut offset).ok()?;
    if item_count <= 0 {
        return None;
    }
    let network_item_id = u16::try_from(read_varint_from_slice(slot, &mut offset).ok()?).ok()?;
    Some(pumpkin_data::item_id_remap::remap_item_id_from_version(
        network_item_id,
        version,
    ))
}

fn held_block_state(player: &PlayerData, hand: i32, version: MinecraftVersion) -> Option<u16> {
    let inventory_index = match hand {
        0 if player.held_slot < HOTBAR_SLOT_COUNT => HOTBAR_START_SLOT + player.held_slot as usize,
        1 => PLAYER_INVENTORY_SLOTS - 1,
        _ => return None,
    };
    let slot = player.inventory.get(inventory_index)?;
    let item_id = inventory_item_id(slot, version)?;
    pumpkin_data::Block::from_item_id(item_id).map(|block| block.default_state.id)
}

fn block_drop_item(state_id: u16) -> Option<&'static pumpkin_data::item::Item> {
    let block = pumpkin_data::Block::from_state_id(state_id);
    let item = match block.name {
        "stone" => &pumpkin_data::item::Item::COBBLESTONE,
        "deepslate" => &pumpkin_data::item::Item::COBBLED_DEEPSLATE,
        "grass_block" => &pumpkin_data::item::Item::DIRT,
        "coal_ore" | "deepslate_coal_ore" => &pumpkin_data::item::Item::COAL,
        "copper_ore" | "deepslate_copper_ore" => &pumpkin_data::item::Item::RAW_COPPER,
        "iron_ore" | "deepslate_iron_ore" => &pumpkin_data::item::Item::RAW_IRON,
        "gold_ore" | "deepslate_gold_ore" => &pumpkin_data::item::Item::RAW_GOLD,
        "diamond_ore" | "deepslate_diamond_ore" => &pumpkin_data::item::Item::DIAMOND,
        "emerald_ore" | "deepslate_emerald_ore" => &pumpkin_data::item::Item::EMERALD,
        "lapis_ore" | "deepslate_lapis_ore" => &pumpkin_data::item::Item::LAPIS_LAZULI,
        "redstone_ore" | "deepslate_redstone_ore" => &pumpkin_data::item::Item::REDSTONE,
        _ if block.item_id != 0 => pumpkin_data::item::Item::from_id(block.item_id)?,
        _ => return None,
    };
    Some(item)
}

fn is_void_lethal(y: f64) -> bool {
    y <= VOID_KILL_Y
}

fn equipped_totem_slot(player: &PlayerData, version: MinecraftVersion) -> Option<usize> {
    let main_hand_slot = HOTBAR_START_SLOT + player.held_slot.min(8) as usize;
    [main_hand_slot, PLAYER_INVENTORY_SLOTS - 1]
        .into_iter()
        .find(|slot| {
            player
                .inventory
                .get(*slot)
                .and_then(|stack| inventory_item_id(stack, version))
                == Some(pumpkin_data::item::Item::TOTEM_OF_UNDYING.id)
        })
}

async fn try_pop_totem(
    stream: &mut TcpStream,
    version: MinecraftVersion,
    player: &mut PlayerData,
    entity_id: i32,
) -> std::io::Result<bool> {
    if let Some(slot) = equipped_totem_slot(player, version) {
        player.inventory[slot].clear();
        let empty = ItemStackSerializer::from(pumpkin_data::item_stack::ItemStack::EMPTY.clone());
        let slot_update = CSetContainerSlot::new(0, 0, slot as i16, &empty);
        let payload = encode_java_packet(&slot_update, version)?;
        write_framed_payload(stream, payload.as_slice()).await?;

        let animation = pumpkin_protocol::java::client::play::CEntityStatus::new(entity_id, 35);
        let payload = encode_java_packet(&animation, version)?;
        write_framed_payload(stream, payload.as_slice()).await?;
        return Ok(true);
    }
    Ok(false)
}

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
    let sv_creative_slot =
        pumpkin_data::packet::serverbound::PLAY_SET_CREATIVE_MODE_SLOT.to_id(version);
    let sv_held_item = pumpkin_data::packet::serverbound::PLAY_SET_CARRIED_ITEM.to_id(version);
    let sv_player_action = pumpkin_data::packet::serverbound::PLAY_PLAYER_ACTION.to_id(version);
    let sv_client_cmd = pumpkin_data::packet::serverbound::PLAY_CLIENT_COMMAND.to_id(version);
    let sv_interact = pumpkin_data::packet::serverbound::PLAY_INTERACT.to_id(version);
    let sv_pick_block = pumpkin_data::packet::serverbound::PLAY_PICK_ITEM_FROM_BLOCK.to_id(version);

    let mut moved = false;

    if pid == sv_use_item_on {
        use pumpkin_protocol::java::server::play::SUseItemOn;
        if let Ok(pkt) = SUseItemOn::read(&mut std::io::Cursor::new(payload), &version) {
            let x = pkt.position.0.x;
            let y = pkt.position.0.y;
            let z = pkt.position.0.z;

            let (nx, ny, nz) = match pkt.face.0 {
                0 => (x, y - 1, z),
                1 => (x, y + 1, z),
                2 => (x, y, z - 1),
                3 => (x, y, z + 1),
                4 => (x - 1, y, z),
                5 => (x + 1, y, z),
                _ => (x, y + 1, z),
            };

            if let Some(block_id) = held_block_state(player, pkt.hand.0, version) {
                save_block_change(db, nx, ny, nz, block_id);
                let _ = block_channel().send(BlockUpdateEvent {
                    x: nx,
                    y: ny,
                    z: nz,
                    state_id: block_id,
                });
            }

            if pkt.sequence.0 > 0 {
                let ack = CAcknowledgeBlockChange::new(pkt.sequence);
                let ack_payload = encode_java_packet(&ack, version)?;
                write_framed_payload(stream, ack_payload.as_slice()).await?;
            }
        }
    } else if pid == sv_pick_block && sv_pick_block >= 0 {
        use pumpkin_data::item::Item;
        use pumpkin_data::item_stack::ItemStack;
        use pumpkin_protocol::java::server::play::SPickItemFromBlock;

        if player.gamemode != 1 {
            return Ok(());
        }
        if let Ok(packet) = SPickItemFromBlock::read(&mut std::io::Cursor::new(payload), &version) {
            let position = packet.pos.0;
            let state = get_block_state(db, position.x, position.y, position.z);
            let block = pumpkin_data::Block::from_state_id(state);
            if block.item_id == 0 {
                return Ok(());
            }
            let Some(item) = Item::from_id(block.item_id) else {
                return Ok(());
            };

            let existing_hotbar_slot = (0..HOTBAR_SLOT_COUNT).find(|hotbar_slot| {
                let inventory_slot = HOTBAR_START_SLOT + *hotbar_slot as usize;
                player
                    .inventory
                    .get(inventory_slot)
                    .and_then(|slot| inventory_item_id(slot, version))
                    == Some(block.item_id)
            });
            let selected_slot = existing_hotbar_slot.unwrap_or(player.held_slot.min(8));
            player.held_slot = selected_slot;
            let inventory_slot = HOTBAR_START_SLOT + selected_slot as usize;

            if existing_hotbar_slot.is_none() {
                let serialized_item = ItemStackSerializer::from(ItemStack::new(1, item));
                let mut bytes = Vec::new();
                serialized_item
                    .write_with_version(&mut bytes, &version)
                    .map_err(|error| {
                        std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string())
                    })?;
                player.inventory[inventory_slot] = bytes;
                let slot_update =
                    CSetContainerSlot::new(0, 0, inventory_slot as i16, &serialized_item);
                let payload = encode_java_packet(&slot_update, version)?;
                write_framed_payload(stream, payload.as_slice()).await?;
            }

            let selected = CSetSelectedSlot::new(selected_slot as i8);
            let payload = encode_java_packet(&selected, version)?;
            write_framed_payload(stream, payload.as_slice()).await?;
        }
    } else if pid == sv_interact {
        use pumpkin_protocol::java::server::play::SInteract;
        if let Ok(pkt) = SInteract::read(&mut std::io::Cursor::new(payload), &version) {
            if pkt.r#type.0 == 1 {
                let target_entity_id = pkt.entity_id.0;
                let target_info = {
                    let guard = online_players().lock().unwrap();
                    guard
                        .values()
                        .find(|op| op.entity_id == target_entity_id)
                        .map(|op| (op.uuid, op.x, op.y, op.z, op.gamemode))
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
            let pos_val = i64::from_be_bytes(payload[o..o + 8].try_into().unwrap_or_default());
            let x = (pos_val >> 38) as i32;
            let y = ((pos_val << 52) >> 52) as i32;
            let z = ((pos_val << 26) >> 38) as i32;
            let mut seq_o = o + 8 + 1; // skip pos (8 bytes) and face (1 byte)
            let sequence = if seq_o < payload.len() {
                read_varint_from_slice(payload, &mut seq_o).unwrap_or(0)
            } else {
                0
            };

            if (status == 2 && player.gamemode != 3) || (status == 0 && player.gamemode == 1) {
                let broken_state = get_block_state(db, x, y, z);
                save_block_change(db, x, y, z, 0);
                let _ = block_channel().send(BlockUpdateEvent {
                    x,
                    y,
                    z,
                    state_id: pumpkin_data::Block::AIR.default_state.id,
                });

                if matches!(player.gamemode, 0 | 2) {
                    use super::state::{item_event_channel, ItemEvent, NEXT_ENTITY_ID};
                    if let Some(item) = block_drop_item(broken_state) {
                        let item_entity_id =
                            NEXT_ENTITY_ID.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let _ = item_event_channel().send(ItemEvent::Spawn {
                            entity_id: item_entity_id,
                            item_id: item.id,
                            count: 1,
                            x: x as f64 + 0.5,
                            y: y as f64 + 0.5,
                            z: z as f64 + 0.5,
                            vx: 0.0,
                            vy: 0.2,
                            vz: 0.0,
                        });
                    }
                }
            } else if status == 3 || status == 4 {
                use super::state::{item_event_channel, ItemEvent, NEXT_ENTITY_ID};
                let item_entity_id =
                    NEXT_ENTITY_ID.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let yaw_rad = (player.yaw + 90.0) * (std::f32::consts::PI / 180.0);
                let pitch_rad = -player.pitch * (std::f32::consts::PI / 180.0);
                let vx = (yaw_rad.cos() * pitch_rad.cos() * 0.3) as f64;
                let vy = (pitch_rad.sin() * 0.3 + 0.1) as f64;
                let vz = (yaw_rad.sin() * pitch_rad.cos() * 0.3) as f64;

                let _ = item_event_channel().send(ItemEvent::Spawn {
                    entity_id: item_entity_id,
                    item_id: 1,
                    count: 1,
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
            if player.gamemode == 1 && slot >= 1 && slot < PLAYER_INVENTORY_SLOTS as i16 {
                let item_stack = pkt.clicked_item.to_stack_for_version(&version);
                let is_legal = item_stack.is_empty()
                    || item_stack.item_count <= item_stack.get_max_stack_size();
                if !is_legal {
                    return Ok(());
                }

                let serialized_item = ItemStackSerializer::from(item_stack);
                let mut buf = Vec::new();
                if serialized_item
                    .write_with_version(&mut buf, &version)
                    .is_ok()
                {
                    player.inventory[slot as usize] = buf;

                    let slot_update = CSetContainerSlot::new(0, 0, slot, &serialized_item);
                    let update_payload = encode_java_packet(&slot_update, version)?;
                    write_framed_payload(stream, update_payload.as_slice()).await?;
                }
            }
        }
    } else if pid == sv_held_item {
        use pumpkin_protocol::java::server::play::SSetHeldItem;
        if let Ok(pkt) = SSetHeldItem::read(&mut std::io::Cursor::new(payload), &version) {
            if (0..HOTBAR_SLOT_COUNT as i16).contains(&pkt.slot) {
                player.held_slot = pkt.slot as u8;
            }
        }
    } else if pid == sv_change_gm && sv_change_gm >= 0 {
        if player.operator_level > 0 {
            let mut o = 0usize;
            let gm_id = read_varint_from_slice(payload, &mut o).unwrap_or(0);
            if (0..=3).contains(&gm_id) {
                change_gamemode(stream, version, player, uuid, gm_id as u8).await?;
            }
        }
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
            process_fall_damage(stream, version, player, on_ground, my_entity_id, uuid).await?;
            moved = true;
        }
    } else if pid == sv_pos {
        if payload.len() >= 25 {
            player.x = f64::from_be_bytes(payload[0..8].try_into().unwrap_or_default());
            player.y = f64::from_be_bytes(payload[8..16].try_into().unwrap_or_default());
            player.z = f64::from_be_bytes(payload[16..24].try_into().unwrap_or_default());

            let on_ground = payload[24] != 0;
            process_fall_damage(stream, version, player, on_ground, my_entity_id, uuid).await?;
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

                send_permission_status(stream, version, my_entity_id, player.operator_level)
                    .await?;
                send_command_tree(stream, version, player.operator_level > 0).await?;

                let pos_pkt = CPlayerPosition::new(
                    VarInt(1),
                    Vector3 {
                        x: 0.0,
                        y: 70.0,
                        z: 0.0,
                    },
                    Vector3 {
                        x: 0.0,
                        y: 0.0,
                        z: 0.0,
                    },
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

                use pumpkin_protocol::java::client::play::{
                    CCenterChunk, CChunkBatchEnd, CChunkBatchStart,
                };
                let center = CCenterChunk {
                    chunk_x: VarInt(0),
                    chunk_z: VarInt(0),
                };
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
                        let chunk_data =
                            crate::world::chunk_gen::encode_chunk_packet(dx, dz, proto_ver, db);
                        let _ = write_framed_payload(stream, chunk_data.as_slice()).await;
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

    if is_void_lethal(player.y) && player.health > 0.0 && !matches!(player.gamemode, 1 | 3) {
        player.health = 0.0;
        let health = pumpkin_protocol::java::client::play::CSetHealth::new(0.0, VarInt(20), 20.0);
        let payload = encode_java_packet(&health, version)?;
        write_framed_payload(stream, payload.as_slice()).await?;
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

    let _ = player_event_channel().send(PlayerEvent::GamemodeChange { uuid, gamemode: gm });

    let ge = CGameEvent::new(GameEvent::ChangeGameMode, gm as f32);
    let payload = encode_java_packet(&ge, version)?;
    write_framed_payload(stream, payload.as_slice()).await?;

    let (flags, fly_speed) = gamemode_abilities(gm);
    let abilities = CPlayerAbilities::new(flags, fly_speed, 0.1);
    let payload = encode_java_packet(&abilities, version)?;
    write_framed_payload(stream, payload.as_slice()).await?;

    let actions = vec![PlayerAction::UpdateGameMode(VarInt(gm as i32))];
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
            if player.operator_level == 0 {
                send_system_message(
                    stream,
                    version,
                    "You do not have permission to use this command.",
                )
                .await?;
                return Ok(());
            }
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
                send_system_message(
                    stream,
                    version,
                    "Unknown gamemode. Use: survival, creative, adventure, spectator",
                )
                .await?;
            }
        } else if parts[0] == "summon" && matches!(parts.len(), 2 | 5) {
            if player.operator_level == 0 {
                send_system_message(
                    stream,
                    version,
                    "You do not have permission to use this command.",
                )
                .await?;
                return Ok(());
            }
            let entity_name = parts[1].strip_prefix("minecraft:").unwrap_or(parts[1]);
            let Some(entity_type) = pumpkin_data::entity::EntityType::from_name(entity_name) else {
                send_system_message(stream, version, "Unknown entity type.").await?;
                return Ok(());
            };
            if !entity_type.summonable || entity_type == &pumpkin_data::entity::EntityType::PLAYER {
                send_system_message(stream, version, "That entity cannot be summoned.").await?;
                return Ok(());
            }
            let coordinates = if parts.len() == 5 {
                let parsed = parts[2..5]
                    .iter()
                    .map(|value| value.parse::<f64>())
                    .collect::<Result<Vec<_>, _>>();
                let Ok(values) = parsed else {
                    send_system_message(stream, version, "Summon coordinates must be numbers.")
                        .await?;
                    return Ok(());
                };
                (values[0], values[1], values[2])
            } else {
                (player.x, player.y, player.z)
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
            send_system_message(
                stream,
                version,
                &format!(
                    "Summoned {} at {:.1}, {:.1}, {:.1}",
                    entity_name, coordinates.0, coordinates.1, coordinates.2
                ),
            )
            .await?;
            log_info!("{}: /{}", username, cmd);
        } else {
            send_system_message(stream, version, &format!("Unknown command: /{}", cmd)).await?;
        }
    }
    Ok(())
}

async fn process_fall_damage(
    stream: &mut TcpStream,
    version: MinecraftVersion,
    player: &mut PlayerData,
    on_ground: bool,
    my_entity_id: i32,
    uuid: Uuid,
) -> std::io::Result<()> {
    if on_ground {
        let fall_dist = player.highest_y - player.y;
        if fall_dist > 3.0 && matches!(player.gamemode, 0 | 2) {
            let damage = (fall_dist - 3.0).ceil() as f32;
            if try_pop_totem(stream, version, player, my_entity_id).await? {
                player.highest_y = player.y;
                return Ok(());
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_hand_has_no_placeable_block() {
        let player = PlayerData::default();
        assert_eq!(held_block_state(&player, 0, MinecraftVersion::V_26_1), None);
    }

    #[test]
    fn void_only_becomes_lethal_below_the_fall_zone() {
        assert!(!is_void_lethal(-64.1));
        assert!(!is_void_lethal(-127.9));
        assert!(is_void_lethal(-128.0));
    }

    #[test]
    fn detects_totems_in_selected_hand_and_offhand() {
        let version = MinecraftVersion::V_26_1;
        let serialized = ItemStackSerializer::from(pumpkin_data::item_stack::ItemStack::new(
            1,
            &pumpkin_data::item::Item::TOTEM_OF_UNDYING,
        ));
        let mut bytes = Vec::new();
        serialized.write_with_version(&mut bytes, &version).unwrap();

        let mut player = PlayerData::default();
        player.held_slot = 2;
        player.inventory[HOTBAR_START_SLOT + 2] = bytes.clone();
        assert_eq!(
            equipped_totem_slot(&player, version),
            Some(HOTBAR_START_SLOT + 2)
        );

        player.inventory[HOTBAR_START_SLOT + 2].clear();
        player.inventory[PLAYER_INVENTORY_SLOTS - 1] = bytes;
        assert_eq!(
            equipped_totem_slot(&player, version),
            Some(PLAYER_INVENTORY_SLOTS - 1)
        );
    }

    #[test]
    fn terrain_blocks_use_survival_style_drops() {
        assert_eq!(
            block_drop_item(pumpkin_data::Block::STONE.default_state.id).map(|item| item.id),
            Some(pumpkin_data::item::Item::COBBLESTONE.id)
        );
        assert_eq!(
            block_drop_item(pumpkin_data::Block::COPPER_ORE.default_state.id).map(|item| item.id),
            Some(pumpkin_data::item::Item::RAW_COPPER.id)
        );
        assert!(block_drop_item(pumpkin_data::Block::AIR.default_state.id).is_none());
    }
}
