use std::io::{Error, ErrorKind};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{Duration, timeout};

use crate::config::ServerConfig;
use crate::logger::{log_error, log_info, log_warn};

const NATIVE_PROTOCOL: i32 = 776;
const MAX_PACKET_LEN: i32 = 2_097_152;

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
    uuid: [u8; 16],
    username: String,
}

pub async fn handle_connection(
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

    if handshake.next_state == 2 && handshake.protocol_version == 763 {
        let reason = r#"{"text":"Kraken does not support protocol 763 (1.20.1)."}"#;
        send_login_disconnect(&mut client, reason).await?;
        log_warn!(
            "Rejected {} due to unsupported protocol {}",
            peer_addr,
            handshake.protocol_version
        );
        return Ok(());
    }

    if handshake.protocol_version == NATIVE_PROTOCOL {
        log_info!(
            "Native route {} protocol {} -> backend {}",
            peer_addr,
            handshake.protocol_version,
            backend_port
        );
    } else if (766..=775).contains(&handshake.protocol_version) {
        log_info!(
            "ViaKraken route {} protocol {} -> backend {}",
            peer_addr,
            handshake.protocol_version,
            backend_port
        );
    } else if handshake.next_state == 2 {
        let reason = format!(
            r#"{{"text":"Kraken supports login protocol 766..=776. You sent {}."}}"#,
            handshake.protocol_version
        );
        send_login_disconnect(&mut client, &reason).await?;
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
                let reason = format!(
                    r#"{{"text":"Kraken backend is unavailable on port {}. Please retry in a few seconds."}}"#,
                    backend_port
                );
                let _ = send_login_disconnect(&mut client, &reason).await;
            }
            log_error!(
                "Failed to connect proxy backend for {}: {}",
                peer_addr,
                e
            );
            return Ok(());
        }
    };

    write_framed_payload(&mut backend, &first_packet).await?;

    if handshake.next_state == 1 {
        relay_status_exchange(&mut client, &mut backend).await?;
        return Ok(());
    }

    let login_start = read_login_start_and_forward(&mut client, &mut backend).await?;
    let player = match relay_login_handoff(
        &mut client,
        &mut backend,
        handshake.protocol_version,
        &login_start.username,
    )
    .await?
    {
        Some(player_name) => player_name,
        None => return Ok(()),
    };

    if let Err(e) = client.set_nodelay(true) {
        log_warn!("Failed to set TCP_NODELAY for client {}: {}", peer_addr, e);
    }
    if let Err(e) = backend.set_nodelay(true) {
        log_warn!("Failed to set TCP_NODELAY for backend {}: {}", peer_addr, e);
    }

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

async fn relay_status_exchange(client: &mut TcpStream, backend: &mut TcpStream) -> std::io::Result<()> {
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
    let username = parse_login_start_username(&login_start)
        .unwrap_or_else(|_| "unknown-player".to_owned());

    write_framed_payload(backend, &login_start).await?;

    Ok(LoginStartInfo { username })
}

async fn relay_login_handoff(
    client: &mut TcpStream,
    backend: &mut TcpStream,
    protocol_version: i32,
    fallback_username: &str,
) -> std::io::Result<Option<String>> {
    loop {
        tokio::select! {
            backend_packet = read_packet(backend) => {
                let backend_packet = backend_packet?;
                let backend_id = packet_id(&backend_packet)?;

                if backend_id == 0x02 {
                    if protocol_version == NATIVE_PROTOCOL {
                        let core = parse_login_success_core_776(&backend_packet)?;
                        let rebuilt = build_login_success_776(&core)?;
                        write_framed_payload(client, &rebuilt).await?;
                        return Ok(Some(core.username));
                    }

                    write_framed_payload(client, &backend_packet).await?;
                    return Ok(Some(fallback_username.to_owned()));
                }

                write_framed_payload(client, &backend_packet).await?;
                if backend_id == 0x00 {
                    return Ok(None);
                }
            }

            client_packet = read_packet(client) => {
                let client_packet = client_packet?;
                write_framed_payload(backend, &client_packet).await?;
            }
        }
    }
}

fn parse_login_success_core_776(packet: &[u8]) -> std::io::Result<LoginSuccessCore> {
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

    let mut uuid = [0u8; 16];
    uuid.copy_from_slice(&packet[offset..offset + 16]);
    offset += 16;

    let username = read_string_from_slice(packet, &mut offset)?;
    let properties_count = read_varint_from_slice(packet, &mut offset)?;
    if properties_count < 0 {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "login success properties count cannot be negative",
        ));
    }

    for _ in 0..properties_count {
        let _name = read_string_from_slice(packet, &mut offset)?;
        let _value = read_string_from_slice(packet, &mut offset)?;
        let is_signed = read_bool_from_slice(packet, &mut offset)?;
        if is_signed {
            let _signature = read_string_from_slice(packet, &mut offset)?;
        }
    }

    let _strict_error_handling = read_bool_from_slice(packet, &mut offset)?;

    if offset != packet.len() {
        return Err(Error::new(
            ErrorKind::InvalidData,
            format!(
                "login success has {} trailing bytes after strictErrorHandling",
                packet.len() - offset
            ),
        ));
    }

    Ok(LoginSuccessCore { uuid, username })
}

fn build_login_success_776(core: &LoginSuccessCore) -> std::io::Result<Vec<u8>> {
    let mut payload = Vec::new();
    write_varint(&mut payload, 0x02);
    payload.extend_from_slice(&core.uuid);
    write_string(&mut payload, &core.username)?;
    write_varint(&mut payload, 0);
    payload.push(0x00);
    Ok(payload)
}

