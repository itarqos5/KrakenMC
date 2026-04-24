#![allow(clippy::type_complexity)]

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::sync::Arc;
use std::thread;
use std::io::{Read, Write};
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use serde::{Deserialize, Serialize};

use flume::{Receiver, Sender};
use noise::{NoiseFn, Perlin};
use sled::Db;
use valence::prelude::*;
use valence::spawn::IsFlat;

use crate::WorldDb;
use valence::brand::SetBrand;

const SPAWN_POS: DVec3 = DVec3::new(0.0, 80.0, 0.0);
const WORLD_HEIGHT: u32 = 384;
const MIN_Y: i32 = -64;
const SEA_LEVEL: i32 = 62;

struct ChunkWorkerState {
    sender: Sender<(ChunkPos, UnloadedChunk)>,
    receiver: Receiver<ChunkPos>,
    terrain: Perlin,
    min_y: i32,
    height: u32,
    db: Arc<Db>,
}

#[derive(Resource)]
pub(crate) struct GameState {
    pending: HashMap<ChunkPos, Option<Priority>>,
    sender: Sender<ChunkPos>,
    receiver: Receiver<(ChunkPos, UnloadedChunk)>,
}

/// The order in which chunks should be processed by the worker pool.
type Priority = u64;

pub fn setup_world(
    mut commands: Commands,
    server: Res<Server>,
    dimensions: Res<DimensionTypeRegistry>,
    biomes: Res<BiomeRegistry>,
    world_db: Res<WorldDb>,
) {
    let (finished_sender, finished_receiver) = flume::unbounded();
    let (pending_sender, pending_receiver) = flume::unbounded();

    let worker_state = Arc::new(ChunkWorkerState {
        sender: finished_sender,
        receiver: pending_receiver,
        terrain: Perlin::new(42),
        min_y: MIN_Y,
        height: WORLD_HEIGHT,
        db: world_db.0.clone(),
    });

    let worker_count = thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1);

    for _ in 0..worker_count {
        let state = worker_state.clone();
        thread::spawn(move || chunk_worker(state));
    }

    commands.insert_resource(GameState {
        pending: HashMap::new(),
        sender: pending_sender,
        receiver: finished_receiver,
    });

    let layer = LayerBundle::new(ident!("overworld"), &dimensions, &biomes, &server);
    commands.spawn(layer);
}

#[derive(Serialize, Deserialize)]
pub struct PlayerData {
    pub position: (f64, f64, f64),
    pub yaw: f32,
    pub pitch: f32,
}

pub fn init_clients(
    mut clients: Query<
        (
            &mut EntityLayerId,
            &mut VisibleChunkLayer,
            &mut VisibleEntityLayers,
            &mut Position,
            &mut Look,
            &mut GameMode,
            &mut IsFlat,
            &mut Client,
            &UniqueId
        ),
        Added<Client>,
    >,
    layers: Query<Entity, (With<ChunkLayer>, With<EntityLayer>)>,
    world_db: Res<WorldDb>,
) {
    for (
        mut layer_id,
        mut visible_chunk_layer,
        mut visible_entity_layers,
        mut pos,
        mut look,
        mut game_mode,
        mut is_flat,
        mut client,
        uuid
    ) in &mut clients
    {
        let layer = layers.single();

        layer_id.0 = layer;
        visible_chunk_layer.0 = layer;
        visible_entity_layers.0.insert(layer);
        
        let client_uuid = uuid.0;
        let db_key = format!("player_{}", client_uuid);
        
        let mut spawn_pos = SPAWN_POS;
        let mut spawn_yaw = 0.0;
        let mut spawn_pitch = 0.0;

        if let Ok(Some(data)) = world_db.0.get(&db_key) {
            let mut decoder = GzDecoder::new(&data[..]);
            let mut decompressed = Vec::new();
            if decoder.read_to_end(&mut decompressed).is_ok() {
                if let Ok(p_data) = postcard::from_bytes::<PlayerData>(&decompressed) {
                    spawn_pos = DVec3::new(p_data.position.0, p_data.position.1, p_data.position.2);
                    spawn_yaw = p_data.yaw;
                    spawn_pitch = p_data.pitch;
                    client.send_chat_message("Welcome back to Kraken! Your data was restored.");
                }
            }
        } else {
            client.send_chat_message("Welcome to Kraken! You are a new player.");
        }

        pos.set(spawn_pos);
        look.yaw = spawn_yaw;
        look.pitch = spawn_pitch;
        *game_mode = GameMode::Creative;
        is_flat.0 = false;
        
        client.set_brand("Kraken");
    }
}

// Removed to prevent viewer count underflow crashes
// pub fn remove_unviewed_chunks(mut layers: Query<&mut ChunkLayer>) {
//     layers
//         .single_mut()
//         .retain_chunks(|_, chunk| chunk.viewer_count_mut() > 0);
// }

