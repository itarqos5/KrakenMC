use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex, OnceLock};

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use noise::{NoiseFn, Perlin};
use pumpkin_util::version::MinecraftVersion;
use serde::{Deserialize, Serialize};

const CHUNK_FORMAT_VERSION: u8 = 2;
const SECTION_COUNT: usize = 24;
const MIN_Y: i32 = -64;
const MAX_Y: i32 = 319;
const CHUNK_WIDTH: usize = 16;
const SECTION_HEIGHT: usize = 16;
const BLOCKS_PER_SECTION: usize = CHUNK_WIDTH * CHUNK_WIDTH * SECTION_HEIGHT;
const BLOCKS_PER_CHUNK: usize = BLOCKS_PER_SECTION * SECTION_COUNT;
const MAX_CACHED_CHUNKS: usize = 128;
const MAX_CACHED_CHUNK_PACKETS: usize = 512;
const DEFAULT_WORLD_SEED: i64 = 42;

type ChunkKey = (usize, i32, i32);
type ChunkPacketKey = (usize, i32, i32, i32);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SavedChunk {
    format_version: u8,
    blocks: Vec<u16>,
    biomes: Vec<u8>,
    temperatures: Vec<i16>,
    surface_y: Vec<i16>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TerrainBiome {
    Ocean,
    SnowyPlains,
    Taiga,
    Forest,
    Plains,
    Savanna,
    Desert,
}

#[derive(Clone, Copy)]
struct ChunkPosition {
    x: i32,
    z: i32,
}

#[derive(Clone, Copy)]
struct BlockPosition {
    x: i32,
    y: i32,
    z: i32,
}

impl TerrainBiome {
    fn protocol_id(self) -> u8 {
        match self {
            Self::Ocean => pumpkin_data::biome::Biome::OCEAN.id,
            Self::SnowyPlains => pumpkin_data::biome::Biome::SNOWY_PLAINS.id,
            Self::Taiga => pumpkin_data::biome::Biome::TAIGA.id,
            Self::Forest => pumpkin_data::biome::Biome::FOREST.id,
            Self::Plains => pumpkin_data::biome::Biome::PLAINS.id,
            Self::Savanna => pumpkin_data::biome::Biome::SAVANNA.id,
            Self::Desert => pumpkin_data::biome::Biome::DESERT.id,
        }
    }

    fn tree_density(self) -> u64 {
        match self {
            Self::Ocean => 0,
            Self::Forest => 72,
            Self::Taiga => 58,
            Self::Plains => 12,
            Self::Savanna => 28,
            Self::SnowyPlains => 5,
            Self::Desert => 0,
        }
    }
}

fn chunk_cache() -> &'static Mutex<HashMap<ChunkKey, Arc<SavedChunk>>> {
    static CACHE: OnceLock<Mutex<HashMap<ChunkKey, Arc<SavedChunk>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn chunk_packet_cache() -> &'static Mutex<HashMap<ChunkPacketKey, Arc<Vec<u8>>>> {
    static CACHE: OnceLock<Mutex<HashMap<ChunkPacketKey, Arc<Vec<u8>>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn database_id(db: &Arc<sled::Db>) -> usize {
    Arc::as_ptr(db) as usize
}

fn write_vi(buf: &mut Vec<u8>, value: i32) {
    let mut value = value as u32;
    loop {
        if value & !0x7f == 0 {
            buf.push(value as u8);
            break;
        }
        buf.push(((value & 0x7f) as u8) | 0x80);
        value >>= 7;
    }
}

fn block_index(local_x: usize, y: i32, local_z: usize) -> Option<usize> {
    if !(MIN_Y..=MAX_Y).contains(&y) || local_x >= CHUNK_WIDTH || local_z >= CHUNK_WIDTH {
        return None;
    }
    Some((y - MIN_Y) as usize * 256 + local_z * 16 + local_x)
}

fn column_index(local_x: usize, local_z: usize) -> usize {
    local_z * CHUNK_WIDTH + local_x
}

fn chunk_storage_key(chunk_x: i32, chunk_z: i32) -> [u8; 8] {
    let mut key = [0u8; 8];
    key[..4].copy_from_slice(&chunk_x.to_be_bytes());
    key[4..].copy_from_slice(&chunk_z.to_be_bytes());
    key
}

fn world_seed(db: &Arc<sled::Db>) -> i64 {
    let Ok(metadata) = db.open_tree("world_metadata") else {
        return DEFAULT_WORLD_SEED;
    };
    if let Ok(Some(value)) = metadata.get(b"seed") {
        if value.len() == 8 {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&value);
            return i64::from_be_bytes(bytes);
        }
    }
    let _ = metadata.insert(b"seed", &DEFAULT_WORLD_SEED.to_be_bytes());
    DEFAULT_WORLD_SEED
}

fn encode_saved_chunk(chunk: &SavedChunk) -> Option<Vec<u8>> {
    let serialized = postcard::to_allocvec(chunk).ok()?;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(&serialized).ok()?;
    encoder.finish().ok()
}

fn decode_saved_chunk(bytes: &[u8]) -> Option<SavedChunk> {
    let mut decoder = GzDecoder::new(bytes);
    let mut decoded = Vec::new();
    decoder.read_to_end(&mut decoded).ok()?;
    let chunk: SavedChunk = postcard::from_bytes(&decoded).ok()?;
    if chunk.format_version != CHUNK_FORMAT_VERSION
        || chunk.blocks.len() != BLOCKS_PER_CHUNK
        || chunk.biomes.len() != 256
        || chunk.temperatures.len() != 256
        || chunk.surface_y.len() != 256
    {
        return None;
    }
    Some(chunk)
}

