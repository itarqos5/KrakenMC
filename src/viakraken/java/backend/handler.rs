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
use crate::viakraken::utils::{read_varint_from_slice, write_framed_payload, write_varint};
use crate::world::chunk_gen::{get_block_state, save_block_change};
use crate::world::player_store::PlayerData;

use super::play::{send_command_tree, send_permission_status, store_inventory_item};
use super::state::{
    block_channel, chat_channel, console_command_channel, gamemode_abilities, online_players,
    player_event_channel, register_summoned_entity, spawn_dropped_item, BlockUpdateEvent,
    ConsoleCommand, PlayerEvent, NEXT_ENTITY_ID,
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

fn inventory_stack(
    slot: &[u8],
    version: MinecraftVersion,
) -> Option<(&'static pumpkin_data::item::Item, u8)> {
    let mut offset = 0;
    let count = u8::try_from(read_varint_from_slice(slot, &mut offset).ok()?).ok()?;
    if count == 0 {
        return None;
    }
    let network_id = u16::try_from(read_varint_from_slice(slot, &mut offset).ok()?).ok()?;
    let item_id = pumpkin_data::item_id_remap::remap_item_id_from_version(network_id, version);
    Some((pumpkin_data::item::Item::from_id(item_id)?, count))
}

fn serialized_stack(
    item: &'static pumpkin_data::item::Item,
    count: u8,
    version: MinecraftVersion,
) -> std::io::Result<Vec<u8>> {
    let stack = ItemStackSerializer::from(pumpkin_data::item_stack::ItemStack::new(count, item));
    let mut bytes = Vec::new();
    stack
        .write_with_version(&mut bytes, &version)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()))?;
    Ok(bytes)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CraftingResult {
    item_id: u16,
    count: u8,
    consumed_slots: [bool; 4],
}

fn crafting_result(player: &PlayerData, version: MinecraftVersion) -> Option<CraftingResult> {
    use pumpkin_data::recipes::{CraftingRecipeTypes, RECIPES_CRAFTING};

    let grid = std::array::from_fn::<_, 4, _>(|index| {
        player
            .inventory
            .get(index + 1)
            .and_then(|slot| inventory_stack(slot, version))
            .map(|(item, _)| item)
    });
    for recipe in RECIPES_CRAFTING {
        let (result, consumed_slots) = match recipe {
            CraftingRecipeTypes::CraftingShaped {
                key,
                pattern,
                result,
                ..
            } if pattern.len() <= 2 && pattern.iter().all(|row| row.len() <= 2) => {
                let mut matched = None;
                let width = pattern.iter().map(|row| row.len()).max().unwrap_or(0);
                for offset_y in 0..=2 - pattern.len() {
                    for offset_x in 0..=2 - width {
                        for mirrored in [false, true] {
                            let mut consumed = [false; 4];
                            let mut valid = true;
                            for grid_y in 0usize..2 {
                                for grid_x in 0usize..2 {
                                    let pattern_y = grid_y.checked_sub(offset_y);
                                    let pattern_x = grid_x.checked_sub(offset_x);
                                    let symbol = pattern_y
                                        .filter(|y| *y < pattern.len())
                                        .and_then(|y| {
                                            let row = pattern[y].as_bytes();
                                            pattern_x.filter(|x| *x < width).map(|x| {
                                                let x = if mirrored { width - 1 - x } else { x };
                                                row.get(x).copied().unwrap_or(b' ') as char
                                            })
                                        })
                                        .unwrap_or(' ');
                                    let slot = grid_y * 2 + grid_x;
                                    let expected = key.iter().find(|(key, _)| *key == symbol);
                                    let cell_matches = match (grid[slot], expected) {
                                        (None, None) if symbol == ' ' => true,
                                        (Some(item), Some((_, ingredient))) => {
                                            consumed[slot] = true;
                                            ingredient.match_item(item)
                                        }
                                        _ => false,
                                    };
                                    valid &= cell_matches;
                                }
                            }
                            if valid {
                                matched = Some(consumed);
                            }
                        }
                    }
                }
                let Some(consumed) = matched else { continue };
                (result, consumed)
            }
            CraftingRecipeTypes::CraftingShapeless {
                ingredients,
                result,
                ..
            } if ingredients.len() <= 4 => {
                let occupied = grid.iter().flatten().count();
                if occupied != ingredients.len() {
                    continue;
                }
                let mut used = [false; 4];
                let mut valid = true;
                for ingredient in *ingredients {
                    let Some(slot) = grid.iter().enumerate().find_map(|(slot, item)| {
                        (!used[slot] && item.is_some_and(|item| ingredient.match_item(item)))
                            .then_some(slot)
                    }) else {
                        valid = false;
                        break;
                    };
                    used[slot] = true;
                }
                if !valid {
                    continue;
                }
                (result, used)
            }
            _ => continue,
        };
        let item = pumpkin_data::item::Item::from_registry_key(result.id)?;
        return Some(CraftingResult {
            item_id: item.id,
            count: result.count,
            consumed_slots,
        });
    }
    None
}