fn parse_login_start_username(packet: &[u8]) -> std::io::Result<String> {
    let mut offset = 0usize;
    let id = read_varint_from_slice(packet, &mut offset)?;
    if id != 0x00 {
        return Err(Error::new(
            ErrorKind::InvalidData,
            format!("expected login start packet id 0x00, got 0x{id:02x}"),
        ));
    }

    read_string_from_slice(packet, &mut offset)
}

async fn infer_bridge_failure_direction(client: &mut TcpStream, backend: &mut TcpStream) -> &'static str {
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

async fn send_login_disconnect(stream: &mut TcpStream, json_reason: &str) -> std::io::Result<()> {
    let mut payload = Vec::new();
    write_string(&mut payload, json_reason)?;
    write_packet(stream, 0x00, &payload).await
}

fn parse_handshake(payload: &[u8]) -> std::io::Result<HandshakeInfo> {
    let mut offset = 0usize;
    let packet_id = read_varint_from_slice(payload, &mut offset)?;
    if packet_id != 0 {
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

fn write_varint(buf: &mut Vec<u8>, value: i32) {
    let mut val = value as u32;
    loop {
        if (val & !0x7F) == 0 {
            buf.push(val as u8);
            break;
        }
        buf.push(((val & 0x7F) as u8) | 0x80);
        val >>= 7;
    }
}

fn read_varint_from_slice(data: &[u8], offset: &mut usize) -> std::io::Result<i32> {
    let mut num_read = 0;
    let mut result: i32 = 0;

    loop {
        if *offset >= data.len() {
            return Err(Error::new(
                ErrorKind::UnexpectedEof,
                "unexpected eof while reading varint",
            ));
        }

        let read = data[*offset];
        *offset += 1;

        let value = (read & 0x7F) as i32;
        result |= value << (7 * num_read);

        num_read += 1;
        if num_read > 5 {
            return Err(Error::new(ErrorKind::InvalidData, "varint too big"));
        }

        if (read & 0x80) == 0 {
            break;
        }
    }

    Ok(result)
}

async fn read_varint(stream: &mut TcpStream) -> std::io::Result<i32> {
    let mut num_read = 0;
    let mut result: i32 = 0;

    loop {
        let mut one = [0u8; 1];
        stream.read_exact(&mut one).await?;

        let value = (one[0] & 0x7F) as i32;
        result |= value << (7 * num_read);

        num_read += 1;
        if num_read > 5 {
            return Err(Error::new(ErrorKind::InvalidData, "varint too big"));
        }

        if (one[0] & 0x80) == 0 {
            break;
        }
    }

    Ok(result)
}

fn read_string_from_slice(data: &[u8], offset: &mut usize) -> std::io::Result<String> {
    let len = read_varint_from_slice(data, offset)?;
    if len < 0 {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "negative string length",
        ));
    }

    let len = len as usize;
    if *offset + len > data.len() {
        return Err(Error::new(
            ErrorKind::UnexpectedEof,
            "not enough bytes for string",
        ));
    }

    let value = std::str::from_utf8(&data[*offset..*offset + len])
        .map_err(|_| Error::new(ErrorKind::InvalidData, "invalid utf8 in string"))?
        .to_owned();

    *offset += len;
    Ok(value)
}

fn write_string(buf: &mut Vec<u8>, value: &str) -> std::io::Result<()> {
    let bytes = value.as_bytes();
    if bytes.len() > i32::MAX as usize {
        return Err(Error::new(ErrorKind::InvalidInput, "string too long"));
    }
    write_varint(buf, bytes.len() as i32);
    buf.extend_from_slice(bytes);
    Ok(())
}

async fn read_packet(stream: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let packet_len = read_varint(stream).await?;
    if !(0..=MAX_PACKET_LEN).contains(&packet_len) {
        return Err(Error::new(ErrorKind::InvalidData, "invalid packet length"));
    }

    let mut payload = vec![0u8; packet_len as usize];
    stream.read_exact(&mut payload).await?;
    Ok(payload)
}

async fn write_framed_payload(stream: &mut TcpStream, payload: &[u8]) -> std::io::Result<()> {
    if payload.len() > MAX_PACKET_LEN as usize {
        return Err(Error::new(ErrorKind::InvalidInput, "payload too large"));
    }
    let mut len_buf = Vec::with_capacity(5);
    write_varint(&mut len_buf, payload.len() as i32);
    stream.write_all(&len_buf).await?;
    stream.write_all(payload).await?;
    Ok(())
}

async fn write_packet(stream: &mut TcpStream, packet_id: i32, payload: &[u8]) -> std::io::Result<()> {
    let mut packet = Vec::with_capacity(8 + payload.len());
    write_varint(&mut packet, packet_id);
    packet.extend_from_slice(payload);
    write_framed_payload(stream, &packet).await
}

fn packet_id(packet: &[u8]) -> std::io::Result<i32> {
    let mut offset = 0usize;
    read_varint_from_slice(packet, &mut offset)
}

fn read_bool_from_slice(data: &[u8], offset: &mut usize) -> std::io::Result<bool> {
    if *offset >= data.len() {
        return Err(Error::new(
            ErrorKind::UnexpectedEof,
            "unexpected eof while reading bool",
        ));
    }

    let value = data[*offset];
    *offset += 1;

    match value {
        0x00 => Ok(false),
        0x01 => Ok(true),
        _ => Err(Error::new(
            ErrorKind::InvalidData,
            format!("invalid boolean byte 0x{value:02x}"),
        )),
    }
}