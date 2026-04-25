use std::io::{Error, ErrorKind};
use std::sync::Arc;

use pumpkin_protocol::java::client::config::{CFinishConfig, CKnownPacks};
use pumpkin_protocol::java::client::login::{CLoginDisconnect, CLoginSuccess};
use pumpkin_protocol::java::server::config::SAcknowledgeFinishConfig;
use pumpkin_protocol::java::server::login::SLoginAcknowledged;
use pumpkin_protocol::packet::MultiVersionJavaPacket;
use pumpkin_protocol::{ClientPacket, KnownPack, Property};
use pumpkin_util::version::MinecraftVersion;
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{timeout, Duration};
use uuid::Uuid;

use crate::config::ServerConfig;
use crate::logger::{log_error, log_info, log_warn};
use crate::viakraken::utils::{
    json_escape, packet_id, read_bool_from_slice, read_packet, read_string_from_slice,
    read_varint_from_slice, write_framed_payload, write_packet, write_string, write_varint_buffer,
    ByteBuffer,
};

const NATIVE_PROTOCOL: i32 = 776;

#[derive(Debug, Clone)]
struct HandshakeInfo {
    protocol_version: i32,
    next_state: i32,
}

#[derive(Debug, Clone)]
struct LoginStartInfo {
    username: String,
}

#[derive(Debug, Clone)]
struct LoginSuccessCore {
    uuid: Uuid,
    username: String,
    properties: Vec<Property>,
}

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