pub fn get_or_generate_chunk(db: &Arc<sled::Db>, chunk_x: i32, chunk_z: i32) -> Arc<SavedChunk> {
    let cache_key = (database_id(db), chunk_x, chunk_z);
    if let Ok(cache) = chunk_cache().lock() {
        if let Some(chunk) = cache.get(&cache_key) {
            return Arc::clone(chunk);
        }
    }

    let storage_key = chunk_storage_key(chunk_x, chunk_z);
    let chunk = db
        .open_tree("saved_chunks_v1")
        .ok()
        .and_then(|tree| tree.get(storage_key).ok().flatten())
        .and_then(|bytes| decode_saved_chunk(&bytes))
        .unwrap_or_else(|| {
            let generated = generate_chunk(chunk_x, chunk_z, world_seed(db));
            if let (Ok(tree), Some(bytes)) = (
                db.open_tree("saved_chunks_v1"),
                encode_saved_chunk(&generated),
            ) {
                let _ = tree.insert(storage_key, bytes);
            }
            generated
        });

    let chunk = Arc::new(chunk);
    if let Ok(mut cache) = chunk_cache().lock() {
        if cache.len() >= MAX_CACHED_CHUNKS {
            cache.clear();
        }
        cache.insert(cache_key, Arc::clone(&chunk));
    }
    chunk
}

pub fn save_block_change(db: &Arc<sled::Db>, x: i32, y: i32, z: i32, state_id: u16) {
    if !(MIN_Y..=MAX_Y).contains(&y) {
        return;
    }
    let chunk_x = x >> 4;
    let chunk_z = z >> 4;
    let Ok(tree) = db.open_tree("chunk_mods") else {
        return;
    };
    let key = format!("{chunk_x},{chunk_z}");
    let mut modifications = get_chunk_mods(db, chunk_x, chunk_z);
    let index = block_index((x & 15) as usize, y, (z & 15) as usize).unwrap() as u32;
    modifications.insert(index, state_id);
    if let Ok(bytes) = postcard::to_allocvec(&modifications) {
        let _ = tree.insert(key.as_bytes(), bytes);
    }

    let db_id = database_id(db);
    if let Ok(mut cache) = chunk_packet_cache().lock() {
        cache.retain(|(cached_db, cached_x, cached_z, _), _| {
            *cached_db != db_id || *cached_x != chunk_x || *cached_z != chunk_z
        });
    }
}

pub fn get_chunk_mods(db: &Arc<sled::Db>, chunk_x: i32, chunk_z: i32) -> HashMap<u32, u16> {
    if let Ok(tree) = db.open_tree("chunk_mods") {
        let key = format!("{chunk_x},{chunk_z}");
        if let Ok(Some(bytes)) = tree.get(key.as_bytes()) {
            if let Ok(modifications) = postcard::from_bytes::<HashMap<u32, u16>>(&bytes) {
                return modifications;
            }
            // Migrate the original u16-key format when an existing world is first loaded.
            if let Ok(old_modifications) = postcard::from_bytes::<HashMap<u16, u16>>(&bytes) {
                return old_modifications
                    .into_iter()
                    .map(|(index, state)| (index as u32, state))
                    .collect();
            }
        }
    }
    HashMap::new()
}

pub fn get_block_state(db: &Arc<sled::Db>, x: i32, y: i32, z: i32) -> u16 {
    let Some(index) = block_index((x & 15) as usize, y, (z & 15) as usize) else {
        return pumpkin_data::Block::AIR.default_state.id;
    };
    let chunk_x = x >> 4;
    let chunk_z = z >> 4;
    if let Some(state) = get_chunk_mods(db, chunk_x, chunk_z).get(&(index as u32)) {
        return *state;
    }
    get_or_generate_chunk(db, chunk_x, chunk_z).blocks[index]
}

pub fn has_open_sky(db: &Arc<sled::Db>, x: i32, y: i32, z: i32) -> bool {
    let chunk_x = x >> 4;
    let chunk_z = z >> 4;
    let chunk = get_or_generate_chunk(db, chunk_x, chunk_z);
    let modifications = get_chunk_mods(db, chunk_x, chunk_z);
    let air = pumpkin_data::Block::AIR.default_state.id;
    for check_y in (y + 1).max(MIN_Y)..=MAX_Y {
        let Some(index) = block_index((x & 15) as usize, check_y, (z & 15) as usize) else {
            continue;
        };
        let state = modifications
            .get(&(index as u32))
            .copied()
            .unwrap_or(chunk.blocks[index]);
        if state != air {
            return false;
        }
    }
    true
}

fn smoothstep(value: f64) -> f64 {
    value * value * (3.0 - 2.0 * value)
}

/// Interpolates climate anchors outside the current chunk, making biome decisions contextual
/// and continuous across chunk boundaries.
fn contextual_noise(noise: &Perlin, world_x: i32, world_z: i32, anchor_size: i32) -> f64 {
    let anchor_x = world_x.div_euclid(anchor_size);
    let anchor_z = world_z.div_euclid(anchor_size);
    let fraction_x = smoothstep(world_x.rem_euclid(anchor_size) as f64 / anchor_size as f64);
    let fraction_z = smoothstep(world_z.rem_euclid(anchor_size) as f64 / anchor_size as f64);
    let sample = |x: i32, z: i32| noise.get([x as f64 / 3.0, z as f64 / 3.0]);
    let north = sample(anchor_x, anchor_z) * (1.0 - fraction_x)
        + sample(anchor_x + 1, anchor_z) * fraction_x;
    let south = sample(anchor_x, anchor_z + 1) * (1.0 - fraction_x)
        + sample(anchor_x + 1, anchor_z + 1) * fraction_x;
    north * (1.0 - fraction_z) + south * fraction_z
}

fn climate_at(
    world_x: i32,
    world_z: i32,
    temperature_noise: &Perlin,
    moisture_noise: &Perlin,
) -> (TerrainBiome, f64) {
    let raw_temperature = contextual_noise(temperature_noise, world_x, world_z, 128);
    let moisture = contextual_noise(moisture_noise, world_x, world_z, 112);
    let temperature = (raw_temperature * 0.72 + 0.55).clamp(-0.25, 1.35);
    let biome = if temperature < 0.18 {
        if moisture > 0.05 {
            TerrainBiome::Taiga
        } else {
            TerrainBiome::SnowyPlains
        }
    } else if temperature > 1.0 {
        if moisture < 0.05 {
            TerrainBiome::Desert
        } else {
            TerrainBiome::Savanna
        }
    } else if moisture > 0.18 {
        TerrainBiome::Forest
    } else {
        TerrainBiome::Plains
    };
    (biome, temperature)
}

