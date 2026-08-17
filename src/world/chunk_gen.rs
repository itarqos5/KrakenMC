use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex, OnceLock};

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use noise::{NoiseFn, Perlin};
use pumpkin_util::version::MinecraftVersion;
use serde::{Deserialize, Serialize};

const CHUNK_FORMAT_VERSION: u8 = 1;
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
    let broad = terrain_noise.get([world_x as f64 / 180.0, world_z as f64 / 180.0]);
    let detail = terrain_noise.get([world_x as f64 / 42.0, world_z as f64 / 42.0]);
    let biome_offset = match biome {
        TerrainBiome::Taiga => 3.0,
        TerrainBiome::Forest => 2.0,
        TerrainBiome::Savanna => 1.0,
        TerrainBiome::Desert => -1.0,
        TerrainBiome::SnowyPlains | TerrainBiome::Plains => 0.0,
    };
    (67.0 + biome_offset + broad * 13.0 + detail * 3.0).round() as i32
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

fn ore_state(seed: i64, world_x: i32, y: i32, world_z: i32, base_state: u16) -> u16 {
    let roll = coordinate_hash(seed, world_x, y, world_z) % 10_000;
    let deep = y < 0;
    let block = if y < 16 && roll < 10 {
        if deep {
            &pumpkin_data::Block::DEEPSLATE_DIAMOND_ORE
        } else {
            &pumpkin_data::Block::DIAMOND_ORE
        }
    } else if y < 16 && roll < 32 {
        if deep {
            &pumpkin_data::Block::DEEPSLATE_REDSTONE_ORE
        } else {
            &pumpkin_data::Block::REDSTONE_ORE
        }
    } else if y < 32 && roll < 48 {
        if deep {
            &pumpkin_data::Block::DEEPSLATE_GOLD_ORE
        } else {
            &pumpkin_data::Block::GOLD_ORE
        }
    } else if (-32..=32).contains(&y) && roll < 68 {
        if deep {
            &pumpkin_data::Block::DEEPSLATE_LAPIS_ORE
        } else {
            &pumpkin_data::Block::LAPIS_ORE
        }
    } else if y < 72 && roll < 125 {
        if deep {
            &pumpkin_data::Block::DEEPSLATE_IRON_ORE
        } else {
            &pumpkin_data::Block::IRON_ORE
        }
    } else if (0..=96).contains(&y) && roll < 165 {
        if deep {
            &pumpkin_data::Block::DEEPSLATE_COPPER_ORE
        } else {
            &pumpkin_data::Block::COPPER_ORE
        }
    } else if y < 128 && roll < 235 {
        if deep {
            &pumpkin_data::Block::DEEPSLATE_COAL_ORE
        } else {
            &pumpkin_data::Block::COAL_ORE
        }
    } else {
        return base_state;
    };
    block.default_state.id
}

fn generate_chunk(chunk_x: i32, chunk_z: i32, seed: i64) -> SavedChunk {
    let terrain_noise = Perlin::new(seed as u32);
    let temperature_noise = Perlin::new((seed as u32).wrapping_add(0x51f2));
    let moisture_noise = Perlin::new((seed as u32).wrapping_add(0xa913));
    let cave_noise = Perlin::new((seed as u32).wrapping_add(0x37c1));
    let tunnel_noise = Perlin::new((seed as u32).wrapping_add(0x8d21));

    let air = pumpkin_data::Block::AIR.default_state.id;
    let bedrock = pumpkin_data::Block::BEDROCK.default_state.id;
    let stone = pumpkin_data::Block::STONE.default_state.id;
    let deepslate = pumpkin_data::Block::DEEPSLATE.default_state.id;
    let dirt = pumpkin_data::Block::DIRT.default_state.id;
    let grass = pumpkin_data::Block::GRASS_BLOCK.default_state.id;
    let sand = pumpkin_data::Block::SAND.default_state.id;
    let sandstone = pumpkin_data::Block::SANDSTONE.default_state.id;
    let snow = pumpkin_data::Block::SNOW.default_state.id;

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
            let (biome, temperature) =
                climate_at(world_x, world_z, &temperature_noise, &moisture_noise);
            let surface = surface_height_at(world_x, world_z, biome, &terrain_noise).clamp(48, 110);
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
                        TerrainBiome::Desert => sand,
                        _ => grass,
                    }
                } else if y >= surface - 3 {
                    match biome {
                        TerrainBiome::Desert => {
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
                    let is_cave = y > MIN_Y + 5
                        && y < surface - 5
                        && (cave_a.abs() > 0.64 || (cave_a.abs() > 0.48 && cave_b.abs() < 0.09));
                    if is_cave {
                        air
                    } else {
                        ore_state(seed, world_x, y, world_z, base)
                    }
                };
                if let Some(index) = block_index(local_x, y, local_z) {
                    chunk.blocks[index] = state;
                }
            }

            if biome == TerrainBiome::SnowyPlains && surface < MAX_Y {
                if let Some(index) = block_index(local_x, surface + 1, local_z) {
                    chunk.blocks[index] = snow;
                }
            }
        }
    }

    add_contextual_trees(
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
                    surface_height_at(world_x, world_z, biome, terrain_noise).clamp(48, 110);
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
}
