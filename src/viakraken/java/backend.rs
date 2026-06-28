use std::io::{Error, ErrorKind};
use std::sync::Arc;

use pumpkin_protocol::java::client::config::{CFinishConfig, CKnownPacks, CRegistryData, RegistryEntry, CUpdateTags};
use pumpkin_protocol::java::client::login::CLoginSuccess;
use pumpkin_protocol::java::client::play::{
    CAcknowledgeBlockChange, CBlockUpdate, CCenterChunk, CChunkBatchEnd, CChunkBatchStart,
    CGameEvent, CKeepAlive, CPlayerAbilities, CPlayerInfoUpdate, CPlayerPosition,
    CSystemChatMessage, CEntityStatus, CPlayerSpawnPosition, Player, PlayerInfoFlags,
    GameEvent, PlayerAction, CCustomPayload
};
use pumpkin_protocol::java::server::config::SAcknowledgeFinishConfig;
use pumpkin_protocol::java::server::login::SLoginAcknowledged;
use pumpkin_protocol::{KnownPack, Property};
use pumpkin_protocol::codec::var_int::VarInt;
use pumpkin_util::math::position::BlockPos;
use pumpkin_util::math::vector3::Vector3;
use pumpkin_util::text::TextComponent;
use pumpkin_util::version::MinecraftVersion;
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{timeout, Duration};
use tokio::sync::broadcast;
use std::sync::OnceLock;
use uuid::Uuid;

use pumpkin_protocol::java::server::play::SChatMessage;
use pumpkin_protocol::ServerPacket;

use crate::config::ServerConfig;
use crate::logger::{log_info, log_warn};
use crate::viakraken::java::packets::encode_java_packet;
use crate::viakraken::java::protocol::{parse_handshake, parse_login_start};
use crate::viakraken::java::support::{
    minecraft_version_from_protocol, packet_id_for_version, strict_error_handling,
    send_status_response_direct,
};
use crate::viakraken::utils::{
    packet_id, read_packet, read_varint_from_slice, write_framed_payload,
};
use crate::world::chunk_gen::encode_chunk_packet;
use crate::world::player_store::{load_player, save_player, PlayerData};

pub fn chat_channel() -> broadcast::Sender<String> {
    static CHAT_CHANNEL: OnceLock<broadcast::Sender<String>> = OnceLock::new();
    CHAT_CHANNEL.get_or_init(|| {
        let (tx, _) = broadcast::channel(100);
        tx
    }).clone()
}

pub async fn run_backend_listener(
    listener: TcpListener,
    config: Arc<ServerConfig>,
    backend_port: u16,
    db: Arc<sled::Db>,
) -> std::io::Result<()> {
    let backend_addr = format!("0.0.0.0:{}", backend_port);
    log_info!("Kraken backend listening on {}", backend_addr);

    loop {
        let (stream, peer_addr) = listener.accept().await?;
        let cfg = config.clone();
        let db = db.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_backend_client(stream, cfg, db).await {
                log_warn!("Backend session {} closed with error: {}", peer_addr, e);
            }
        });
    }
}

async fn handle_backend_client(
    mut stream: TcpStream,
    config: Arc<ServerConfig>,
    db: Arc<sled::Db>,
) -> std::io::Result<()> {
    let handshake_packet = read_packet(&mut stream).await?;
    let handshake = parse_handshake(&handshake_packet)?;

    match handshake.next_state {
        1 => handle_status(&mut stream, &config, handshake.protocol_version).await,
        2 => handle_login(&mut stream, &config, handshake.protocol_version, db).await,
        _ => Err(Error::new(
            ErrorKind::InvalidData,
            "invalid handshake next state",
        )),
    }
}

async fn handle_status(
    stream: &mut TcpStream,
    config: &ServerConfig,
    protocol_version: i32,
) -> std::io::Result<()> {
    let request_packet = read_packet(stream).await?;
    let mut offset = 0usize;
    let request_id = read_varint_from_slice(&request_packet, &mut offset)?;
    if request_id != 0x00 {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "expected status request packet id 0",
        ));
    }

    send_status_response_direct(stream, protocol_version, config).await
}