fn refresh_crafting_output(
    player: &mut PlayerData,
    version: MinecraftVersion,
) -> std::io::Result<()> {
    player.inventory[0] = if let Some(result) = crafting_result(player, version) {
        let item = pumpkin_data::item::Item::from_id(result.item_id).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "recipe result item is missing",
            )
        })?;
        serialized_stack(item, result.count, version)?
    } else {
        Vec::new()
    };
    Ok(())
}

fn decrement_crafting_inputs(
    player: &mut PlayerData,
    version: MinecraftVersion,
    consumed_slots: [bool; 4],
) -> std::io::Result<()> {
    for (grid_index, consumed) in consumed_slots.into_iter().enumerate() {
        if !consumed {
            continue;
        }
        let slot = grid_index + 1;
        let Some((item, count)) = inventory_stack(&player.inventory[slot], version) else {
            continue;
        };
        player.inventory[slot] = if count > 1 {
            serialized_stack(item, count - 1, version)?
        } else {
            Vec::new()
        };
    }
    Ok(())
}

fn click_inventory_slot(
    player: &mut PlayerData,
    version: MinecraftVersion,
    slot: usize,
    button: i8,
) -> std::io::Result<()> {
    if slot >= player.inventory.len() || slot == 0 {
        return Ok(());
    }
    let slot_stack = player.inventory[slot].clone();
    match (
        inventory_stack(&player.carried_item, version),
        inventory_stack(&slot_stack, version),
        button,
    ) {
        (None, Some((item, count)), 1) => {
            let taken = count.div_ceil(2);
            player.carried_item = serialized_stack(item, taken, version)?;
            player.inventory[slot] = if count > taken {
                serialized_stack(item, count - taken, version)?
            } else {
                Vec::new()
            };
        }
        (Some((cursor_item, cursor_count)), None, 1) => {
            player.inventory[slot] = serialized_stack(cursor_item, 1, version)?;
            player.carried_item = if cursor_count > 1 {
                serialized_stack(cursor_item, cursor_count - 1, version)?
            } else {
                Vec::new()
            };
        }
        (Some((cursor_item, cursor_count)), Some((slot_item, slot_count)), 1)
            if cursor_item.id == slot_item.id =>
        {
            let max = pumpkin_data::item_stack::ItemStack::new(1, slot_item).get_max_stack_size();
            if slot_count < max {
                player.inventory[slot] = serialized_stack(slot_item, slot_count + 1, version)?;
                player.carried_item = if cursor_count > 1 {
                    serialized_stack(cursor_item, cursor_count - 1, version)?
                } else {
                    Vec::new()
                };
            }
        }
        (None, Some(_), _) => {
            player.carried_item = slot_stack;
            player.inventory[slot] = Vec::new();
        }
        (Some(_), None, _) => {
            player.inventory[slot] = std::mem::take(&mut player.carried_item);
        }
        (Some((cursor_item, cursor_count)), Some((slot_item, slot_count)), _)
            if cursor_item.id == slot_item.id =>
        {
            let max = pumpkin_data::item_stack::ItemStack::new(1, slot_item).get_max_stack_size();
            let moved = cursor_count.min(max.saturating_sub(slot_count));
            if moved > 0 {
                player.inventory[slot] = serialized_stack(slot_item, slot_count + moved, version)?;
                player.carried_item = if cursor_count > moved {
                    serialized_stack(cursor_item, cursor_count - moved, version)?
                } else {
                    Vec::new()
                };
            }
        }
        (Some(_), Some(_), _) => {
            player.inventory[slot] = std::mem::take(&mut player.carried_item);
            player.carried_item = slot_stack;
        }
        _ => {}
    }
    Ok(())
}