pub async fn handle_java_connection(
    mut client: TcpStream,
    peer_addr: std::net::SocketAddr,
    _config: Arc<ServerConfig>,
    backend_port: u16,
) -> std::io::Result<()> {
    let first_packet = read_packet(&mut client).await?;
    let handshake = parse_handshake(&first_packet)?;

    if handshake.next_state != 1 && handshake.next_state != 2 {
        log_warn!(
            "Rejected {} with invalid handshake next_state {}",
            peer_addr,
            handshake.next_state
        );
        return Ok(());
    }

    if handshake.next_state == 2 && !is_supported_login_protocol(handshake.protocol_version) {
        send_login_disconnect_json(
            &mut client,
            handshake.protocol_version,
            "Unsupported Java protocol. Supported: 766, 767, 774.",
        )
        .await?;
        log_warn!(
            "Rejected {} unsupported login protocol {}",
            peer_addr,
            handshake.protocol_version
        );
        return Ok(());
    }

    let mut backend = match TcpStream::connect(("127.0.0.1", backend_port)).await {
        Ok(stream) => stream,
        Err(e) => {
            if handshake.next_state == 2 {
                let _ = send_login_disconnect_json(
                    &mut client,
                    handshake.protocol_version,
                    "Backend unavailable. Please retry in a few seconds.",
                )
                .await;
            }
            log_error!("Failed to connect proxy backend for {}: {}", peer_addr, e);
            return Ok(());
        }
    };

    if let Err(e) = client.set_nodelay(true) {
        log_warn!("Failed to set TCP_NODELAY for client {}: {}", peer_addr, e);
    }
    if let Err(e) = backend.set_nodelay(true) {
        log_warn!("Failed to set TCP_NODELAY for backend {}: {}", peer_addr, e);
    }

    write_framed_payload(&mut backend, &first_packet).await?;

    if handshake.next_state == 1 {
        relay_status_exchange(&mut client, &mut backend).await?;
        return Ok(());
    }

    let login_start = read_login_start_and_forward(&mut client, &mut backend).await?;
    log_info!(
        "Player {} joining with Protocol {}",
        login_start.username,
        handshake.protocol_version
    );

    let version = minecraft_version_from_protocol(handshake.protocol_version)?;
    let player = match relay_login_handoff(
        &mut client,
        &mut backend,
        handshake.protocol_version,
        version,
        &login_start.username,
    )
    .await
    {
        Ok(Some(name)) => name,
        Ok(None) => return Ok(()),
        Err(e) if is_decoder_exception(&e) => {
            let _ = send_login_disconnect_json(
                &mut client,
                handshake.protocol_version,
                "DecoderException: malformed login packet",
            )
            .await;
            log_warn!(
                "DecoderException during login handoff for {} (protocol {}): {}",
                peer_addr,
                handshake.protocol_version,
                e
            );
            return Ok(());
        }
        Err(e) => return Err(e),
    };

    log_info!("Bridge established for {} ({})", player, peer_addr);

    let mut bridge = Box::pin(tokio::io::copy_bidirectional(&mut client, &mut backend));
    let bridge_result = bridge.as_mut().await;
    drop(bridge);

    match bridge_result {
        Ok((to_backend, to_client)) => {
            log_info!(
                "Bridge closed for {} ({}) (to_backend={} bytes, to_client={} bytes)",
                player,
                peer_addr,
                to_backend,
                to_client
            );
        }
        Err(e) => {
            let direction = infer_bridge_failure_direction(&mut client, &mut backend).await;
            log_warn!(
                "Bridge failed for {} ({}) direction={} error={}",
                player,
                peer_addr,
                direction,
                e
            );
        }
    }

    Ok(())
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

async fn relay_status_exchange(
    client: &mut TcpStream,
    backend: &mut TcpStream,
) -> std::io::Result<()> {
    let status_request = read_packet(client).await?;
    write_framed_payload(backend, &status_request).await?;

    let status_response = read_packet(backend).await?;
    write_framed_payload(client, &status_response).await?;

    if let Ok(Ok(ping_request)) = timeout(Duration::from_secs(10), read_packet(client)).await {
        write_framed_payload(backend, &ping_request).await?;
        let pong_response = read_packet(backend).await?;
        write_framed_payload(client, &pong_response).await?;
    }

    Ok(())
}

async fn read_login_start_and_forward(
    client: &mut TcpStream,
    backend: &mut TcpStream,
) -> std::io::Result<LoginStartInfo> {
    let login_start = read_packet(client).await?;
    let username =
        parse_login_start_username(&login_start).unwrap_or_else(|_| "Literal_u".to_owned());
    write_framed_payload(backend, &login_start).await?;
    Ok(LoginStartInfo { username })
}

async fn relay_login_handoff(
    client: &mut TcpStream,
    backend: &mut TcpStream,
    protocol_version: i32,
    version: MinecraftVersion,
    fallback_username: &str,
) -> std::io::Result<Option<String>> {
    enum Phase {
        Login,
        Config,
    }

    let login_success_id =
        packet_id_for_version::<CLoginSuccess<'static>>(version, "login-success")?;
    let login_disconnect_id =
        packet_id_for_version::<CLoginDisconnect>(version, "login-disconnect")?;
    let login_ack_id = packet_id_for_version::<SLoginAcknowledged>(version, "login-ack")?;
    let config_finish_clientbound_id =
        packet_id_for_version::<CFinishConfig>(version, "config-finish-clientbound")?;
    let config_finish_serverbound_id =
        packet_id_for_version::<SAcknowledgeFinishConfig>(version, "config-finish-serverbound")?;

    let mut phase = Phase::Login;
    let mut player_name: Option<String> = None;
    let mut backend_sent_finish_config = false;
    let mut client_sent_finish_config = false;

    loop {
        tokio::select! {
            backend_packet = read_packet(backend) => {
                let backend_packet = backend_packet?;
                let backend_id = packet_id(&backend_packet)?;

                if matches!(phase, Phase::Login) && backend_id == login_success_id {
                    let core = parse_backend_login_success(&backend_packet)?;
                    let strict_error_handling = strict_error_handling(protocol_version);
                    let rebuilt = build_login_success_packet(&core, version, strict_error_handling)?;
                    write_framed_payload(client, rebuilt.as_slice()).await?;

                    player_name = Some(core.username.clone());
                    log_info!("Configuration transition armed (state=3) for {}", core.username);
                    continue;
                }

                write_framed_payload(client, &backend_packet).await?;

                if matches!(phase, Phase::Login) && backend_id == login_disconnect_id {
                    return Ok(None);
                }

                if matches!(phase, Phase::Config) && backend_id == config_finish_clientbound_id {
                    backend_sent_finish_config = true;
                    if client_sent_finish_config {
                        return Ok(Some(player_name.unwrap_or_else(|| fallback_username.to_owned())));
                    }
                }
            }

            client_packet = read_packet(client) => {
                let client_packet = client_packet?;
                let client_id = packet_id(&client_packet)?;
                write_framed_payload(backend, &client_packet).await?;

                match phase {
                    Phase::Login => {
                        if client_id == login_ack_id && player_name.is_some() {
                            phase = Phase::Config;
                            log_info!(
                                "Client {} acknowledged login; entering configuration relay",
                                player_name.as_deref().unwrap_or(fallback_username)
                            );
                        }
                    }
                    Phase::Config => {
                        if client_id == config_finish_serverbound_id {
                            client_sent_finish_config = true;
                            if backend_sent_finish_config {
                                return Ok(Some(player_name.unwrap_or_else(|| fallback_username.to_owned())));
                            }
                        }
                    }
                }
            }
        }
    }
}

fn parse_backend_login_success(packet: &[u8]) -> std::io::Result<LoginSuccessCore> {
    let mut offset = 0usize;
    let id = read_varint_from_slice(packet, &mut offset)?;
    if id != 0x02 {
        return Err(Error::new(
            ErrorKind::InvalidData,
            format!("expected login success packet id 0x02, got 0x{id:02x}"),
        ));
    }

    if offset + 16 > packet.len() {
        return Err(Error::new(
            ErrorKind::UnexpectedEof,
            "login success missing 16-byte UUID",
        ));
    }

    let mut uuid_bytes = [0u8; 16];
    uuid_bytes.copy_from_slice(&packet[offset..offset + 16]);
    offset += 16;

    let username = read_string_from_slice(packet, &mut offset)?;
    let properties_count = read_varint_from_slice(packet, &mut offset)?;
    if properties_count < 0 {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "login success properties count cannot be negative",
        ));
    }

    let mut properties = Vec::with_capacity(properties_count as usize);
    for _ in 0..properties_count {
        let name = read_string_from_slice(packet, &mut offset)?;
        let value = read_string_from_slice(packet, &mut offset)?;
        let has_signature = read_bool_from_slice(packet, &mut offset)?;
        let signature = if has_signature {
            Some(read_string_from_slice(packet, &mut offset)?)
        } else {
            None
        };

        properties.push(Property {
            name,
            value,
            signature,
        });
    }

    if offset < packet.len() {
        let _ = read_bool_from_slice(packet, &mut offset)?;
    }

    if offset != packet.len() {
        return Err(Error::new(
            ErrorKind::InvalidData,
            format!("login success has {} trailing bytes", packet.len() - offset),
        ));
    }

    Ok(LoginSuccessCore {
        uuid: Uuid::from_bytes(uuid_bytes),
        username,
        properties,
    })
}