/// Build abilities byte for a gamemode.
/// 0=survival, 1=creative, 2=adventure, 3=spectator
fn gamemode_abilities(gamemode: u8) -> (i8, f32) {
    match gamemode {
        1 => (0x01 | 0x04 | 0x08, 0.05), // invulnerable + allow fly + instant break
        3 => (0x02 | 0x04, 0.05),         // flying + allow fly (spectator)
        _ => (0, 0.05),                    // survival/adventure: nothing
    }
}


async fn handle_login(
    stream: &mut TcpStream,
    config: &ServerConfig,
    protocol_version: i32,
    db: Arc<sled::Db>,
) -> std::io::Result<()> {
    let version = minecraft_version_from_protocol(protocol_version)?;

    let login_start_packet = read_packet(stream).await?;
    let (username, claimed_uuid) = parse_login_start(&login_start_packet)?;
    let profile_uuid = claimed_uuid.unwrap_or_else(Uuid::new_v4);
    let properties: Vec<Property> = Vec::new();

    let strict_error_handling = strict_error_handling(protocol_version);
    let login_success =
        CLoginSuccess::new(&profile_uuid, &username, &properties, strict_error_handling);
    let login_success_payload = encode_java_packet(&login_success, version)?;
    write_framed_payload(stream, login_success_payload.as_slice()).await?;

    let login_ack_id = packet_id_for_version::<SLoginAcknowledged>(version, "login-ack")?;
    if let Ok(Ok(login_ack_packet)) = timeout(Duration::from_secs(15), read_packet(stream)).await {
        let ack_id = packet_id(&login_ack_packet)?;
        if ack_id != login_ack_id {
            log_warn!(
                "Unexpected login packet after Login Success: id={} expected={} (user={})",
                ack_id,
                login_ack_id,
                username
            );
        }
    }

    let known_packs: [KnownPack<'static>; 0] = [];
    let known_packs_packet = CKnownPacks::new(&known_packs);
    let known_packs_payload = encode_java_packet(&known_packs_packet, version)?;
    write_framed_payload(stream, known_packs_payload.as_slice()).await?;

    // Send registry data
    let registry = pumpkin_data::registry::Registry::get_synced(version);
    for reg in registry {
        let entries: Vec<RegistryEntry> = reg
            .registry_entries
            .iter()
            .map(|r| RegistryEntry::new(r.entry_id.clone(), r.data.clone()))
            .collect();
        let packet = CRegistryData::new(&reg.registry_id, &entries);
        let payload = encode_java_packet(&packet, version)?;
        write_framed_payload(stream, payload.as_slice()).await?;
    }

    // Send tags
    let mut tags = vec![
        pumpkin_data::tag::RegistryKey::Block,
        pumpkin_data::tag::RegistryKey::Fluid,
        pumpkin_data::tag::RegistryKey::Enchantment,
        pumpkin_data::tag::RegistryKey::WorldgenBiome,
        pumpkin_data::tag::RegistryKey::Item,
        pumpkin_data::tag::RegistryKey::EntityType,
        pumpkin_data::tag::RegistryKey::Dialog,
    ];

    if version.protocol_version() >= MinecraftVersion::V_1_21_11.protocol_version() {
        if let Some(map) = pumpkin_data::tag::get_registry_key_tags(version, pumpkin_data::tag::RegistryKey::Timeline) {
            if !map.is_empty() {
                tags.push(pumpkin_data::tag::RegistryKey::Timeline);
            }
        }
    }
    if let Some(map) = pumpkin_data::tag::get_registry_key_tags(version, pumpkin_data::tag::RegistryKey::DimensionType) {
        if !map.is_empty() {
            tags.push(pumpkin_data::tag::RegistryKey::DimensionType);
        }
    }
    if let Some(map) = pumpkin_data::tag::get_registry_key_tags(version, pumpkin_data::tag::RegistryKey::DamageType) {
        if !map.is_empty() {
            tags.push(pumpkin_data::tag::RegistryKey::DamageType);
        }
    }
    if let Some(map) = pumpkin_data::tag::get_registry_key_tags(version, pumpkin_data::tag::RegistryKey::BannerPattern) {
        if !map.is_empty() {
            tags.push(pumpkin_data::tag::RegistryKey::BannerPattern);
        }
    }

    let tags_packet = CUpdateTags::new(&tags);
    let tags_payload = encode_java_packet(&tags_packet, version)?;
    write_framed_payload(stream, tags_payload.as_slice()).await?;

    let finish_config_packet = CFinishConfig;
    let finish_config_payload = encode_java_packet(&finish_config_packet, version)?;
    write_framed_payload(stream, finish_config_payload.as_slice()).await?;

    let mut entered_play = false;
    let config_finish_id =
        packet_id_for_version::<SAcknowledgeFinishConfig>(version, "config-finish")?;
    while let Ok(Ok(config_packet)) = timeout(Duration::from_secs(15), read_packet(stream)).await {
        let finish_id = packet_id(&config_packet)?;
        if finish_id == config_finish_id {
            entered_play = true;
            break;
        } else {
            log_info!("Received client config packet: id={} for {}", finish_id, username);
        }
    }

    if !entered_play {
        log_warn!(
            "Did not receive config-finish from {}; transition to Play not confirmed",
            username
        );
        return Ok(());
    }

    // ===== PLAY STATE =====
    handle_play(stream, config, version, protocol_version, &username, profile_uuid, db).await
}

async fn handle_play(
    stream: &mut TcpStream,
    config: &ServerConfig,
    version: MinecraftVersion,
    protocol_version: i32,
    username: &str,
    uuid: Uuid,
    db: Arc<sled::Db>,
) -> std::io::Result<()> {
    // Load persisted player data
    let mut player = load_player(&db, uuid);

    // Send CLogin (join game)
    let login_play = pumpkin_protocol::java::client::play::CLogin::new(
        1, // entity_id
        false,
        vec![
            "minecraft:overworld".to_string(),
            "minecraft:the_nether".to_string(),
            "minecraft:the_end".to_string(),
        ],
        VarInt(config.max_players as i32),
        VarInt(3),
        VarInt(3),
        false,
        true,
        false,
        pumpkin_data::dimension::Dimension::OVERWORLD,
        42,
        player.gamemode,
        -1,
        false,
        false,
        None,
        VarInt(0),
        VarInt(63),
        true,
    );
    let login_play_payload = encode_java_packet(&login_play, version)?;
    write_framed_payload(stream, login_play_payload.as_slice()).await?;

    // --- Player Info Update: add self to tablist ---
    {
        let flags = (PlayerInfoFlags::ADD_PLAYER
            | PlayerInfoFlags::UPDATE_LISTED
            | PlayerInfoFlags::UPDATE_GAME_MODE
            | PlayerInfoFlags::UPDATE_LATENCY)
            .bits();

        let actions = vec![
            PlayerAction::AddPlayer {
                name: username,
                properties: &[],
            },
            PlayerAction::UpdateGameMode(VarInt(player.gamemode as i32)),
            PlayerAction::UpdateListed(true),
            PlayerAction::UpdateLatency(VarInt(0)),
        ];

        let players = vec![Player {
            uuid,
            actions: &actions,
        }];

        let info_update = CPlayerInfoUpdate::new(flags, &players);
        let payload = encode_java_packet(&info_update, version)?;
        write_framed_payload(stream, payload.as_slice()).await?;
    }

    // --- Player Abilities ---
    {
        let (flags, fly_speed) = gamemode_abilities(player.gamemode);
        let abilities = CPlayerAbilities::new(flags, fly_speed, 0.1);
        let payload = encode_java_packet(&abilities, version)?;
        write_framed_payload(stream, payload.as_slice()).await?;
    }

    // --- Set Spawn Position ---
    {
        let spawn_pos = BlockPos(Vector3 { x: 0, y: 70, z: 0 });
        let spawn_pkt = CPlayerSpawnPosition::new(spawn_pos, 0.0, 0.0, "minecraft:overworld".to_string());
        let payload = encode_java_packet(&spawn_pkt, version)?;
        write_framed_payload(stream, payload.as_slice()).await?;
    }

    // --- Teleport player to persisted position ---
    {
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
    }

    // --- Start waiting for level chunks ---
    {
        let waiting = CGameEvent::new(GameEvent::StartWaitingChunks, 0.0);
        let payload = encode_java_packet(&waiting, version)?;
        write_framed_payload(stream, payload.as_slice()).await?;
    }

    // --- Center Chunk ---
    let chunk_x = (player.x as i32) >> 4;
    let chunk_z = (player.z as i32) >> 4;
    {
        let center = CCenterChunk { chunk_x: VarInt(chunk_x), chunk_z: VarInt(chunk_z) };
        let payload = encode_java_packet(&center, version)?;
        write_framed_payload(stream, payload.as_slice()).await?;
    }

    // --- Send nearby chunks (7x7 grid around player) with chunk batches ---
    let batch_start = CChunkBatchStart;
    let start_payload = encode_java_packet(&batch_start, version)?;
    write_framed_payload(stream, start_payload.as_slice()).await?;

    let mut chunk_count = 0u16;
    for dz in -3i32..=3 {
        for dx in -3i32..=3 {
            let cx = chunk_x + dx;
            let cz = chunk_z + dz;
            let chunk_packet_bytes = encode_chunk_packet(cx, cz, protocol_version);
            write_framed_payload(stream, &chunk_packet_bytes).await?;
            chunk_count += 1;
        }
    }

    let batch_end = CChunkBatchEnd::new(chunk_count);
    let end_payload = encode_java_packet(&batch_end, version)?;
    write_framed_payload(stream, end_payload.as_slice()).await?;

    // --- Enable F3+F4 Gamemode Switcher (OP level 4) ---
    {
        let op_status = CEntityStatus::new(1, 28);
        let payload = encode_java_packet(&op_status, version)?;
        write_framed_payload(stream, payload.as_slice()).await?;
    }

    // --- Server Branding ---
    {
        let brand_data = b"\x06Kraken";
        let brand_pkt = CCustomPayload::new("minecraft:brand", brand_data);
        let payload = encode_java_packet(&brand_pkt, version)?;
        write_framed_payload(stream, payload.as_slice()).await?;
    }

    log_info!(
        "Login flow completed for {} (protocol={}, max_players={}, pos=({:.1},{:.1},{:.1}))",
        username,
        protocol_version,
        config.max_players,
        player.x, player.y, player.z
    );

    // ===== PLAY LOOP =====
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
    let mut keep_alive_id = 0i64;
    let mut buf = vec![0u8; 65536];
    let mut pending_bytes = Vec::new();
    let mut chat_rx = chat_channel().subscribe();

    'play: loop {
        tokio::select! {
            Ok(msg) = chat_rx.recv() => {
                let text_comp = TextComponent::text(msg);
                let pkt = CSystemChatMessage::new(&text_comp, false);
                if let Ok(payload) = encode_java_packet(&pkt, version) {
                    let _ = write_framed_payload(stream, payload.as_slice()).await;
                }
            }
            _ = interval.tick() => {
                keep_alive_id = keep_alive_id.wrapping_add(1);
                let ka = CKeepAlive::new(keep_alive_id);
                if let Ok(payload) = encode_java_packet(&ka, version) {
                    let _ = write_framed_payload(stream, payload.as_slice()).await;
                }
            }
            res = stream.read(&mut buf) => {
                match res {
                    Ok(0) => break 'play,
                    Err(_) => break 'play,
                    Ok(n) => {
                        pending_bytes.extend_from_slice(&buf[..n]);
                        // Process all complete packets in the buffer
                        loop {
                            // Try to decode one packet from pending_bytes
                            let mut offset = 0usize;
                            let pkt_len = match read_varint_from_slice(&pending_bytes, &mut offset) {
                                Ok(len) if len > 0 => len as usize,
                                Ok(_) => { pending_bytes.clear(); break; }
                                Err(_) => break, // not enough data yet
                            };

                            if offset + pkt_len > pending_bytes.len() {
                                break; // incomplete packet, wait for more data
                            }

                            let pkt_data = pending_bytes[offset..offset + pkt_len].to_vec();
                            pending_bytes.drain(..offset + pkt_len);

                            // Dispatch the packet
                            if let Err(e) = handle_play_packet(
                                stream, version, &pkt_data,
                                &mut player, username, uuid, &db,
                            ).await {
                                log_warn!("Play packet error for {}: {}", username, e);
                            }
                        }
                    }
                }
            }
        }
    }

    // On disconnect: save player data
    save_player(&db, uuid, &player);
    log_info!("Player {} disconnected, state saved.", username);
    Ok(())
}

