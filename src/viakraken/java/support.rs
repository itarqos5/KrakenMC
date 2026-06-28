use std::io::{Error, ErrorKind};

use pumpkin_protocol::packet::MultiVersionJavaPacket;
use pumpkin_util::version::MinecraftVersion;
use tokio::net::TcpStream;
use tokio::time::{timeout, Duration};

use crate::config::ServerConfig;
use crate::viakraken::utils::{
    json_escape, read_packet, read_varint_from_slice, write_packet, write_string,
};

pub(super) fn strict_error_handling(protocol_version: i32) -> bool {
    protocol_version == 774 || protocol_version >= 767
}

pub(super) fn packet_id_for_version<P: MultiVersionJavaPacket>(
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

pub(super) fn is_supported_login_protocol(protocol: i32) -> bool {
    matches!(protocol, 766 | 767 | 774 | 775)
}

pub(super) fn minecraft_version_from_protocol(protocol: i32) -> std::io::Result<MinecraftVersion> {
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

pub(super) fn is_decoder_exception(error: &Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::InvalidData | ErrorKind::UnexpectedEof
    ) || error.to_string().contains("DecoderException")
}

pub(super) async fn infer_bridge_failure_direction(
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

pub(crate) async fn send_status_response_direct(
    stream: &mut TcpStream,
    protocol_version: i32,
    config: &ServerConfig,
) -> std::io::Result<()> {
    let motd = json_escape(&config.motd);
    let is_supported = is_supported_login_protocol(protocol_version);

    let name = match protocol_version {
        766 => "1.20.5".to_string(),
        767 => "1.21".to_string(),
        774 => "1.21.11".to_string(),
        775 => "26.1".to_string(),
        776 => "1.21.11".to_string(),
        _ => {
            let mc_ver = MinecraftVersion::from_protocol(protocol_version as u32);
            if mc_ver == MinecraftVersion::Unknown {
                "1.21.11".to_string()
            } else {
                format!("{}", mc_ver)
            }
        }
    };

    let (ver_name, advertised_protocol) = if is_supported {
        (name, protocol_version)
    } else {
        (format!("Kraken {}", name), 775)
    };

    let status_json = format!(
        r#"{{"version":{{"name":"{}","protocol":{}}},"players":{{"max":{},"online":0,"sample":[]}},"description":{{"text":"{}"}}}}"#,
        ver_name, advertised_protocol, config.max_players, motd
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