fn surface_height_at(
    world_x: i32,
    world_z: i32,
    biome: TerrainBiome,
    terrain_noise: &Perlin,
) -> i32 {
    // Vanilla separates terrain shape from biome selection. These bands approximate its
    // continentalness, erosion, ridge and detail density functions while remaining cheap enough
    // for synchronous chunk snapshots.
    let continentalness = terrain_noise.get([world_x as f64 / 720.0, world_z as f64 / 720.0]);
    let erosion = terrain_noise.get([world_x as f64 / 260.0 + 91.0, world_z as f64 / 260.0 - 37.0]);
    let ridge_sample = terrain_noise.get([
        world_x as f64 / 145.0 - 113.0,
        world_z as f64 / 145.0 + 67.0,
    ]);
    let ridges = (1.0 - ridge_sample.abs()).powi(3);
    let detail = terrain_noise.get([world_x as f64 / 48.0, world_z as f64 / 48.0]);
    let biome_offset = match biome {
        TerrainBiome::Ocean => -2.0,
        TerrainBiome::Taiga => 3.0,
        TerrainBiome::Forest => 2.0,
        TerrainBiome::Savanna => 1.0,
        TerrainBiome::Desert => -1.0,
        TerrainBiome::SnowyPlains | TerrainBiome::Plains => 0.0,
    };
    let ocean_depth = if continentalness < -0.12 {
        (continentalness + 0.12) * 45.0
    } else {
        0.0
    };
    let mountain_height = ((continentalness + 0.08).max(0.0) * ridges * 58.0)
        * (1.0 - erosion * 0.35).clamp(0.45, 1.35);
    (63.0 + biome_offset + continentalness * 31.0 + ocean_depth + mountain_height + detail * 3.5)
        .round() as i32
}

fn coordinate_hash(seed: i64, x: i32, y: i32, z: i32) -> u64 {
    let mut value = seed as u64
        ^ (x as i64 as u64).wrapping_mul(0x9e3779b185ebca87)
        ^ (y as i64 as u64).wrapping_mul(0xc2b2ae3d27d4eb4f)
        ^ (z as i64 as u64).wrapping_mul(0x165667b19e3779f9);
    value ^= value >> 30;
    value = value.wrapping_mul(0xbf58476d1ce4e5b9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94d049bb133111eb);
    value ^ (value >> 31)
}

struct OreVein {
    salt: i64,
    attempts: i32,
    min_y: i32,
    max_y: i32,
    radius: i32,
    stone_ore: u16,
    deepslate_ore: u16,
}

fn add_contextual_ore_veins(chunk: &mut SavedChunk, chunk_x: i32, chunk_z: i32, seed: i64) {
    let veins = [
        OreVein {
            salt: 0x434f_414c,
            attempts: 4,
            min_y: 0,
            max_y: 128,
            radius: 1,
            stone_ore: pumpkin_data::Block::COAL_ORE.default_state.id,
            deepslate_ore: pumpkin_data::Block::DEEPSLATE_COAL_ORE.default_state.id,
        },
        OreVein {
            salt: 0x434f_5050,
            attempts: 3,
            min_y: 0,
            max_y: 96,
            radius: 2,
            stone_ore: pumpkin_data::Block::COPPER_ORE.default_state.id,
            deepslate_ore: pumpkin_data::Block::DEEPSLATE_COPPER_ORE.default_state.id,
        },
        OreVein {
            salt: 0x4952_4f4e,
            attempts: 4,
            min_y: -56,
            max_y: 72,
            radius: 1,
            stone_ore: pumpkin_data::Block::IRON_ORE.default_state.id,
            deepslate_ore: pumpkin_data::Block::DEEPSLATE_IRON_ORE.default_state.id,
        },
        OreVein {
            salt: 0x474f_4c44,
            attempts: 2,
            min_y: -56,
            max_y: 32,
            radius: 1,
            stone_ore: pumpkin_data::Block::GOLD_ORE.default_state.id,
            deepslate_ore: pumpkin_data::Block::DEEPSLATE_GOLD_ORE.default_state.id,
        },
        OreVein {
            salt: 0x4c41_5049,
            attempts: 1,
            min_y: -32,
            max_y: 32,
            radius: 1,
            stone_ore: pumpkin_data::Block::LAPIS_ORE.default_state.id,
            deepslate_ore: pumpkin_data::Block::DEEPSLATE_LAPIS_ORE.default_state.id,
        },
        OreVein {
            salt: 0x5245_4453,
            attempts: 2,
            min_y: -60,
            max_y: 16,
            radius: 1,
            stone_ore: pumpkin_data::Block::REDSTONE_ORE.default_state.id,
            deepslate_ore: pumpkin_data::Block::DEEPSLATE_REDSTONE_ORE.default_state.id,
        },
        OreVein {
            salt: 0x4449_414d,
            attempts: 2,
            min_y: -60,
            max_y: 12,
            radius: 1,
            stone_ore: pumpkin_data::Block::DIAMOND_ORE.default_state.id,
            deepslate_ore: pumpkin_data::Block::DEEPSLATE_DIAMOND_ORE.default_state.id,
        },
    ];

    for vein in &veins {
        for source_z in chunk_z - 1..=chunk_z + 1 {
            for source_x in chunk_x - 1..=chunk_x + 1 {
                for attempt in 0..vein.attempts {
                    let hash = coordinate_hash(seed ^ vein.salt, source_x, attempt, source_z);
                    let span = (vein.max_y - vein.min_y + 1) as u64;
                    let origin = BlockPosition {
                        x: source_x * 16 + (hash & 15) as i32,
                        y: vein.min_y + ((hash >> 8) % span) as i32,
                        z: source_z * 16 + ((hash >> 24) & 15) as i32,
                    };
                    let direction_x = ((hash >> 40) % 3) as i32 - 1;
                    let direction_y = ((hash >> 44) % 3) as i32 - 1;
                    let direction_z = ((hash >> 48) % 3) as i32 - 1;
                    let length = 2 + ((hash >> 52) % 4) as i32;
                    for step in 0..length {
                        place_ore_blob(
                            chunk,
                            ChunkPosition {
                                x: chunk_x,
                                z: chunk_z,
                            },
                            BlockPosition {
                                x: origin.x + direction_x * step,
                                y: origin.y + direction_y * step / 2,
                                z: origin.z + direction_z * step,
                            },
                            vein,
                            hash.wrapping_add(step as u64),
                        );
                    }
                }
            }
        }
    }
}