async fn handle_play_packet(
    stream: &mut TcpStream,
    version: MinecraftVersion,
    pkt_data: &[u8],
    player: &mut PlayerData,
    username: &str,
    _uuid: Uuid,
    _db: &Arc<sled::Db>,
) -> std::io::Result<()> {
    if pkt_data.is_empty() {
        return Ok(());
    }

    let mut offset = 0usize;
    let pid = read_varint_from_slice(pkt_data, &mut offset)?;
    let payload = &pkt_data[offset..];

    // Use packet ID tables from pumpkin-data for the player's version


    let sv_player_action = pumpkin_data::packet::serverbound::PLAY_PLAYER_ACTION.to_id(version);
    let sv_use_item_on = pumpkin_data::packet::serverbound::PLAY_USE_ITEM_ON.to_id(version);
    let sv_change_gm = pumpkin_data::packet::serverbound::PLAY_CHANGE_GAME_MODE.to_id(version);
    let sv_chat_cmd = pumpkin_data::packet::serverbound::PLAY_CHAT_COMMAND.to_id(version);
    let sv_pos_rot = pumpkin_data::packet::serverbound::PLAY_MOVE_PLAYER_POS_ROT.to_id(version);
    let sv_pos = pumpkin_data::packet::serverbound::PLAY_MOVE_PLAYER_POS.to_id(version);

    if pid == sv_player_action {
        // SPlayerAction: status(VarInt), pos(BlockPos=i64), face(u8), sequence(VarInt)
        let mut o = 0usize;
        let status = read_varint_from_slice(payload, &mut o).unwrap_or(0);
        // Read block pos (packed i64)
        if o + 8 <= payload.len() {
            let packed = i64::from_be_bytes(payload[o..o+8].try_into().unwrap_or_default());
            o += 8;
            let _face = if o < payload.len() { payload[o]; o += 1; } else { 0u8; };
            let sequence = read_varint_from_slice(payload, &mut o).unwrap_or(0);

            // StartedDigging or FinishedDigging or creative instant break
            if status == 0 || status == 2 {
                // Unpack BlockPos: x=26bit signed, y=12bit signed, z=26bit signed
                let x = (packed >> 38) as i32;
                let y = ((packed << 52) >> 52) as i32;
                let z = ((packed << 26) >> 38) as i32;
                let block_pos = BlockPos(Vector3 { x, y, z });

                // Acknowledge block change
                let ack = CAcknowledgeBlockChange::new(VarInt(sequence));
                let ack_payload = encode_java_packet(&ack, version)?;
                write_framed_payload(stream, ack_payload.as_slice()).await?;

                // Send block update (air = state 0)
                let update = CBlockUpdate::new(block_pos, VarInt(0));
                let update_payload = encode_java_packet(&update, version)?;
                write_framed_payload(stream, update_payload.as_slice()).await?;
            }
        }
    } else if pid == sv_use_item_on {
        // SUseItemOn: hand(VarInt), pos(BlockPos=i64), face(VarInt), cursor(3xf32), inside(bool), worldborder(bool), sequence(VarInt)
        let mut o = 0usize;
        let _hand = read_varint_from_slice(payload, &mut o).unwrap_or(0);
        if o + 8 <= payload.len() {
            let packed = i64::from_be_bytes(payload[o..o+8].try_into().unwrap_or_default());
            o += 8;
            let face = read_varint_from_slice(payload, &mut o).unwrap_or(0);
            // Skip cursor_pos (3 x f32 = 12 bytes), inside_block (bool), worldborder (bool)
            o += 14;
            let sequence = read_varint_from_slice(payload, &mut o).unwrap_or(0);

            // Unpack target block pos
            let x = (packed >> 38) as i32;
            let y = ((packed << 52) >> 52) as i32;
            let z = ((packed << 26) >> 38) as i32;

            // Compute adjacent block pos based on face
            let (nx, ny, nz) = match face {
                0 => (x, y - 1, z), // -Y (bottom)
                1 => (x, y + 1, z), // +Y (top)
                2 => (x, y, z - 1), // -Z (north)
                3 => (x, y, z + 1), // +Z (south)
                4 => (x - 1, y, z), // -X (west)
                5 => (x + 1, y, z), // +X (east)
                _ => (x, y + 1, z),
            };
            let place_pos = BlockPos(Vector3 { x: nx, y: ny, z: nz });

            // Acknowledge the placement
            if sequence > 0 {
                let ack = CAcknowledgeBlockChange::new(VarInt(sequence));
                let ack_payload = encode_java_packet(&ack, version)?;
                write_framed_payload(stream, ack_payload.as_slice()).await?;
            }
            
            // Note: We no longer force the block to Stone! The client will keep the block it placed locally.
            // A full implementation would track the block state in world chunks.
        }
    } else if pid == sv_change_gm && sv_change_gm >= 0 {
        // SChangeGameMode: gamemode (VarInt)
        let mut o = 0usize;
        let gm = read_varint_from_slice(payload, &mut o).unwrap_or(0) as u8;
        change_gamemode(stream, version, player, gm).await?;
        log_info!("{} changed gamemode to {}", username, gm);
    } else if pid == sv_chat_cmd {
        // SChatCommand: command (String)
        let mut o = 0usize;
        // String = VarInt length + bytes
        let cmd_len = read_varint_from_slice(payload, &mut o).unwrap_or(0) as usize;
        if o + cmd_len <= payload.len() {
            if let Ok(cmd) = std::str::from_utf8(&payload[o..o + cmd_len]) {
                handle_command(stream, version, player, cmd, username).await?;
            }
        }
    } else if pid == sv_pos_rot {
        // SPlayerPositionRotation: x(f64), y(f64), z(f64), yaw(f32), pitch(f32), collision(u8)
        if payload.len() >= 33 {
            player.x = f64::from_be_bytes(payload[0..8].try_into().unwrap_or_default());
            player.y = f64::from_be_bytes(payload[8..16].try_into().unwrap_or_default());
            player.z = f64::from_be_bytes(payload[16..24].try_into().unwrap_or_default());
            player.yaw = f32::from_be_bytes(payload[24..28].try_into().unwrap_or_default());
            player.pitch = f32::from_be_bytes(payload[28..32].try_into().unwrap_or_default());
            
            let on_ground = payload[32] != 0;
            process_fall_damage(stream, version, player, on_ground).await?;
        }
    } else if pid == sv_pos {
        // SPlayerPosition: x, y, z, collision
        if payload.len() >= 25 {
            player.x = f64::from_be_bytes(payload[0..8].try_into().unwrap_or_default());
            player.y = f64::from_be_bytes(payload[8..16].try_into().unwrap_or_default());
            player.z = f64::from_be_bytes(payload[16..24].try_into().unwrap_or_default());
            
            let on_ground = payload[24] != 0;
            process_fall_damage(stream, version, player, on_ground).await?;
        }
    } else if pid == pumpkin_data::packet::serverbound::PLAY_MOVE_PLAYER_ROT.to_id(version) {
        // SPlayerRotation: yaw, pitch, collision
        if payload.len() >= 8 {
            player.yaw = f32::from_be_bytes(payload[0..4].try_into().unwrap_or_default());
            player.pitch = f32::from_be_bytes(payload[4..8].try_into().unwrap_or_default());
        }
    }

    if player.y < -64.0 {
        // Reset player if they fall into the void (since chunk generation is limited to spawn area)
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
    } else if pid == pumpkin_data::packet::serverbound::PLAY_CHAT.to_id(version) {
        if let Ok(msg) = SChatMessage::read(&mut std::io::Cursor::new(payload), &version) {
            let _ = chat_channel().send(format!("<{}> {}", username, msg.message));
        }
    }
    // All other packets (keep-alive echo, client info, etc.) are silently discarded.

    Ok(())
}

