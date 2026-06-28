use std::io::{Error, ErrorKind};
use std::sync::Arc;

use pumpkin_protocol::java::client::config::{CFinishConfig, CKnownPacks, CRegistryData, RegistryEntry, CUpdateTags};
use pumpkin_protocol::java::client::login::CLoginSuccess;
use pumpkin_protocol::java::client::play::CLogin;
use pumpkin_protocol::java::server::config::SAcknowledgeFinishConfig;
use pumpkin_protocol::java::server::login::SLoginAcknowledged;
use pumpkin_protocol::{KnownPack, Property};
use pumpkin_protocol::codec::var_int::VarInt;
use pumpkin_util::version::MinecraftVersion;
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{timeout, Duration};
use uuid::Uuid;

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



pub async fn run_backend_listener(
    listener: TcpListener,
    config: Arc<ServerConfig>,
    backend_port: u16,
) -> std::io::Result<()> {
    let backend_addr = format!("0.0.0.0:{}", backend_port);
    log_info!("Kraken backend listening on {}", backend_addr);

    loop {
        let (stream, peer_addr) = listener.accept().await?;
        let cfg = config.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_backend_client(stream, cfg).await {
                log_warn!("Backend session {} closed with error: {}", peer_addr, e);
            }
        });
    }
}

async fn handle_backend_client(
    mut stream: TcpStream,
    config: Arc<ServerConfig>,
) -> std::io::Result<()> {
    let handshake_packet = read_packet(&mut stream).await?;
    let handshake = parse_handshake(&handshake_packet)?;

    match handshake.next_state {
        1 => handle_status(&mut stream, &config, handshake.protocol_version).await,
        2 => handle_login(&mut stream, &config, handshake.protocol_version).await,
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

async fn handle_login(
    stream: &mut TcpStream,
    config: &ServerConfig,
    protocol_version: i32,
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

    let login_play = CLogin::new(
        1, // entity_id
        false, // is_hardcore
        vec![
            "minecraft:overworld".to_string(),
            "minecraft:the_nether".to_string(),
            "minecraft:the_end".to_string(),
        ], // dimension_names
        VarInt(100), // max_players
        VarInt(10), // view_distance
        VarInt(10), // simulated_distance
        false, // reduced_debug_info
        true, // enabled_respawn_screen
        false, // limited_crafting
        pumpkin_data::dimension::Dimension::OVERWORLD, // dimension
        42, // hashed_seed
        1, // game_mode: Creative
        -1, // previous_gamemode
        false, // debug
        false, // is_flat
        None, // death_dimension_name
        VarInt(0), // portal_cooldown
        VarInt(63), // sealevel
        true, // enforce_secure_chat
    );

    let login_play_payload = encode_java_packet(&login_play, version)?;
    write_framed_payload(stream, login_play_payload.as_slice()).await?;

    log_info!(
        "Login flow completed for {} (protocol={}, max_players={})",
        username,
        protocol_version,
        config.max_players
    );

    let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
    let mut keep_alive_id = 0i64;
    let mut buf = [0u8; 1024];

    loop {
        tokio::select! {
            _ = interval.tick() => {
                keep_alive_id = keep_alive_id.wrapping_add(1);
                let keep_alive = pumpkin_protocol::java::client::play::CKeepAlive::new(keep_alive_id);
                if let Ok(payload) = encode_java_packet(&keep_alive, version) {
                    let _ = write_framed_payload(stream, payload.as_slice()).await;
                }
            }
            res = stream.read(&mut buf) => {
                match res {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        }
    }

    Ok(())
}