fn place_ore_blob(
    chunk: &mut SavedChunk,
    target_chunk: ChunkPosition,
    center: BlockPosition,
    vein: &OreVein,
    hash: u64,
) {
    let stone = pumpkin_data::Block::STONE.default_state.id;
    let deepslate = pumpkin_data::Block::DEEPSLATE.default_state.id;
    for dy in -vein.radius..=vein.radius {
        for dz in -vein.radius..=vein.radius {
            for dx in -vein.radius..=vein.radius {
                if dx * dx + dy * dy + dz * dz > vein.radius * vein.radius + 1 {
                    continue;
                }
                let position = BlockPosition {
                    x: center.x + dx,
                    y: center.y + dy,
                    z: center.z + dz,
                };
                if position.x >> 4 != target_chunk.x || position.z >> 4 != target_chunk.z {
                    continue;
                }
                let Some(index) = block_index(
                    (position.x & 15) as usize,
                    position.y,
                    (position.z & 15) as usize,
                ) else {
                    continue;
                };
                let base = chunk.blocks[index];
                if (base == stone || base == deepslate)
                    && !coordinate_hash(hash as i64, position.x, position.y, position.z)
                        .is_multiple_of(5)
                {
                    chunk.blocks[index] = if base == deepslate {
                        vein.deepslate_ore
                    } else {
                        vein.stone_ore
                    };
                }
            }
        }
    }
}

fn generate_chunk(chunk_x: i32, chunk_z: i32, seed: i64) -> SavedChunk {
    let terrain_noise = Perlin::new(seed as u32);
    let temperature_noise = Perlin::new((seed as u32).wrapping_add(0x51f2));
    let moisture_noise = Perlin::new((seed as u32).wrapping_add(0xa913));
    let cave_noise = Perlin::new((seed as u32).wrapping_add(0x37c1));
    let tunnel_noise = Perlin::new((seed as u32).wrapping_add(0x8d21));
    let noodle_noise = Perlin::new((seed as u32).wrapping_add(0x6b43));

    let air = pumpkin_data::Block::AIR.default_state.id;
    let bedrock = pumpkin_data::Block::BEDROCK.default_state.id;
    let stone = pumpkin_data::Block::STONE.default_state.id;
    let deepslate = pumpkin_data::Block::DEEPSLATE.default_state.id;
    let dirt = pumpkin_data::Block::DIRT.default_state.id;
    let grass = pumpkin_data::Block::GRASS_BLOCK.default_state.id;
    let sand = pumpkin_data::Block::SAND.default_state.id;
    let sandstone = pumpkin_data::Block::SANDSTONE.default_state.id;
    let snow = pumpkin_data::Block::SNOW.default_state.id;
    let water = pumpkin_data::Block::WATER.default_state.id;
    const SEA_LEVEL: i32 = 63;

    let mut chunk = SavedChunk {
        format_version: CHUNK_FORMAT_VERSION,
        blocks: vec![air; BLOCKS_PER_CHUNK],
        biomes: vec![0; 256],
        temperatures: vec![0; 256],
        surface_y: vec![0; 256],
    };

    for local_z in 0..CHUNK_WIDTH {
        for local_x in 0..CHUNK_WIDTH {
            let world_x = chunk_x * 16 + local_x as i32;
            let world_z = chunk_z * 16 + local_z as i32;
            let (climate_biome, temperature) =
                climate_at(world_x, world_z, &temperature_noise, &moisture_noise);
            let surface =
                surface_height_at(world_x, world_z, climate_biome, &terrain_noise).clamp(34, 180);
            let biome = if surface < SEA_LEVEL - 2 {
                TerrainBiome::Ocean
            } else {
                climate_biome
            };
            let column = column_index(local_x, local_z);
            chunk.biomes[column] = biome.protocol_id();
            chunk.temperatures[column] = (temperature * 1000.0).round() as i16;
            chunk.surface_y[column] = surface as i16;

            for y in MIN_Y..=surface {
                let state = if y == MIN_Y
                    || (y < MIN_Y + 5
                        && coordinate_hash(seed, world_x, y, world_z).is_multiple_of(5))
                {
                    bedrock
                } else if y == surface {
                    match biome {
                        TerrainBiome::Desert | TerrainBiome::Ocean => sand,
                        _ => grass,
                    }
                } else if y >= surface - 3 {
                    match biome {
                        TerrainBiome::Desert | TerrainBiome::Ocean => {
                            if y == surface - 3 {
                                sandstone
                            } else {
                                sand
                            }
                        }
                        _ => dirt,
                    }
                } else {
                    let base = if y < 0 { deepslate } else { stone };
                    let cave_a = cave_noise.get([
                        world_x as f64 / 31.0,
                        y as f64 / 23.0,
                        world_z as f64 / 31.0,
                    ]);
                    let cave_b = tunnel_noise.get([
                        world_x as f64 / 57.0,
                        y as f64 / 19.0,
                        world_z as f64 / 57.0,
                    ]);
                    let cave_c = noodle_noise.get([
                        world_x as f64 / 38.0 - 17.0,
                        y as f64 / 27.0 + 41.0,
                        world_z as f64 / 38.0 + 23.0,
                    ]);
                    let cheese_cave = y < 48 && cave_a > 0.69;
                    let spaghetti_cave = cave_a.abs() < 0.105 && cave_b.abs() < 0.09;
                    let noodle_cave = y < 34 && cave_b.abs() < 0.055 && cave_c.abs() < 0.052;
                    let is_cave = y > MIN_Y + 5
                        && y < surface - 5
                        && (cheese_cave || spaghetti_cave || noodle_cave);
                    if is_cave {
                        air
                    } else {
                        base
                    }
                };
                if let Some(index) = block_index(local_x, y, local_z) {
                    chunk.blocks[index] = state;
                }
            }

            if surface < SEA_LEVEL {
                for y in surface + 1..=SEA_LEVEL {
                    if let Some(index) = block_index(local_x, y, local_z) {
                        chunk.blocks[index] = water;
                    }
                }
            }

            if biome == TerrainBiome::SnowyPlains && surface >= SEA_LEVEL && surface < MAX_Y {
                if let Some(index) = block_index(local_x, surface + 1, local_z) {
                    chunk.blocks[index] = snow;
                }
            }
        }
    }

    seal_isolated_cave_pockets(&mut chunk);
    add_contextual_ore_veins(&mut chunk, chunk_x, chunk_z, seed);

    add_contextual_trees(
        &mut chunk,
        chunk_x,
        chunk_z,
        seed,
        &terrain_noise,
        &temperature_noise,
        &moisture_noise,
    );
    add_contextual_structures(
        &mut chunk,
        chunk_x,
        chunk_z,
        seed,
        &terrain_noise,
        &temperature_noise,
        &moisture_noise,
    );
    chunk
}