async fn change_gamemode(
    stream: &mut TcpStream,
    version: MinecraftVersion,
    player: &mut PlayerData,
    gm: u8,
) -> std::io::Result<()> {
    player.gamemode = gm;

    // Send CGameEvent(ChangeGameMode, value)
    let ge = CGameEvent::new(GameEvent::ChangeGameMode, gm as f32);
    let payload = encode_java_packet(&ge, version)?;
    write_framed_payload(stream, payload.as_slice()).await?;

    // Send updated abilities
    let (flags, fly_speed) = gamemode_abilities(gm);
    let abilities = CPlayerAbilities::new(flags, fly_speed, 0.1);
    let payload = encode_java_packet(&abilities, version)?;
    write_framed_payload(stream, payload.as_slice()).await?;

    Ok(())
}

async fn handle_command(
    stream: &mut TcpStream,
    version: MinecraftVersion,
    player: &mut PlayerData,
    cmd: &str,
    username: &str,
) -> std::io::Result<()> {
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    if parts.is_empty() {
        return Ok(());
    }

    match parts[0] {
        "gamemode" | "gm" if parts.len() >= 2 => {
            let gm = match parts[1] {
                "survival" | "s" | "0" => Some(0u8),
                "creative" | "c" | "1" => Some(1u8),
                "adventure" | "a" | "2" => Some(2u8),
                "spectator" | "sp" | "3" => Some(3u8),
                _ => None,
            };
            if let Some(gm_id) = gm {
                change_gamemode(stream, version, player, gm_id).await?;
                let msg_text = format!("Game mode changed to {}", parts[1]);
                send_system_message(stream, version, &msg_text).await?;
                log_info!("{}: /gamemode {}", username, parts[1]);
            } else {
                send_system_message(stream, version, "Unknown gamemode. Use: survival, creative, adventure, spectator").await?;
            }
        }
        _ => {
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
) -> std::io::Result<()> {
    if on_ground {
        let fall_dist = player.highest_y - player.y;
        if fall_dist > 3.0 && player.gamemode == 0 { // Survival mode only
            // Send damage animation (entity status 2)
            let status = CEntityStatus::new(1, 2);
            let payload = encode_java_packet(&status, version)?;
            write_framed_payload(stream, payload.as_slice()).await?;
        }
        player.highest_y = player.y;
    } else {
        if player.y > player.highest_y {
            player.highest_y = player.y;
        }
    }
    Ok(())
}

async fn send_system_message(
    stream: &mut TcpStream,
    version: MinecraftVersion,
    text: &str,
) -> std::io::Result<()> {
    let content = TextComponent::text(text.to_owned());
    let msg = CSystemChatMessage::new(&content, false);
    let payload = encode_java_packet(&msg, version)?;
    write_framed_payload(stream, payload.as_slice()).await
}