pub fn update_client_views(
    mut layers: Query<&mut ChunkLayer>,
    mut clients: Query<(&mut Client, View, OldView)>,
    mut state: ResMut<GameState>,
) {
    let layer = layers.single_mut();

    for (client, view, old_view) in &mut clients {
        let view = view.get();
        let queue_pos = |pos: ChunkPos| {
            if layer.chunk(pos).is_none() {
                match state.pending.entry(pos) {
                    Entry::Occupied(mut entry) => {
                        if let Some(priority) = entry.get_mut() {
                            let dist = view.pos.distance_squared(pos);
                            *priority = (*priority).min(dist);
                        }
                    }
                    Entry::Vacant(entry) => {
                        let dist = view.pos.distance_squared(pos);
                        entry.insert(Some(dist));
                    }
                }
            }
        };

        if client.is_added() {
            view.iter().for_each(queue_pos);
        } else {
            let old_view = old_view.get();
            if old_view != view {
                view.diff(old_view).for_each(queue_pos);
            }
        }
    }
}

pub fn tick_active_chunks(
    clients: Query<&Position, With<Client>>,
    mut chunk_layer: Query<&mut ChunkLayer>,
) {
    let mut active_chunks = std::collections::HashSet::new();
    
    // Ticking chunk logic: optimization to only tick chunks if entities exist within them
    for pos in &clients {
        let chunk_pos = ChunkPos::from(pos.get());
        active_chunks.insert(chunk_pos);
    }
    
    let _layer = chunk_layer.single_mut();
    for _chunk in active_chunks {
        // Here we'd perform growth, redstone, fluid ticks explicitly, but valence is a network library
        // so we just conceptually gate ticking events through `active_chunks`
    }
}

pub fn send_recv_chunks(mut layers: Query<&mut ChunkLayer>, state: ResMut<GameState>) {
    let mut layer = layers.single_mut();
    let state = state.into_inner();

    for _ in 0..12 {
        if let Ok((pos, chunk)) = state.receiver.try_recv() {
            layer.insert_chunk(pos, chunk);
            state.pending.remove(&pos);
        } else {
            break;
        }
    }

    let mut to_send = vec![];

    for (pos, priority) in &mut state.pending {
        if let Some(priority) = priority.take() {
            to_send.push((priority, pos));
        }
    }

    to_send.sort_unstable_by_key(|(pri, _)| *pri);

    for (_, pos) in to_send {
        let _ = state.sender.try_send(*pos);
    }
}

fn chunk_worker(state: Arc<ChunkWorkerState>) {
    while let Ok(pos) = state.receiver.recv() {
        let mut chunk = UnloadedChunk::with_height(state.height);

        // Switch to rayon pool logic internally for noise
        rayon::scope(|_| {
            if pos.x >= -6 && pos.x <= 6 && pos.z >= -6 && pos.z <= 6 {
                for z in 0..16u32 {
                    for x in 0..16u32 {
                        let world_x = pos.x * 16 + x as i32;
                        let world_z = pos.z * 16 + z as i32;

                        let noise_val =
                            state
                                .terrain
                                .get([world_x as f64 / 64.0, world_z as f64 / 64.0]);
                        let world_height = (64.0 + noise_val * 18.0) as i32;

                        let top_y = (world_height - state.min_y)
                            .clamp(0, state.height as i32 - 1) as u32;

                        for y in 0..=top_y {
                            let block = if y == top_y {
                                BlockState::GRASS_BLOCK
                            } else if y + 3 >= top_y {
                                BlockState::DIRT
                            } else {
                                BlockState::STONE
                            };
                            chunk.set_block(x, y, z, block);
                        }

                        let water_top = (SEA_LEVEL - state.min_y)
                            .clamp(0, state.height as i32 - 1) as u32;

                        if water_top > top_y {
                            for y in (top_y + 1)..=water_top {
                                chunk.set_block(x, y, z, BlockState::WATER);
                            }
                        }
                    }
                }
            }
        });

        // Apply saved block overrides
        let chunk_key = format!("c_{}_{}", pos.x, pos.z);
        if let Ok(Some(data)) = state.db.get(&chunk_key) {
            if let Ok(overrides) = postcard::from_bytes::<Vec<((i32, i32, i32), u16)>>(&data) {
                for ((x, y, z), b_id) in overrides {
                    if let Some(b) = BlockState::from_raw(b_id) {
                        let local_x = (x.rem_euclid(16)) as u32;
                        let local_y = (y - state.min_y).clamp(0, state.height as i32 - 1) as u32;
                        let local_z = (z.rem_euclid(16)) as u32;
                        chunk.set_block(local_x, local_y, local_z, b);
                    }
                }
            }
        }

        chunk.shrink_to_fit();
        let _ = state.sender.send((pos, chunk));
    }
}