fn seal_isolated_cave_pockets(chunk: &mut SavedChunk) {
    let air = pumpkin_data::Block::AIR.default_state.id;
    let stone = pumpkin_data::Block::STONE.default_state.id;
    let deepslate = pumpkin_data::Block::DEEPSLATE.default_state.id;
    let original = chunk.blocks.clone();
    for y in MIN_Y + 1..MAX_Y {
        for z in 1..15 {
            for x in 1..15 {
                let index = block_index(x, y, z).unwrap();
                if original[index] != air || y >= chunk.surface_y[column_index(x, z)] as i32 - 5 {
                    continue;
                }
                let neighbors = [
                    (x - 1, y, z),
                    (x + 1, y, z),
                    (x, y - 1, z),
                    (x, y + 1, z),
                    (x, y, z - 1),
                    (x, y, z + 1),
                ];
                if neighbors
                    .iter()
                    .all(|(nx, ny, nz)| original[block_index(*nx, *ny, *nz).unwrap()] != air)
                {
                    chunk.blocks[index] = if y < 0 { deepslate } else { stone };
                }
            }
        }
    }
}

fn add_contextual_trees(
    chunk: &mut SavedChunk,
    chunk_x: i32,
    chunk_z: i32,
    seed: i64,
    terrain_noise: &Perlin,
    temperature_noise: &Perlin,
    moisture_noise: &Perlin,
) {
    for source_z in chunk_z - 1..=chunk_z + 1 {
        for source_x in chunk_x - 1..=chunk_x + 1 {
            for candidate in 0..4 {
                let hash = coordinate_hash(seed ^ 0x5452_4545, source_x, candidate, source_z);
                let world_x = source_x * 16 + (hash & 15) as i32;
                let world_z = source_z * 16 + ((hash >> 8) & 15) as i32;
                let (biome, _) = climate_at(world_x, world_z, temperature_noise, moisture_noise);
                if (hash >> 16) % 100 >= biome.tree_density() {
                    continue;
                }
                let surface =
                    surface_height_at(world_x, world_z, biome, terrain_noise).clamp(34, 180);
                if surface < 63 {
                    continue;
                }
                place_tree(
                    chunk,
                    ChunkPosition {
                        x: chunk_x,
                        z: chunk_z,
                    },
                    BlockPosition {
                        x: world_x,
                        y: surface + 1,
                        z: world_z,
                    },
                    biome,
                    hash,
                );
            }
        }
    }
}

fn set_tree_block(
    chunk: &mut SavedChunk,
    target_chunk: ChunkPosition,
    position: BlockPosition,
    state: u16,
    replace_only_air: bool,
) {
    if position.x >> 4 != target_chunk.x || position.z >> 4 != target_chunk.z {
        return;
    }
    let Some(index) = block_index(
        (position.x & 15) as usize,
        position.y,
        (position.z & 15) as usize,
    ) else {
        return;
    };
    if !replace_only_air || chunk.blocks[index] == pumpkin_data::Block::AIR.default_state.id {
        chunk.blocks[index] = state;
    }
}

fn place_tree(
    chunk: &mut SavedChunk,
    target_chunk: ChunkPosition,
    origin: BlockPosition,
    biome: TerrainBiome,
    hash: u64,
) {
    let (log, leaves, height) = match biome {
        TerrainBiome::Taiga | TerrainBiome::SnowyPlains => (
            pumpkin_data::Block::SPRUCE_LOG.default_state.id,
            pumpkin_data::Block::SPRUCE_LEAVES.default_state.id,
            6 + (hash % 3) as i32,
        ),
        TerrainBiome::Savanna => (
            pumpkin_data::Block::ACACIA_LOG.default_state.id,
            pumpkin_data::Block::ACACIA_LEAVES.default_state.id,
            5 + (hash % 2) as i32,
        ),
        _ => (
            pumpkin_data::Block::OAK_LOG.default_state.id,
            pumpkin_data::Block::OAK_LEAVES.default_state.id,
            5 + (hash % 3) as i32,
        ),
    };

    for y in origin.y..origin.y + height {
        set_tree_block(
            chunk,
            target_chunk,
            BlockPosition { y, ..origin },
            log,
            false,
        );
    }
    let canopy_base = origin.y + height - 3;
    for dy in 0..=3 {
        let radius: i32 = if dy == 3 { 1 } else { 2 };
        for dx in -radius..=radius {
            for dz in -radius..=radius {
                if dx.abs() == radius && dz.abs() == radius && (hash + dy as u64).is_multiple_of(3)
                {
                    continue;
                }
                set_tree_block(
                    chunk,
                    target_chunk,
                    BlockPosition {
                        x: origin.x + dx,
                        y: canopy_base + dy,
                        z: origin.z + dz,
                    },
                    leaves,
                    true,
                );
            }
        }
    }
}

