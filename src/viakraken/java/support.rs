use std::io::{Error, ErrorKind};

use pumpkin_protocol::packet::MultiVersionJavaPacket;
use pumpkin_util::version::MinecraftVersion;
use tokio::net::TcpStream;
use tokio::time::{timeout, Duration};

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
    matches!(protocol, 766 | 767 | 774)
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