fn build_login_success_packet(
    core: &LoginSuccessCore,
    version: MinecraftVersion,
    strict_error_handling: bool,
) -> std::io::Result<ByteBuffer> {
    let packet = CLoginSuccess::new(
        &core.uuid,
        &core.username,
        &core.properties,
        strict_error_handling,
    );
    encode_java_packet(&packet, version)
}

fn encode_java_packet<P: ClientPacket>(
    packet: &P,
    version: MinecraftVersion,
) -> std::io::Result<ByteBuffer> {
    let packet_id = <P as MultiVersionJavaPacket>::to_id(version);
    if packet_id < 0 {
        return Err(Error::new(
            ErrorKind::InvalidData,
            format!(
                "packet id unavailable for version {}",
                version.protocol_version()
            ),
        ));
    }

    let mut payload = ByteBuffer::new();
    write_varint_buffer(&mut payload, packet_id);
    packet
        .write_packet_data(&mut payload, &version)
        .map_err(|e| Error::new(ErrorKind::InvalidData, e.to_string()))?;
    Ok(payload)
}

async fn send_login_disconnect_json(
    stream: &mut TcpStream,
    protocol_version: i32,
    reason: &str,
) -> std::io::Result<()> {
    let version =
        minecraft_version_from_protocol(protocol_version).unwrap_or(MinecraftVersion::V_1_21);
    let json_reason = format!(r#"{{"text":"{}"}}"#, json_escape(reason));
    let packet = CLoginDisconnect::new(json_reason);
    let payload = encode_java_packet(&packet, version)?;
    write_framed_payload(stream, payload.as_slice()).await
}

fn parse_handshake(payload: &[u8]) -> std::io::Result<HandshakeInfo> {
    let mut offset = 0usize;
    let packet_id = read_varint_from_slice(payload, &mut offset)?;
    if packet_id != 0x00 {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "first packet was not handshake",
        ));
    }

    let protocol_version = read_varint_from_slice(payload, &mut offset)?;
    let _server_addr = read_string_from_slice(payload, &mut offset)?;

    if offset + 2 > payload.len() {
        return Err(Error::new(
            ErrorKind::UnexpectedEof,
            "missing handshake port",
        ));
    }
    offset += 2;

    let next_state = read_varint_from_slice(payload, &mut offset)?;
    Ok(HandshakeInfo {
        protocol_version,
        next_state,
    })
}