fn add_contextual_structures(
    chunk: &mut SavedChunk,
    chunk_x: i32,
    chunk_z: i32,
    seed: i64,
    terrain_noise: &Perlin,
    temperature_noise: &Perlin,
    moisture_noise: &Perlin,
) {
    const REGION_SIZE: i32 = 10;
    let target_region_x = chunk_x.div_euclid(REGION_SIZE);
    let target_region_z = chunk_z.div_euclid(REGION_SIZE);
    for region_z in target_region_z - 1..=target_region_z + 1 {
        for region_x in target_region_x - 1..=target_region_x + 1 {
            let hash = coordinate_hash(seed ^ 0x5354_5255_4354, region_x, 0, region_z);
            if !hash.is_multiple_of(3) {
                continue;
            }
            let source_chunk_x = region_x * REGION_SIZE + ((hash >> 8) % REGION_SIZE as u64) as i32;
            let source_chunk_z =
                region_z * REGION_SIZE + ((hash >> 20) % REGION_SIZE as u64) as i32;
            let world_x = source_chunk_x * 16 + 8;
            let world_z = source_chunk_z * 16 + 8;
            let (biome, _) = climate_at(world_x, world_z, temperature_noise, moisture_noise);
            if matches!(biome, TerrainBiome::Desert | TerrainBiome::SnowyPlains) {
                continue;
            }
            let surface = surface_height_at(world_x, world_z, biome, terrain_noise).clamp(34, 180);
            if surface < 63 {
                continue;
            }
            place_cabin(
                chunk,
                ChunkPosition {
                    x: chunk_x,
                    z: chunk_z,
                },
                BlockPosition {
                    x: world_x,
                    y: surface,
                    z: world_z,
                },
            );
        }
    }
}

fn set_structure_block(
    chunk: &mut SavedChunk,
    target_chunk: ChunkPosition,
    position: BlockPosition,
    state: u16,
) {
    if position.x >> 4 != target_chunk.x || position.z >> 4 != target_chunk.z {
        return;
    }
    let Some(index) = block_index(
        (position.x & 15) as usize,
        position.y,
        (position.z & 15) as usize,
    ) else {
        return;
    };
    chunk.blocks[index] = state;
}

fn place_cabin(chunk: &mut SavedChunk, target_chunk: ChunkPosition, center: BlockPosition) {
    let cobblestone = pumpkin_data::Block::COBBLESTONE.default_state.id;
    let planks = pumpkin_data::Block::OAK_PLANKS.default_state.id;
    let logs = pumpkin_data::Block::OAK_LOG.default_state.id;
    let air = pumpkin_data::Block::AIR.default_state.id;

    for dz in -3i32..=3 {
        for dx in -3i32..=3 {
            set_structure_block(
                chunk,
                target_chunk,
                BlockPosition {
                    x: center.x + dx,
                    y: center.y - 1,
                    z: center.z + dz,
                },
                cobblestone,
            );
            set_structure_block(
                chunk,
                target_chunk,
                BlockPosition {
                    x: center.x + dx,
                    y: center.y,
                    z: center.z + dz,
                },
                planks,
            );
        }
    }

    for dy in 1..=3 {
        for offset in -3i32..=3 {
            for (dx, dz) in [(offset, -3), (offset, 3), (-3, offset), (3, offset)] {
                let is_corner = dx.abs() == 3 && dz.abs() == 3;
                let is_door = dz == -3 && dx == 0 && dy <= 2;
                set_structure_block(
                    chunk,
                    target_chunk,
                    BlockPosition {
                        x: center.x + dx,
                        y: center.y + dy,
                        z: center.z + dz,
                    },
                    if is_door {
                        air
                    } else if is_corner {
                        logs
                    } else {
                        planks
                    },
                );
            }
        }
    }

    for dz in -4i32..=4 {
        for dx in -4i32..=4 {
            set_structure_block(
                chunk,
                target_chunk,
                BlockPosition {
                    x: center.x + dx,
                    y: center.y + 4,
                    z: center.z + dz,
                },
                planks,
            );
        }
    }
}

pub fn encode_chunk_packet(
    chunk_x: i32,
    chunk_z: i32,
    protocol_version: i32,
    db: &Arc<sled::Db>,
) -> Arc<Vec<u8>> {
    let cache_key = (database_id(db), chunk_x, chunk_z, protocol_version);
    if let Ok(cache) = chunk_packet_cache().lock() {
        if let Some(packet) = cache.get(&cache_key) {
            return Arc::clone(packet);
        }
    }

    let chunk = get_or_generate_chunk(db, chunk_x, chunk_z);
    let modifications = get_chunk_mods(db, chunk_x, chunk_z);
    let packet = Arc::new(build_chunk_packet(
        chunk_x,
        chunk_z,
        protocol_version,
        &chunk,
        &modifications,
    ));
    if let Ok(mut cache) = chunk_packet_cache().lock() {
        if cache.len() >= MAX_CACHED_CHUNK_PACKETS {
            cache.clear();
        }
        cache.insert(cache_key, Arc::clone(&packet));
    }
    packet
}

