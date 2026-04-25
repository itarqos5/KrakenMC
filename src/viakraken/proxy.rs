use std::io::{Error, ErrorKind, Write};
use std::sync::Arc;

use bytes::BytesMut;
use pumpkin_protocol::bedrock::{client::disconnect_player::CDisconnectPlayer, RAKNET_MAGIC};
use pumpkin_protocol::java::client::login::{CLoginDisconnect, CLoginSuccess};
use pumpkin_protocol::packet::{MultiVersionJavaPacket, Packet};
use pumpkin_protocol::{BClientPacket, ClientPacket, Property};
use pumpkin_util::version::MinecraftVersion;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{Duration, timeout};
use uuid::Uuid;

use crate::config::ServerConfig;
use crate::logger::{log_error, log_info, log_warn};

const MAX_PACKET_LEN: i32 = 2_097_152;
const BEDROCK_COMING_SOON: &str = "Bedrock Support Coming Soon";

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

#[derive(Debug, Clone, Default)]
struct ByteBuffer {
    inner: BytesMut,
}

impl ByteBuffer {
    fn new() -> Self {
        Self {
            inner: BytesMut::new(),
        }
    }

    fn push(&mut self, byte: u8) {
        self.inner.extend_from_slice(&[byte]);
    }

    fn extend_from_slice(&mut self, bytes: &[u8]) {
        self.inner.extend_from_slice(bytes);
    }

    fn as_slice(&self) -> &[u8] {
        self.inner.as_ref()
    }
}

impl Write for ByteBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.inner.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub async fn handle_connection(
    mut client: TcpStream,
    peer_addr: std::net::SocketAddr,
    _config: Arc<ServerConfig>,
    backend_port: u16,
) -> std::io::Result<()> {
    if is_probably_bedrock(&mut client).await? {
        log_info!("Bedrock ingress detected from {}", peer_addr);
        handle_bedrock_handler(&mut client).await?;
        return Ok(());
    }

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
            log_error!(
                "Failed to connect proxy backend for {}: {}",
                peer_addr,
                e
            );
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
    let username = parse_login_start_username(&login_start)
        .unwrap_or_else(|_| "Literal_u".to_owned());

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
    loop {
        tokio::select! {
            backend_packet = read_packet(backend) => {
                let backend_packet = backend_packet?;
                let backend_id = packet_id(&backend_packet)?;

                if backend_id == 0x02 {
                    let core = parse_backend_login_success(&backend_packet)?;
                    let strict_error_handling = protocol_version >= 767;
                    let rebuilt = build_login_success_packet(&core, version, strict_error_handling)?;

                    write_framed_payload(client, rebuilt.as_slice()).await?;
                    log_info!("Configuration transition armed (state=3) for {}", core.username);

                    return Ok(Some(core.username));
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
            format!("packet id unavailable for version {}", version.protocol_version()),
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

fn write_varint_buffer(buf: &mut ByteBuffer, value: i32) {
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
        return Err(Error::new(ErrorKind::InvalidData, "negative string length"));
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

    let mut frame = ByteBuffer::new();
    write_varint_buffer(&mut frame, payload.len() as i32);
    frame.extend_from_slice(payload);

    stream.write_all(frame.as_slice()).await?;
    Ok(())
}

fn packet_id(packet: &[u8]) -> std::io::Result<i32> {
    let mut offset = 0usize;
    read_varint_from_slice(packet, &mut offset)
}

async fn is_probably_bedrock(stream: &mut TcpStream) -> std::io::Result<bool> {
    let mut sniff = [0u8; 32];
    let count = stream.peek(&mut sniff).await?;
    if count == 0 {
        return Ok(false);
    }

    let data = &sniff[..count];
    let first = data[0];
    let raknet_offline_id = matches!(first, 0x01 | 0x05 | 0x07 | 0x09 | 0x1c);
    let has_magic = data
        .windows(RAKNET_MAGIC.len())
        .any(|window| window == RAKNET_MAGIC.as_slice());

    Ok(raknet_offline_id || has_magic)
}

async fn handle_bedrock_handler(stream: &mut TcpStream) -> std::io::Result<()> {
    let packet = CDisconnectPlayer::new(2, BEDROCK_COMING_SOON.to_owned());

    let mut response = ByteBuffer::new();
    response.push(CDisconnectPlayer::PACKET_ID as u8);
    packet
        .write_packet(&mut response)
        .map_err(|e| Error::new(ErrorKind::InvalidData, e.to_string()))?;

    stream.write_all(response.as_slice()).await?;
    stream.flush().await?;
    let _ = stream.shutdown().await;

    Ok(())
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
    matches!(error.kind(), ErrorKind::InvalidData | ErrorKind::UnexpectedEof)
        || error.to_string().contains("DecoderException")
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

fn json_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out
}
