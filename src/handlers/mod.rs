use bevy_ecs::prelude::*;
use serde::{Deserialize, Serialize};
use std::io::Write;
use flate2::write::GzEncoder;
use flate2::Compression;
use uuid::Uuid;

use crate::WorldDb;
use crate::logger::{log_error, log_warn};

#[derive(Component, Serialize, Deserialize, Clone, Debug)]
pub struct Position {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

#[derive(Component, Serialize, Deserialize, Clone, Debug)]
pub struct Health(pub f32);

#[derive(Component, Serialize, Deserialize, Clone, Debug)]
pub struct XP(pub i32);

#[derive(Component, Serialize, Deserialize, Clone, Debug)]
pub struct Inventory(pub Vec<u8>);

#[derive(Component, Serialize, Deserialize, Clone, Debug)]
pub struct Saturation(pub f32);

#[derive(Component, Clone)]
pub struct PlayerUuid(pub Uuid);

#[derive(Component)]
pub struct Disconnected;

#[derive(Component)]
pub struct PlayerJoinEvent(pub Uuid, pub String);

#[derive(Serialize, Deserialize)]
struct PlayerSaveData {
    pub position: Position,
    pub health: Health,
    pub xp: XP,
    pub inventory: Inventory,
    pub saturation: Saturation,
}

pub fn handle_player_join(
    mut commands: Commands,
    query: Query<(Entity, &PlayerJoinEvent), Added<PlayerJoinEvent>>,
) {
    for (entity, join_event) in query.iter() {
        // Spawn full player into the ECS
        commands.entity(entity).insert((
            PlayerUuid(join_event.0),
            Position { x: 0.0, y: 70.0, z: 0.0 }, // Spawn coords
            Health(20.0),
            XP(0),
            Inventory(vec![]),
            Saturation(20.0),
        )).remove::<PlayerJoinEvent>();
    }
}

pub fn handle_disconnect(
    mut commands: Commands,
    query: Query<(
        Entity,
        &PlayerUuid,
        &Position,
        &Health,
        &XP,
        &Inventory,
        &Saturation,
    ), With<Disconnected>>,
    db: Res<WorldDb>,
) {
    let mut wrote_any = false;

    for (entity, uuid, pos, hp, xp, inv, sat) in query.iter() {
        let save_data = PlayerSaveData {
            position: pos.clone(),
            health: hp.clone(),
            xp: xp.clone(),
            inventory: inv.clone(),
            saturation: sat.clone(),
        };

        // Serialize via Postcard
        if let Ok(serialized) = postcard::to_allocvec(&save_data) {
            // Compress via Gzip
            let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
            if encoder.write_all(&serialized).is_ok() {
                if let Ok(compressed) = encoder.finish() {
                    let key = format!("player:{}", uuid.0);
                    match db.0.insert(key.as_bytes(), compressed) {
                        Ok(_) => wrote_any = true,
                        Err(e) => log_error!("Failed to persist disconnected player {}: {}", uuid.0, e),
                    }
                }
            }
        }
        
        commands.entity(entity).despawn();
    }

    if wrote_any {
        if let Err(e) = db.0.flush() {
            log_warn!("Failed to flush disconnected player persistence batch: {}", e);
        }
    }
}

pub fn handle_chat_and_commands() {
}

pub fn handle_chunk_request() {
}

pub fn handle_block_break() {
}

pub fn handle_take_damage() {
}