fn build_chunk_packet(
    chunk_x: i32,
    chunk_z: i32,
    protocol_version: i32,
    chunk: &SavedChunk,
    modifications: &HashMap<u32, u16>,
) -> Vec<u8> {
    let version = MinecraftVersion::from_protocol(protocol_version as u32);
    let mut packet_id =
        pumpkin_data::packet::clientbound::PLAY_LEVEL_CHUNK_WITH_LIGHT.to_id(version);
    if packet_id < 0 {
        packet_id = if protocol_version >= 775 { 45 } else { 44 };
    }
    let mut buf = Vec::new();
    write_vi(&mut buf, packet_id);
    buf.extend_from_slice(&chunk_x.to_be_bytes());
    buf.extend_from_slice(&chunk_z.to_be_bytes());

    let mut heightmap = vec![0i64; 37];
    for index in 0..256 {
        let value = (chunk.surface_y[index] as i32 - MIN_Y + 1).clamp(0, 511) as i64;
        let long_index = index / 7;
        let bit_index = (index % 7) * 9;
        heightmap[long_index] |= value << bit_index;
    }
    write_vi(&mut buf, 3);
    for heightmap_type in [1, 4, 5] {
        write_vi(&mut buf, heightmap_type);
        write_vi(&mut buf, heightmap.len() as i32);
        for value in &heightmap {
            buf.extend_from_slice(&value.to_be_bytes());
        }
    }

    let air = pumpkin_data::Block::AIR.default_state.id;
    let mut sections = Vec::new();
    for section in 0..SECTION_COUNT {
        let section_start = section * BLOCKS_PER_SECTION;
        let mut blocks = Vec::with_capacity(BLOCKS_PER_SECTION);
        let mut non_air = 0i16;
        for offset in 0..BLOCKS_PER_SECTION {
            let global_index = section_start + offset;
            let state = modifications
                .get(&(global_index as u32))
                .copied()
                .unwrap_or(chunk.blocks[global_index]);
            if state != air {
                non_air += 1;
            }
            blocks.push(state);
        }
        sections.extend_from_slice(&non_air.to_be_bytes());
        if version >= MinecraftVersion::V_26_1 {
            sections.extend_from_slice(&0i16.to_be_bytes());
        }
        write_block_palette(&mut sections, &blocks, version);
        write_biome_palette(&mut sections, chunk, version);
    }

    write_vi(&mut buf, sections.len() as i32);
    buf.extend_from_slice(&sections);
    write_vi(&mut buf, 0);

    let total_light_sections = SECTION_COUNT + 2;
    let mut empty_mask = vec![0i64; total_light_sections.div_ceil(64)];
    for bit in 0..total_light_sections {
        empty_mask[bit / 64] |= 1i64 << (bit % 64);
    }
    let zero_mask = vec![0i64; empty_mask.len()];
    for mask in [&zero_mask, &zero_mask, &empty_mask, &empty_mask] {
        write_vi(&mut buf, mask.len() as i32);
        for value in mask {
            buf.extend_from_slice(&value.to_be_bytes());
        }
    }
    write_vi(&mut buf, 0);
    write_vi(&mut buf, 0);
    buf
}

fn write_block_palette(buf: &mut Vec<u8>, blocks: &[u16], version: MinecraftVersion) {
    let remap =
        |state| pumpkin_data::block_state_remap::remap_block_state_for_version(state, version);
    let first = remap(blocks[0]);
    if blocks.iter().all(|state| remap(*state) == first) {
        buf.push(0);
        write_vi(buf, first as i32);
        if version <= MinecraftVersion::V_1_21_4 {
            write_vi(buf, 0);
        }
        return;
    }

    const BITS: usize = 15;
    let values_per_long = 64 / BITS;
    let mut packed = vec![0i64; blocks.len().div_ceil(values_per_long)];
    for (index, state) in blocks.iter().enumerate() {
        let network_state = remap(*state);
        packed[index / values_per_long] |=
            (network_state as i64) << ((index % values_per_long) * BITS);
    }
    buf.push(BITS as u8);
    if version <= MinecraftVersion::V_1_21_4 {
        write_vi(buf, packed.len() as i32);
    }
    for value in packed {
        buf.extend_from_slice(&value.to_be_bytes());
    }
}

