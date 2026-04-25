use std::io::{Error, ErrorKind};

use uuid::Uuid;

use crate::viakraken::java::types::HandshakeInfo;
use crate::viakraken::utils::{read_string_from_slice, read_varint_from_slice};

pub(super) fn parse_handshake(payload: &[u8]) -> std::io::Result<HandshakeInfo> {
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

pub(super) fn parse_login_start(packet: &[u8]) -> std::io::Result<(String, Option<Uuid>)> {
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

pub(super) fn parse_login_start_username(packet: &[u8]) -> std::io::Result<String> {
    let (username, _) = parse_login_start(packet)?;
    Ok(username)
}
