use std::io::{Error, ErrorKind};
use std::sync::Arc;

use pumpkin_protocol::java::client::config::{CFinishConfig, CKnownPacks};
use pumpkin_protocol::java::client::login::CLoginSuccess;
use pumpkin_protocol::java::server::config::SAcknowledgeFinishConfig;
use pumpkin_protocol::java::server::login::SLoginAcknowledged;
use pumpkin_protocol::{KnownPack, Property};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{timeout, Duration};
use uuid::Uuid;

use crate::config::ServerConfig;
use crate::logger::{log_info, log_warn};
use crate::viakraken::java::packets::encode_java_packet;
use crate::viakraken::java::protocol::{parse_handshake, parse_login_start};
use crate::viakraken::java::support::{
    minecraft_version_from_protocol, packet_id_for_version, strict_error_handling,
};
use crate::viakraken::utils::{
    json_escape, packet_id, read_packet, read_varint_from_slice, write_framed_payload,
    write_packet, write_string,
};

const NATIVE_PROTOCOL: i32 = 776;

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

    let motd = json_escape(&config.motd);
    let advertised_protocol = if protocol_version == NATIVE_PROTOCOL {
        NATIVE_PROTOCOL
    } else {
        protocol_version
    };
    let status_json = format!(
        r#"{{"version":{{"name":"1.21.1","protocol":{}}},"players":{{"max":{},"online":0,"sample":[]}},"description":{{"text":"{}"}}}}"#,
        advertised_protocol, config.max_players, motd
    );

    let mut payload = Vec::new();
    write_string(&mut payload, &status_json)?;
    write_packet(stream, 0x00, &payload).await?;

    if let Ok(Ok(ping_packet)) = timeout(Duration::from_secs(15), read_packet(stream)).await {
        let mut ping_offset = 0usize;
        let ping_id = read_varint_from_slice(&ping_packet, &mut ping_offset)?;
        if ping_id == 0x01 && ping_offset + 8 <= ping_packet.len() {
            let mut pong_payload = Vec::with_capacity(8);
            pong_payload.extend_from_slice(&ping_packet[ping_offset..ping_offset + 8]);
            write_packet(stream, 0x01, &pong_payload).await?;
        }
    }

    Ok(())
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

    let finish_config_packet = CFinishConfig;
    let finish_config_payload = encode_java_packet(&finish_config_packet, version)?;
    write_framed_payload(stream, finish_config_payload.as_slice()).await?;

    let mut entered_play = false;
    let config_finish_id =
        packet_id_for_version::<SAcknowledgeFinishConfig>(version, "config-finish")?;
    if let Ok(Ok(config_finish_packet)) =
        timeout(Duration::from_secs(15), read_packet(stream)).await
    {
        let finish_id = packet_id(&config_finish_packet)?;
        if finish_id == config_finish_id {
            entered_play = true;
        } else {
            log_warn!(
                "Unexpected config packet for {}: id={} expected={}",
                username,
                finish_id,
                config_finish_id
            );
        }
    }

    if !entered_play {
        log_warn!(
            "Did not receive config-finish from {}; transition to Play not confirmed",
            username
        );
    }

    log_info!(
        "Login flow completed for {} (protocol={}, max_players={})",
        username,
        protocol_version,
        config.max_players
    );
    Ok(())
}