fn write_biome_palette(buf: &mut Vec<u8>, chunk: &SavedChunk, version: MinecraftVersion) {
    const BITS: usize = 7;
    let values_per_long = 64 / BITS;
    let mut values = [0u8; 64];
    for biome_y in 0..4 {
        for biome_z in 0..4 {
            for biome_x in 0..4 {
                let value_index = biome_y * 16 + biome_z * 4 + biome_x;
                values[value_index] = chunk.biomes[column_index(biome_x * 4 + 2, biome_z * 4 + 2)];
            }
        }
    }
    let mut packed = vec![0i64; values.len().div_ceil(values_per_long)];
    for (index, biome) in values.iter().enumerate() {
        packed[index / values_per_long] |= (*biome as i64) << ((index % values_per_long) * BITS);
    }
    buf.push(BITS as u8);
    if version <= MinecraftVersion::V_1_21_4 {
        write_vi(buf, packed.len() as i32);
    }
    for value in packed {
        buf.extend_from_slice(&value.to_be_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terrain_states_are_remapped_for_older_clients() {
        let version = MinecraftVersion::V_1_21_4;
        for block in [
            &pumpkin_data::Block::GRASS_BLOCK,
            &pumpkin_data::Block::SNOW,
            &pumpkin_data::Block::COPPER_ORE,
            &pumpkin_data::Block::DEEPSLATE,
        ] {
            let canonical = block.default_state.id;
            let network =
                pumpkin_data::block_state_remap::remap_block_state_for_version(canonical, version);
            assert_ne!(network, 0, "{} remapped to air", block.name);
        }
        assert_ne!(
            pumpkin_data::Block::COPPER_ORE.default_state.id,
            pumpkin_data::block_state_remap::remap_block_state_for_version(
                pumpkin_data::Block::COPPER_ORE.default_state.id,
                version,
            ),
            "the regression requires a non-identity legacy mapping"
        );
    }

    #[test]
    fn generated_chunks_are_saved_and_stable() {
        let db = Arc::new(sled::Config::new().temporary(true).open().unwrap());
        let first = get_or_generate_chunk(&db, 4, -3);
        chunk_cache().lock().unwrap().clear();
        let second = get_or_generate_chunk(&db, 4, -3);
        assert_eq!(first.blocks, second.blocks);
        assert_eq!(first.biomes, second.biomes);
        assert_eq!(first.temperatures, second.temperatures);
    }

    #[test]
    fn neighboring_chunks_share_tree_canopies_without_seams() {
        let air = pumpkin_data::Block::AIR.default_state.id;
        let empty_chunk = || SavedChunk {
            format_version: CHUNK_FORMAT_VERSION,
            blocks: vec![air; BLOCKS_PER_CHUNK],
            biomes: vec![TerrainBiome::Forest.protocol_id(); 256],
            temperatures: vec![700; 256],
            surface_y: vec![69; 256],
        };
        let mut left = empty_chunk();
        let mut right = empty_chunk();
        let origin = BlockPosition { x: 15, y: 70, z: 8 };
        place_tree(
            &mut left,
            ChunkPosition { x: 0, z: 0 },
            origin,
            TerrainBiome::Forest,
            1,
        );
        place_tree(
            &mut right,
            ChunkPosition { x: 1, z: 0 },
            origin,
            TerrainBiome::Forest,
            1,
        );
        let leaves = [
            pumpkin_data::Block::OAK_LEAVES.default_state.id,
            pumpkin_data::Block::SPRUCE_LEAVES.default_state.id,
            pumpkin_data::Block::ACACIA_LEAVES.default_state.id,
        ];
        assert!(left.blocks.iter().any(|state| leaves.contains(state)));
        assert!(right.blocks.iter().any(|state| leaves.contains(state)));
    }

    #[test]
    fn block_changes_override_saved_terrain() {
        let db = Arc::new(sled::Config::new().temporary(true).open().unwrap());
        let original = get_block_state(&db, 17, 70, -1);
        save_block_change(&db, 17, 70, -1, 42);
        assert_eq!(get_block_state(&db, 17, 70, -1), 42);
        assert_ne!(original, 42);
    }

    #[test]
    fn underground_contains_caves_and_ores() {
        let chunk = generate_chunk(0, 0, DEFAULT_WORLD_SEED);
        let air = pumpkin_data::Block::AIR.default_state.id;
        let diamond = pumpkin_data::Block::DEEPSLATE_DIAMOND_ORE.default_state.id;
        let underground = &chunk.blocks[..((60 - MIN_Y) as usize * 256)];
        assert!(underground.iter().any(|state| *state == air));
        assert!(underground.iter().any(|state| *state == diamond));
    }

    #[test]
    fn iron_generates_in_connected_clusters() {
        let chunk = generate_chunk(0, 0, DEFAULT_WORLD_SEED);
        let iron = [
            pumpkin_data::Block::IRON_ORE.default_state.id,
            pumpkin_data::Block::DEEPSLATE_IRON_ORE.default_state.id,
        ];
        let mut total = 0;
        let mut connected = 0;
        for y in MIN_Y + 1..MAX_Y {
            for z in 1..15 {
                for x in 1..15 {
                    let index = block_index(x, y, z).unwrap();
                    if !iron.contains(&chunk.blocks[index]) {
                        continue;
                    }
                    total += 1;
                    let neighbors = [
                        (x - 1, y, z),
                        (x + 1, y, z),
                        (x, y - 1, z),
                        (x, y + 1, z),
                        (x, y, z - 1),
                        (x, y, z + 1),
                    ];
                    if neighbors.iter().any(|(nx, ny, nz)| {
                        iron.contains(&chunk.blocks[block_index(*nx, *ny, *nz).unwrap()])
                    }) {
                        connected += 1;
                    }
                }
            }
        }
        assert!(total > 0);
        assert!(
            connected * 4 >= total * 3,
            "iron was not sufficiently clustered"
        );
    }

    #[test]
    fn cave_noise_does_not_create_single_block_air_pockets() {
        let chunk = generate_chunk(0, 0, DEFAULT_WORLD_SEED);
        let air = pumpkin_data::Block::AIR.default_state.id;
        for y in MIN_Y + 2..50 {
            for z in 1..15 {
                for x in 1..15 {
                    if chunk.blocks[block_index(x, y, z).unwrap()] != air {
                        continue;
                    }
                    let neighbors = [
                        (x - 1, y, z),
                        (x + 1, y, z),
                        (x, y - 1, z),
                        (x, y + 1, z),
                        (x, y, z - 1),
                        (x, y, z + 1),
                    ];
                    assert!(neighbors.iter().any(|(nx, ny, nz)| {
                        chunk.blocks[block_index(*nx, *ny, *nz).unwrap()] == air
                    }));
                }
            }
        }
    }

    #[test]
    fn cabins_are_chunk_context_aware() {
        let air = pumpkin_data::Block::AIR.default_state.id;
        let mut chunk = SavedChunk {
            format_version: CHUNK_FORMAT_VERSION,
            blocks: vec![air; BLOCKS_PER_CHUNK],
            biomes: vec![TerrainBiome::Plains.protocol_id(); 256],
            temperatures: vec![500; 256],
            surface_y: vec![64; 256],
        };
        place_cabin(
            &mut chunk,
            ChunkPosition { x: 0, z: 0 },
            BlockPosition { x: 15, y: 64, z: 8 },
        );
        assert!(chunk
            .blocks
            .contains(&pumpkin_data::Block::OAK_PLANKS.default_state.id));
        assert!(chunk
            .blocks
            .contains(&pumpkin_data::Block::COBBLESTONE.default_state.id));
    }

    #[test]
    fn overhead_blocks_prevent_open_sky() {
        let db = Arc::new(sled::Config::new().temporary(true).open().unwrap());
        assert!(has_open_sky(&db, 0, 300, 0));
        save_block_change(&db, 0, 310, 0, pumpkin_data::Block::STONE.default_state.id);
        assert!(!has_open_sky(&db, 0, 300, 0));
    }
}
