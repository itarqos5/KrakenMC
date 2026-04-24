use std::sync::Arc;
use flume::{Receiver, Sender};
use noise::{NoiseFn, Perlin};
use valence::prelude::*;

const WORLD_HEIGHT: u32 = 384;
const MIN_Y: i32 = -64;
const SEA_LEVEL: i32 = 62;

pub struct ChunkWorkerState {
    pub sender: Sender<(ChunkPos, UnloadedChunk)>,
    pub receiver: Receiver<ChunkPos>,
    pub terrain: Perlin,
    pub min_y: i32,
    pub height: u32,
}

pub fn chunk_worker(state: Arc<ChunkWorkerState>) {
    let exe_dir = std::env::current_exe().unwrap().parent().unwrap().to_path_buf();
    let world_dir = exe_dir.join("world");

    while let Ok(pos) = state.receiver.recv() {
        let mut chunk = UnloadedChunk::with_height(state.height);

        // Pre-generation (limit to 8 chunks)
        // Just prototype
        if pos.x >= 0 && pos.x < 4 && pos.z >= 0 && pos.z < 2 {
            let chunk_file_path = world_dir.join(format!("chunk_{}_{}.bin", pos.x, pos.z));
            for z in 0..16u32 {
                for x in 0..16u32 {
                    let world_x = pos.x * 16 + x as i32;
                    let world_z = pos.z * 16 + z as i32;
                    let noise_val = state.terrain.get([world_x as f64 / 150.0, world_z as f64 / 150.0]); // Smoother for mountains
                    let world_height = (70.0 + noise_val * 40.0) as i32; // Higher base and larger variation
                    let top_y = (world_height - state.min_y).clamp(0, state.height as i32 - 1) as u32;

                    // Deepslate Geology Layer
                    for y in 0..=top_y.min(state.height / 3) {
                        chunk.set_block(x, y, z, BlockState::DEEPSLATE);
                    }
                    if state.min_y > -64 {
                        let bedrock_noise = state.terrain.get([world_x as f64 * 4.0, world_z as f64 * 4.0]);
                        if bedrock_noise > -0.5 {
                             chunk.set_block(x, 0, z, BlockState::BEDROCK);
                        }
                    }

                    for y in top_y.min(state.height / 3) + 1..=top_y {
                        let block = if y == top_y { BlockState::GRASS_BLOCK } else if y + 3 >= top_y { BlockState::DIRT } else { BlockState::STONE };
                        chunk.set_block(x, y, z, block);
                    }
                    let water_top = (SEA_LEVEL - state.min_y).clamp(0, state.height as i32 - 1) as u32;
                    if water_top > top_y {
                        for y in (top_y + 1)..=water_top {
                            chunk.set_block(x, y, z, BlockState::WATER);
                        }
                        // Replace underwater grass with dirt or sand/gravel
                        chunk.set_block(x, top_y, z, BlockState::DIRT);
                    } else if top_y > water_top + 1 {
                        // Chance to spawn a tree
                        // Very naive pseudo-random check
                        let tree_noise = state.terrain.get([world_x as f64 * 3.14, world_z as f64 * 2.71]);
                        if tree_noise > 0.8 && top_y + 6 < state.height {
                            // Trunk
                            for ty in 1..=4 {
                                chunk.set_block(x, top_y + ty, z, BlockState::OAK_LOG);
                            }
                            // Leaves
                            for ly in 3..=5 {
                                for lx in x.saturating_sub(1)..=(x + 1).min(15) {
                                    for lz in z.saturating_sub(1)..=(z + 1).min(15) {
                                        if chunk.block(lx, top_y + ly, lz).state.is_air() {
                                            chunk.set_block(lx, top_y + ly, lz, BlockState::OAK_LEAVES);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            let _ = std::fs::write(&chunk_file_path, "KRAKEN_PROTOTYPE_CHUNK_DATA");
        }
        chunk.shrink_to_fit();
        let _ = state.sender.send((pos, chunk));
    }
}