fn take_crafting_output(player: &mut PlayerData, version: MinecraftVersion) -> std::io::Result<()> {
    let Some(result) = crafting_result(player, version) else {
        return Ok(());
    };
    let item = pumpkin_data::item::Item::from_id(result.item_id).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "recipe result item is missing",
        )
    })?;
    let new_count = match inventory_stack(&player.carried_item, version) {
        None => result.count,
        Some((cursor_item, cursor_count)) if cursor_item.id == result.item_id => {
            let max = pumpkin_data::item_stack::ItemStack::new(1, item).get_max_stack_size();
            let Some(total) = cursor_count
                .checked_add(result.count)
                .filter(|count| *count <= max)
            else {
                return Ok(());
            };
            total
        }
        Some(_) => return Ok(()),
    };
    player.carried_item = serialized_stack(item, new_count, version)?;
    decrement_crafting_inputs(player, version, result.consumed_slots)?;
    refresh_crafting_output(player, version)
}

fn take_crafting_output_to_inventory(
    player: &mut PlayerData,
    version: MinecraftVersion,
) -> std::io::Result<()> {
    let Some(result) = crafting_result(player, version) else {
        return Ok(());
    };
    let item = pumpkin_data::item::Item::from_id(result.item_id).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "recipe result item is missing",
        )
    })?;
    let max = pumpkin_data::item_stack::ItemStack::new(1, item).get_max_stack_size();
    let destination = (36..45).chain(9..36).find_map(|slot| {
        match inventory_stack(&player.inventory[slot], version) {
            Some((existing, count))
                if existing.id == item.id && count.saturating_add(result.count) <= max =>
            {
                Some((slot, count + result.count))
            }
            None => Some((slot, result.count)),
            _ => None,
        }
    });
    let Some((slot, count)) = destination else {
        return Ok(());
    };
    player.inventory[slot] = serialized_stack(item, count, version)?;
    decrement_crafting_inputs(player, version, result.consumed_slots)?;
    refresh_crafting_output(player, version)
}

async fn send_inventory_content(
    stream: &mut TcpStream,
    version: MinecraftVersion,
    player: &PlayerData,
    revision: i32,
) -> std::io::Result<()> {
    let mut payload = Vec::new();
    write_varint(&mut payload, 0);
    write_varint(&mut payload, revision);
    write_varint(&mut payload, player.inventory.len() as i32);
    for slot in &player.inventory {
        if slot.is_empty() {
            write_varint(&mut payload, 0);
        } else {
            payload.extend_from_slice(slot);
        }
    }
    if player.carried_item.is_empty() {
        write_varint(&mut payload, 0);
    } else {
        payload.extend_from_slice(&player.carried_item);
    }
    let mut packet = Vec::new();
    write_varint(
        &mut packet,
        pumpkin_data::packet::clientbound::PLAY_CONTAINER_SET_CONTENT.to_id(version),
    );
    packet.extend_from_slice(&payload);
    write_framed_payload(stream, packet.as_slice()).await
}

fn held_inventory_slot(player: &PlayerData, hand: i32) -> Option<usize> {
    Some(match hand {
        0 if player.held_slot < HOTBAR_SLOT_COUNT => HOTBAR_START_SLOT + player.held_slot as usize,
        1 => PLAYER_INVENTORY_SLOTS - 1,
        _ => return None,
    })
}

fn held_block_state(player: &PlayerData, hand: i32, version: MinecraftVersion) -> Option<u16> {
    let inventory_index = held_inventory_slot(player, hand)?;
    let slot = player.inventory.get(inventory_index)?;
    let item_id = inventory_item_id(slot, version)?;
    pumpkin_data::Block::from_item_id(item_id).map(|block| block.default_state.id)
}

fn remove_inventory_items(
    player: &mut PlayerData,
    slot: usize,
    requested: u8,
    version: MinecraftVersion,
) -> std::io::Result<Option<(&'static pumpkin_data::item::Item, u8, u8)>> {
    let Some((item, count)) = player
        .inventory
        .get(slot)
        .and_then(|stack| inventory_stack(stack, version))
    else {
        return Ok(None);
    };
    let removed = requested.min(count);
    let remaining = count - removed;
    player.inventory[slot] = if remaining == 0 {
        Vec::new()
    } else {
        serialized_stack(item, remaining, version)?
    };
    Ok(Some((item, removed, remaining)))
}

