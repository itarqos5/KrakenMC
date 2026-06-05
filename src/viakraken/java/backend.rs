use std::io::{Error, ErrorKind};
use std::sync::Arc;

use tokio::net::{TcpListener, TcpStream};
use tokio::time::{timeout, Duration};
use uuid::Uuid;

use crate::config::ServerConfig;
use crate::logger::{log_info, log_warn};
use crate::viakraken::java::packets::{
    build_finish_config_packet, build_known_packs_packet, build_login_success_packet,
};
use crate::viakraken::java::protocol::{parse_handshake, parse_login_start};
use crate::viakraken::java::support::{
    minecraft_version_from_protocol, strict_error_handling, LOGIN_ACKNOWLEDGED_ID,
    CONFIG_FINISH_SERVERBOUND_ID,
};
use crate::viakraken::utils::{
    json_escape, packet_id, read_packet, read_varint_from_slice, write_framed_payload,
    write_packet, write_string,
};

const NATIVE_PROTOCOL: i32 = 775;

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
        r#"{{"version":{{"name":"26.1","protocol":{}}},"players":{{"max":{},"online":0,"sample":[]}},"description":{{"text":"{}"}}}}"#,
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
    let _version = minecraft_version_from_protocol(protocol_version)?;

    let login_start_packet = read_packet(stream).await?;
    let (username, claimed_uuid) = parse_login_start(&login_start_packet)?;
    let profile_uuid = claimed_uuid.unwrap_or_else(Uuid::new_v4);

    let strict = strict_error_handling(protocol_version);
    let core = crate::viakraken::java::types::LoginSuccessCore {
        uuid: profile_uuid,
        username: username.clone(),
        properties: Vec::new(),
    };
    let login_success_payload = build_login_success_packet(&core, strict)?;
    write_framed_payload(stream, login_success_payload.as_slice()).await?;

    // Wait for LoginAcknowledge
    if let Ok(Ok(login_ack_packet)) = timeout(Duration::from_secs(15), read_packet(stream)).await {
        let ack_id = packet_id(&login_ack_packet)?;
        if ack_id != LOGIN_ACKNOWLEDGED_ID {
            log_warn!(
                "Unexpected login packet after Login Success: id={} expected={} (user={})",
                ack_id,
                LOGIN_ACKNOWLEDGED_ID,
                username
            );
        }
    }

    // Send KnownPacks
    let known_packs_payload = build_known_packs_packet();
    write_framed_payload(stream, known_packs_payload.as_slice()).await?;

    // Send FinishConfig
    let finish_config_payload = build_finish_config_packet();
    write_framed_payload(stream, finish_config_payload.as_slice()).await?;

    let mut entered_play = false;
    if let Ok(Ok(config_finish_packet)) =
        timeout(Duration::from_secs(15), read_packet(stream)).await
    {
        let finish_id = packet_id(&config_finish_packet)?;
        if finish_id == CONFIG_FINISH_SERVERBOUND_ID {
            entered_play = true;
        } else {
            log_warn!(
                "Unexpected config packet for {}: id={} expected={}",
                username,
                finish_id,
                CONFIG_FINISH_SERVERBOUND_ID
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
