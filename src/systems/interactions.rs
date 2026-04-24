use valence::prelude::*;
use valence::action::{DiggingEvent, DiggingState};
use valence::interact_block::InteractBlockEvent;
use valence::inventory::HeldItem;
use valence::entity::item::{ItemEntityBundle, Stack};
use valence::entity::{Position, Velocity};

use crate::WorldDb;

fn store_override(db: &sled::Db, pos: BlockPos, block: BlockState) {
    let cx = pos.x >> 4;
    let cz = pos.z >> 4;
    let key = format!("c_{}_{}", cx, cz);
    let mut overrides: Vec<((i32, i32, i32), u16)> = vec![];
    if let Ok(Some(data)) = db.get(&key) {
        if let Ok(existing) = postcard::from_bytes(&data) {
            overrides = existing;
        }
    }
    // Remove existing block at this position if any
    overrides.retain(|(p, _)| *p != (pos.x, pos.y, pos.z));
    overrides.push(((pos.x, pos.y, pos.z), block.to_raw()));
    if let Ok(encoded) = postcard::to_allocvec(&overrides) {
        let _ = db.insert(key, encoded);
    }
}

pub fn interact_blocks(
    mut commands: Commands,
    mut chunk_layer: Query<&mut ChunkLayer>,
    clients: Query<(&GameMode, &Inventory, &HeldItem)>,
    mut interact: EventReader<InteractBlockEvent>,
    mut dig: EventReader<DiggingEvent>,
    world_db: Res<WorldDb>,
) {
    let mut layer = match chunk_layer.get_single_mut() {
        Ok(l) => l,
        Err(_) => return,
    };

    for ev in interact.read() {
        if let Ok((_game_mode, inventory, held)) = clients.get(ev.client) {
            let item = inventory.slot(held.slot());
            if let Some(block_kind) = BlockKind::from_item_kind(item.item) {
                let pos = ev.position.get_in_direction(ev.face);
                let block = block_kind.to_state();
                layer.set_block(pos, block);
                store_override(&world_db.0, pos, block);
            }
        }
    }

    for ev in dig.read() {
        if let Ok((game_mode, _, _)) = clients.get(ev.client) {
            if (ev.state == DiggingState::Start && *game_mode == GameMode::Creative) || ev.state == DiggingState::Stop {
                let pos = ev.position;
                if let Some(b) = layer.block(pos) {
                    if b.state == BlockState::TNT {
                        for x in -1..=1 {
                            for y in -1..=1 {
                                for z in -1..=1 {
                                    let explode_pos = BlockPos::new(pos.x + x, pos.y + y, pos.z + z);
                                    layer.set_block(explode_pos, BlockState::AIR);
                                    store_override(&world_db.0, explode_pos, BlockState::AIR);
                                }
                            }
                        }
                    } else {
                        // Drop it
                        let item_kind = b.state.to_kind().to_item_kind();
                        if item_kind != ItemKind::Air {
                            if *game_mode != GameMode::Creative {
                                commands.spawn(ItemEntityBundle {
                                    item_stack: Stack(ItemStack::new(item_kind, 1, None)),
                                    position: Position::new([
                                        pos.x as f64 + 0.5,
                                        pos.y as f64 + 0.5,
                                        pos.z as f64 + 0.5,
                                    ]),
                                    velocity: Velocity([0.0, 0.2, 0.0].into()),
                                    ..Default::default()
                                });
                            }
                        }

                        layer.set_block(pos, BlockState::AIR);
                        store_override(&world_db.0, pos, BlockState::AIR);
                    }
                }
            }
        }
    }
}
