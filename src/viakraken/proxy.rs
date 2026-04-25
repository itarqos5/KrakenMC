use std::io::{Error, ErrorKind};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::config::ServerConfig;
use crate::logger::{log_error, log_info, log_warn};

const NATIVE_PROTOCOL: i32 = 776;
const MAX_PACKET_LEN: i32 = 2_097_152;

#[derive(Debug, Clone)]
struct HandshakeInfo {
    protocol_version: i32,
    next_state: i32,
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
            log_error!(
                "Failed to connect proxy backend for {}: {}",
                peer_addr,
                e
            );
            return Ok(());
        }
    };

    write_framed_payload(&mut backend, &first_packet).await?;

    match tokio::io::copy_bidirectional(&mut client, &mut backend).await {
        Ok((to_backend, to_client)) => {
            log_info!(
                "Bridge closed for {} (to_backend={} bytes, to_client={} bytes)",
                peer_addr,
                to_backend,
                to_client
            );
        }
        Err(e) => {
            log_warn!("Bridge terminated for {}: {}", peer_addr, e);
        }
    }

    Ok(())
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