fn parse_login_start(packet: &[u8]) -> std::io::Result<(String, Option<Uuid>)> {
    let mut offset = 0usize;
    let id = read_varint_from_slice(packet, &mut offset)?;
    if id != 0x00 {
        return Err(Error::new(
            ErrorKind::InvalidData,
            format!("expected login start packet id 0x00, got 0x{id:02x}"),
        ));
    }

    let username = read_string_from_slice(packet, &mut offset)?;
    let uuid = if offset + 16 <= packet.len() {
        let mut uuid_bytes = [0u8; 16];
        uuid_bytes.copy_from_slice(&packet[offset..offset + 16]);
        Some(Uuid::from_bytes(uuid_bytes))
    } else {
        None
    };

    Ok((username, uuid))
}

fn parse_login_start_username(packet: &[u8]) -> std::io::Result<String> {
    let (username, _) = parse_login_start(packet)?;
    Ok(username)
}

fn strict_error_handling(protocol_version: i32) -> bool {
    protocol_version == 774 || protocol_version >= 767
}

fn packet_id_for_version<P: MultiVersionJavaPacket>(
    version: MinecraftVersion,
    label: &str,
) -> std::io::Result<i32> {
    let id = <P as MultiVersionJavaPacket>::to_id(version);
    if id < 0 {
        return Err(Error::new(
            ErrorKind::InvalidData,
            format!(
                "packet {} unavailable for protocol {}",
                label,
                version.protocol_version()
            ),
        ));
    }
    Ok(id)
}

fn is_supported_login_protocol(protocol: i32) -> bool {
    matches!(protocol, 766 | 767 | 774)
}

fn minecraft_version_from_protocol(protocol: i32) -> std::io::Result<MinecraftVersion> {
    if protocol < 0 {
        return Err(Error::new(
            ErrorKind::InvalidData,
            format!("invalid protocol {protocol}"),
        ));
    }

    let version = MinecraftVersion::from_protocol(protocol as u32);
    if version == MinecraftVersion::Unknown {
        return Err(Error::new(
            ErrorKind::InvalidData,
            format!("unsupported protocol {protocol}"),
        ));
    }

    Ok(version)
}

fn is_decoder_exception(error: &Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::InvalidData | ErrorKind::UnexpectedEof
    ) || error.to_string().contains("DecoderException")
}

async fn infer_bridge_failure_direction(
    client: &mut TcpStream,
    backend: &mut TcpStream,
) -> &'static str {
    let client_closed = stream_looks_closed(client).await;
    let backend_closed = stream_looks_closed(backend).await;

    match (client_closed, backend_closed) {
        (true, false) => "client_to_backend",
        (false, true) => "backend_to_client",
        (true, true) => "both_sides",
        (false, false) => "unknown",
    }
}

async fn stream_looks_closed(stream: &mut TcpStream) -> bool {
    let mut buf = [0u8; 1];
    match timeout(Duration::from_millis(5), stream.peek(&mut buf)).await {
        Ok(Ok(0)) => true,
        Ok(Ok(_)) => false,
        Ok(Err(e)) => matches!(
            e.kind(),
            ErrorKind::ConnectionReset
                | ErrorKind::BrokenPipe
                | ErrorKind::NotConnected
                | ErrorKind::UnexpectedEof
        ),
        Err(_) => false,
    }
}
