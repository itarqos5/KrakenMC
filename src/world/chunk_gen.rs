use noise::{NoiseFn, Perlin};
use pumpkin_util::version::MinecraftVersion;

/// Number of block sections in the overworld (-64..320 = 384 blocks high = 24 sections)
const SECTION_COUNT: usize = 24;
const MIN_Y: i32 = -64;
const CHUNK_WIDTH: usize = 16;
const SECTION_HEIGHT: usize = 16;

/// Write a VarInt into a Vec<u8>.
fn write_vi(buf: &mut Vec<u8>, value: i32) {
    let mut v = value as u32;
    loop {
        if (v & !0x7F) == 0 {
            buf.push(v as u8);
            break;
        }
        buf.push(((v & 0x7F) as u8) | 0x80);
        v >>= 7;
    }
}

/// Build the raw PLAY_LEVEL_CHUNK_WITH_LIGHT packet payload for a single chunk.
/// This is hand-crafted wire format for protocol 774/775.
pub fn encode_chunk_packet(chunk_x: i32, chunk_z: i32, protocol_version: i32) -> Vec<u8> {
    let perlin = Perlin::new(42);
    // Precompute surface height for each column (0..16, 0..16)
    let mut surface_y = [[0i32; 16]; 16];
    for x in 0..CHUNK_WIDTH {
        for z in 0..CHUNK_WIDTH {
            let nx = (chunk_x * 16 + x as i32) as f64 / 32.0;
            let nz = (chunk_z * 16 + z as i32) as f64 / 32.0;
            let n = perlin.get([nx, nz]);
            // n is in [-1.0, 1.0], map to height offset [-8, +8] around base y=68
            surface_y[x][z] = 68 + (n * 8.0) as i32;
        }
    }

    let version = MinecraftVersion::from_protocol(protocol_version as u32);
    let mut packet_id = pumpkin_data::packet::clientbound::PLAY_LEVEL_CHUNK_WITH_LIGHT.to_id(version);
    if packet_id < 0 {
        // Fallback to V_26_1 or V_1_21_11 default ID
        packet_id = if protocol_version >= 775 { 45 } else { 44 };
    }

    let mut buf: Vec<u8> = Vec::new();
    write_vi(&mut buf, packet_id); // packet ID

    // Chunk X, Z
    buf.extend_from_slice(&chunk_x.to_be_bytes());
    buf.extend_from_slice(&chunk_z.to_be_bytes());

    // === Heightmaps (new format >=1.21.5: VarInt map size, then index/len/data triplets) ===
    // We send 3 heightmaps: WORLD_SURFACE(1), MOTION_BLOCKING(4), MOTION_BLOCKING_NO_LEAVES(5)
    // Each is 256 values (16x16) packed into ceil(256*9/64)=36 i64s at 9 bits/entry
    let pack_heightmap = |ys: &[[i32; 16]; 16]| -> Vec<i64> {
        let bits = 9usize;
        let values_per_i64 = 64 / bits; // 7
        let total = CHUNK_WIDTH * CHUNK_WIDTH; // 256
        let num_longs = (total + values_per_i64 - 1) / values_per_i64; // 37
        let mut longs = vec![0i64; num_longs];
        for z in 0..CHUNK_WIDTH {
            for x in 0..CHUNK_WIDTH {
                let idx = z * CHUNK_WIDTH + x;
                let height = (ys[x][z] - MIN_Y + 1).max(0).min(511) as i64;
                let long_idx = idx / values_per_i64;
                let bit_idx = (idx % values_per_i64) * bits;
                longs[long_idx] |= height << bit_idx;
            }
        }
        longs
    };
    let hm = pack_heightmap(&surface_y);

    // Map size = 3
    write_vi(&mut buf, 3);
    for (index, hm_data) in [(1i32, &hm), (4i32, &hm), (5i32, &hm)] {
        write_vi(&mut buf, index);
        write_vi(&mut buf, hm_data.len() as i32);
        for &val in hm_data {
            buf.extend_from_slice(&val.to_be_bytes());
        }
    }

    // === Chunk sections data ===
    const AIR: u16 = 0;
    const STONE: u16 = 1;
    const DIRT: u16 = 78;       // approximate
    const GRASS: u16 = 9;       // approximate
    const PLAINS_BIOME: u16 = 39;

    let mut sections_buf: Vec<u8> = Vec::new();
    for section_idx in 0..SECTION_COUNT {
        let section_base_y = MIN_Y + (section_idx as i32) * SECTION_HEIGHT as i32;

        let mut blocks = [AIR; CHUNK_WIDTH * CHUNK_WIDTH * SECTION_HEIGHT];
        let mut non_air_count = 0i16;
        for bx in 0..CHUNK_WIDTH {
            for bz in 0..CHUNK_WIDTH {
                let surf = surface_y[bx][bz];
                for by in 0..SECTION_HEIGHT {
                    let world_y = section_base_y + by as i32;
                    let idx = by * CHUNK_WIDTH * CHUNK_WIDTH + bz * CHUNK_WIDTH + bx;
                    let block = if world_y < surf - 3 {
                        STONE
                    } else if world_y < surf {
                        DIRT
                    } else if world_y == surf {
                        GRASS
                    } else {
                        AIR
                    };
                    blocks[idx] = block;
                    if block != AIR {
                        non_air_count += 1;
                    }
                }
            }
        }

        // Non-air block count
        sections_buf.extend_from_slice(&non_air_count.to_be_bytes());
        
        // Fluid count (only for V_26_1 / V_775 and above)
        if version >= MinecraftVersion::V_26_1 {
            sections_buf.extend_from_slice(&0i16.to_be_bytes());
        }

        // Block palette
        let first = blocks[0];
        let all_same = blocks.iter().all(|&b| b == first);
        if all_same {
            sections_buf.push(0); // bits per entry
            write_vi(&mut sections_buf, first as i32); // state id
            if version <= MinecraftVersion::V_1_21_4 {
                write_vi(&mut sections_buf, 0); // data array length (no longs)
            }
        } else {
            let bits: usize = 15;
            let values_per_i64 = 64 / bits; // 4
            let total = CHUNK_WIDTH * CHUNK_WIDTH * SECTION_HEIGHT; // 4096
            let num_longs = (total + values_per_i64 - 1) / values_per_i64;
            let mut longs = vec![0i64; num_longs];
            for (idx, &block) in blocks.iter().enumerate() {
                let long_idx = idx / values_per_i64;
                let bit_idx = (idx % values_per_i64) * bits;
                longs[long_idx] |= (block as i64) << bit_idx;
            }
            sections_buf.push(bits as u8);
            if version <= MinecraftVersion::V_1_21_4 {
                write_vi(&mut sections_buf, longs.len() as i32);
            }
            for packed in longs {
                sections_buf.extend_from_slice(&packed.to_be_bytes());
            }
        }

        // Biome palette - single value (plains)
        sections_buf.push(0); // bits per entry = 0 (single)
        write_vi(&mut sections_buf, PLAINS_BIOME as i32);
        if version <= MinecraftVersion::V_1_21_4 {
            write_vi(&mut sections_buf, 0); // no data array
        }
    }

    // Write sections size as VarInt then the data
    write_vi(&mut buf, sections_buf.len() as i32);
    buf.extend_from_slice(&sections_buf);

    // Block entities count = 0
    write_vi(&mut buf, 0);

    // === Light data ===
    let total_bits = SECTION_COUNT + 2;
    let empty_mask_longs = {
        let num_longs = (total_bits + 63) / 64;
        let mut v = vec![0i64; num_longs];
        for bit in 0..total_bits {
            let li = bit / 64;
            let bi = bit % 64;
            v[li] |= 1i64 << bi;
        }
        v
    };
    let zero_mask_longs = vec![0i64; (total_bits + 63) / 64];

    let write_bitset = |buf: &mut Vec<u8>, longs: &Vec<i64>| {
        write_vi(buf, longs.len() as i32);
        for &l in longs {
            buf.extend_from_slice(&l.to_be_bytes());
        }
    };

    write_bitset(&mut buf, &zero_mask_longs);
    write_bitset(&mut buf, &zero_mask_longs);
    write_bitset(&mut buf, &empty_mask_longs);
    write_bitset(&mut buf, &empty_mask_longs);
    write_vi(&mut buf, 0);
    write_vi(&mut buf, 0);

    buf
}