async fn send_inventory_slot(
    stream: &mut TcpStream,
    version: MinecraftVersion,
    slot: usize,
    item: &'static pumpkin_data::item::Item,
    count: u8,
) -> std::io::Result<()> {
    let stack = if count == 0 {
        pumpkin_data::item_stack::ItemStack::EMPTY.clone()
    } else {
        pumpkin_data::item_stack::ItemStack::new(count, item)
    };
    let serialized = ItemStackSerializer::from(stack);
    let update = CSetContainerSlot::new(0, 0, slot as i16, &serialized);
    let payload = encode_java_packet(&update, version)?;
    write_framed_payload(stream, payload.as_slice()).await
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
    let sv_container_click = pumpkin_data::packet::serverbound::PLAY_CONTAINER_CLICK.to_id(version);

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

                if player.gamemode != 1 {
                    if let Some(slot) = held_inventory_slot(player, pkt.hand.0) {
                        if let Some((item, _, remaining)) =
                            remove_inventory_items(player, slot, 1, version)?
                        {
                            send_inventory_slot(stream, version, slot, item, remaining).await?;
                        }
                    }
                }
            }

            if pkt.sequence.0 > 0 {
                let ack = CAcknowledgeBlockChange::new(pkt.sequence);
                let ack_payload = encode_java_packet(&ack, version)?;
                write_framed_payload(stream, ack_payload.as_slice()).await?;
            }
        }
    } else if pid == sv_container_click {
        use pumpkin_protocol::java::server::play::{SClickSlot, SlotActionType};
        if let Ok(packet) = SClickSlot::read(&mut std::io::Cursor::new(payload), &version) {
            if packet.sync_id.0 == 0 {
                match packet.mode {
                    SlotActionType::Pickup if packet.slot == 0 => {
                        take_crafting_output(player, version)?;
                    }
                    SlotActionType::Pickup if packet.slot > 0 => {
                        click_inventory_slot(player, version, packet.slot as usize, packet.button)?;
                        refresh_crafting_output(player, version)?;
                    }
                    SlotActionType::QuickMove if packet.slot == 0 => {
                        take_crafting_output_to_inventory(player, version)?;
                    }
                    _ => {}
                }
                send_inventory_content(stream, version, player, packet.revision.0 + 1).await?;
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
                    if let Some(item) = block_drop_item(broken_state) {
                        spawn_dropped_item(
                            item.id,
                            1,
                            x as f64 + 0.5,
                            y as f64 + 0.5,
                            z as f64 + 0.5,
                            0.0,
                            0.2,
                            0.0,
                        );
                    }
                }
            } else if status == 3 || status == 4 {
                let slot = HOTBAR_START_SLOT + player.held_slot.min(8) as usize;
                let requested = if status == 3 { 1 } else { u8::MAX };
                if let Some((item, removed, remaining)) =
                    remove_inventory_items(player, slot, requested, version)?
                {
                    let yaw_rad = (player.yaw + 90.0) * (std::f32::consts::PI / 180.0);
                    let pitch_rad = -player.pitch * (std::f32::consts::PI / 180.0);
                    let vx = (yaw_rad.cos() * pitch_rad.cos() * 0.3) as f64;
                    let vy = (pitch_rad.sin() * 0.3 + 0.1) as f64;
                    let vz = (yaw_rad.sin() * pitch_rad.cos() * 0.3) as f64;
                    spawn_dropped_item(
                        item.id,
                        removed,
                        player.x,
                        player.y + 1.5,
                        player.z,
                        vx,
                        vy,
                        vz,
                    );
                    send_inventory_slot(stream, version, slot, item, remaining).await?;
                }
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
        } else if parts[0] == "give" && matches!(parts.len(), 2 | 3) {
            if player.operator_level == 0 {
                send_system_message(
                    stream,
                    version,
                    "You do not have permission to use this command.",
                )
                .await?;
                return Ok(());
            }
            let item_name = parts[1].strip_prefix("minecraft:").unwrap_or(parts[1]);
            let Some(item) = pumpkin_data::item::Item::from_registry_key(item_name) else {
                send_system_message(stream, version, "Unknown item.").await?;
                return Ok(());
            };
            let requested_count = parts
                .get(2)
                .and_then(|count| count.parse::<u8>().ok())
                .unwrap_or(1);
            let max_count = pumpkin_data::item_stack::ItemStack::new(1, item).get_max_stack_size();
            if requested_count == 0 || requested_count > max_count {
                send_system_message(
                    stream,
                    version,
                    &format!("Count must be between 1 and {max_count}."),
                )
                .await?;
                return Ok(());
            }
            if store_inventory_item(stream, version, player, item.id, requested_count).await? {
                send_system_message(
                    stream,
                    version,
                    &format!("Gave {requested_count} {}.", item.registry_key),
                )
                .await?;
                log_info!("{}: /{}", username, cmd);
            } else {
                send_system_message(stream, version, "Your inventory is full.").await?;
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
    fn held_crafting_table_resolves_to_the_crafting_table_block() {
        let version = MinecraftVersion::V_26_1;
        let mut player = PlayerData::default();
        player.inventory[HOTBAR_START_SLOT] =
            serialized_stack(&pumpkin_data::item::Item::CRAFTING_TABLE, 1, version).unwrap();

        assert_eq!(
            held_block_state(&player, 0, version),
            Some(pumpkin_data::Block::CRAFTING_TABLE.default_state.id)
        );
    }

    #[test]
    fn removing_items_updates_the_authoritative_stack() {
        let version = MinecraftVersion::V_26_1;
        let mut player = PlayerData::default();
        let slot = HOTBAR_START_SLOT + 3;
        player.inventory[slot] =
            serialized_stack(&pumpkin_data::item::Item::DIRT, 2, version).unwrap();

        let (_, removed, remaining) = remove_inventory_items(&mut player, slot, 1, version)
            .unwrap()
            .unwrap();
        assert_eq!((removed, remaining), (1, 1));
        assert_eq!(
            inventory_stack(&player.inventory[slot], version).map(|(item, count)| (item.id, count)),
            Some((pumpkin_data::item::Item::DIRT.id, 1))
        );

        let (_, removed, remaining) = remove_inventory_items(&mut player, slot, u8::MAX, version)
            .unwrap()
            .unwrap();
        assert_eq!((removed, remaining), (1, 0));
        assert!(player.inventory[slot].is_empty());
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

    #[test]
    fn starter_recipes_work_in_the_player_crafting_grid() {
        let version = MinecraftVersion::V_26_1;
        let mut player = PlayerData::default();
        player.inventory[1] =
            serialized_stack(&pumpkin_data::item::Item::OAK_LOG, 1, version).unwrap();
        assert_eq!(
            crafting_result(&player, version).map(|result| (result.item_id, result.count)),
            Some((pumpkin_data::item::Item::OAK_PLANKS.id, 4))
        );

        for slot in 1..=4 {
            player.inventory[slot] =
                serialized_stack(&pumpkin_data::item::Item::OAK_PLANKS, 1, version).unwrap();
        }
        assert_eq!(
            crafting_result(&player, version).map(|result| (result.item_id, result.count)),
            Some((pumpkin_data::item::Item::CRAFTING_TABLE.id, 1))
        );

        player.inventory[1] =
            serialized_stack(&pumpkin_data::item::Item::OAK_PLANKS, 1, version).unwrap();
        player.inventory[2].clear();
        player.inventory[3] =
            serialized_stack(&pumpkin_data::item::Item::OAK_PLANKS, 1, version).unwrap();
        player.inventory[4].clear();
        assert_eq!(
            crafting_result(&player, version).map(|result| (result.item_id, result.count)),
            Some((pumpkin_data::item::Item::STICK.id, 4))
        );

        take_crafting_output(&mut player, version).unwrap();
        assert_eq!(
            inventory_stack(&player.carried_item, version).map(|(item, count)| (item.id, count)),
            Some((pumpkin_data::item::Item::STICK.id, 4))
        );
        assert!(player.inventory[1].is_empty());
        assert!(player.inventory[3].is_empty());
        assert!(player.inventory[0].is_empty());
    }
}
