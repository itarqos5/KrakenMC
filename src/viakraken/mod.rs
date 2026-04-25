use bevy_app::Plugin;
use std::io::{Error, ErrorKind};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{Duration, timeout};
use uuid::Uuid;

use crate::config::ServerConfig;
use crate::logger::{log_error, log_info, log_warn};

mod proxy;

const NATIVE_PROTOCOL: i32 = 776;
const MAX_PACKET_LEN: i32 = 2_097_152;
const COMPRESSION_THRESHOLD: i32 = -1;

#[derive(Debug, Clone)]
struct HandshakeInfo {
    protocol_version: i32,
    next_state: i32,
}

pub struct ViaKrakenPlugin {
    pub config: Arc<ServerConfig>,
}

impl Plugin for ViaKrakenPlugin {
    fn build(&self, _app: &mut bevy_app::App) {
        let backend_config = self.config.clone();
        std::thread::spawn(move || {
            let runtime = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(e) => {
                    log_error!("Failed to create backend runtime: {}", e);
                    return;
                }
            };

            runtime.block_on(async move {
                if let Err(e) = run_backend_listener(backend_config).await {
                    log_error!("Backend listener stopped: {}", e);
                }
            });
        });

        let proxy_config = self.config.clone();
        std::thread::spawn(move || {
            let runtime = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(e) => {
                    log_error!("Failed to create proxy runtime: {}", e);
                    return;
                }
            };

            runtime.block_on(async move {
                let vk = ViaKraken {
                    config: proxy_config,
                };
                vk.start().await;
            });
        });
    }
}

pub struct ViaKraken {
    pub config: Arc<ServerConfig>,
}

impl ViaKraken {
    pub async fn start(&self) {
        let proxy_addr = format!("0.0.0.0:{}", self.config.server_port);
        let listener = match TcpListener::bind(&proxy_addr).await {
            Ok(listener) => listener,
            Err(e) => {
                log_error!("Failed to bind proxy listener on {}: {}", proxy_addr, e);
                return;
            }
        };

        log_info!("ViaKraken proxy listening on {}", proxy_addr);

        loop {
            match listener.accept().await {
                Ok((stream, peer_addr)) => {
                    let config = self.config.clone();
                    tokio::spawn(async move {
                        if let Err(e) = proxy::handle_connection(stream, peer_addr, config).await {
                            log_error!("Proxy connection {} failed: {}", peer_addr, e);
                        }
                    });
                }
                Err(e) => {
                    log_error!("Failed accepting proxy client: {}", e);
                }
            }
        }
    }
}

async fn run_backend_listener(config: Arc<ServerConfig>) -> std::io::Result<()> {
    let backend_addr = format!("0.0.0.0:{}", config.server_port + 1);
    let listener = TcpListener::bind(&backend_addr).await?;

    log_info!(
        "Kraken backend listening on {} (compression threshold={})",
        backend_addr,
        COMPRESSION_THRESHOLD
    );

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

async fn handle_backend_client(mut stream: TcpStream, config: Arc<ServerConfig>) -> std::io::Result<()> {
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
        advertised_protocol,
        config.max_players,
        motd
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
    let login_start_packet = read_packet(stream).await?;
    let (username, claimed_uuid) = parse_login_start(&login_start_packet)?;

    let profile_uuid = claimed_uuid.unwrap_or_else(Uuid::new_v4);

    let mut login_success_payload = Vec::new();
    login_success_payload.extend_from_slice(profile_uuid.as_bytes());
    write_string(&mut login_success_payload, &username)?;
    write_varint(&mut login_success_payload, 0);
    login_success_payload.push(0);
    write_packet(stream, 0x02, &login_success_payload).await?;

    if let Ok(Ok(login_ack_packet)) = timeout(Duration::from_secs(15), read_packet(stream)).await {
        let mut ack_offset = 0usize;
        let ack_id = read_varint_from_slice(&login_ack_packet, &mut ack_offset)?;
        if ack_id != 0x03 {
            log_warn!(
                "Unexpected login packet after Login Success: id={} (user={})",
                ack_id,
                username
            );
        }
    }

    let brand = if protocol_version == NATIVE_PROTOCOL {
        "Kraken"
    } else {
        "Kraken (ViaKraken)"
    };

    let mut brand_data = Vec::new();
    write_string(&mut brand_data, brand)?;

    let mut custom_payload = Vec::new();
    write_string(&mut custom_payload, "minecraft:brand")?;
    custom_payload.extend_from_slice(&brand_data);
    write_packet(stream, 0x01, &custom_payload).await?;

    let mut known_packs_payload = Vec::new();
    write_varint(&mut known_packs_payload, 0);
    write_packet(stream, 0x0E, &known_packs_payload).await?;

    write_packet(stream, 0x03, &[]).await?;

    let _ = timeout(Duration::from_secs(15), read_packet(stream)).await;

    log_info!(
        "Login flow completed for {} (protocol={}, max_players={})",
        username,
        protocol_version,
        config.max_players
    );

    Ok(())
}

fn parse_handshake(payload: &[u8]) -> std::io::Result<HandshakeInfo> {
    let mut offset = 0usize;
    let packet_id = read_varint_from_slice(payload, &mut offset)?;
    if packet_id != 0x00 {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "expected handshake packet id 0",
        ));
    }

    let protocol_version = read_varint_from_slice(payload, &mut offset)?;
    let _host = read_string_from_slice(payload, &mut offset)?;

    if offset + 2 > payload.len() {
        return Err(Error::new(
            ErrorKind::UnexpectedEof,
            "handshake missing port",
        ));
    }
    offset += 2;

    let next_state = read_varint_from_slice(payload, &mut offset)?;

    Ok(HandshakeInfo {
        protocol_version,
        next_state,
    })
}

fn parse_login_start(payload: &[u8]) -> std::io::Result<(String, Option<Uuid>)> {
    let mut offset = 0usize;
    let packet_id = read_varint_from_slice(payload, &mut offset)?;
    if packet_id != 0x00 {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "expected login start packet id 0",
        ));
    }

    let username = read_string_from_slice(payload, &mut offset)?;
    let uuid = if offset + 16 <= payload.len() {
        let mut uuid_bytes = [0u8; 16];
        uuid_bytes.copy_from_slice(&payload[offset..offset + 16]);
        Some(Uuid::from_bytes(uuid_bytes))
    } else {
        None
    };

    Ok((username, uuid))
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

async fn write_packet(stream: &mut TcpStream, packet_id: i32, payload: &[u8]) -> std::io::Result<()> {
    let mut packet = Vec::with_capacity(8 + payload.len());
    write_varint(&mut packet, packet_id);
    packet.extend_from_slice(payload);

    let mut packet_len = Vec::with_capacity(5);
    write_varint(&mut packet_len, packet.len() as i32);

    stream.write_all(&packet_len).await?;
    stream.write_all(&packet).await?;
    Ok(())
}

fn json_escape(input: &str) -> String {
    input.replace('\\', "\\\\").replace('"', "\\\"")
}
