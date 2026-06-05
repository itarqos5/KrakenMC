use std::io::{Error, ErrorKind};

use tokio::net::TcpStream;
use tokio::time::{timeout, Duration};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProtocolVersion {
    V1_20_5,
    V1_21_0,
    V1_21_11,
    V26_1,
}

impl ProtocolVersion {
    pub fn from_protocol(protocol: i32) -> Option<Self> {
        match protocol {
            766 => Some(Self::V1_20_5),
            767 => Some(Self::V1_21_0),
            774 => Some(Self::V1_21_11),
            775 => Some(Self::V26_1),
            _ => None,
        }
    }
}

// Login packet IDs are stable across all supported protocol versions.
pub(super) const LOGIN_DISCONNECT_ID: i32 = 0x00;
pub(super) const LOGIN_SUCCESS_ID: i32 = 0x02;
pub(super) const LOGIN_ACKNOWLEDGED_ID: i32 = 0x03;

// Config packet IDs
pub(super) const CONFIG_FINISH_CLIENTBOUND_ID: i32 = 0x03;
pub(super) const CONFIG_FINISH_SERVERBOUND_ID: i32 = 0x03;

pub(super) fn strict_error_handling(protocol_version: i32) -> bool {
    protocol_version == 774 || protocol_version >= 767
}

pub(super) fn is_supported_login_protocol(protocol: i32) -> bool {
    ProtocolVersion::from_protocol(protocol).is_some()
}

pub(super) fn minecraft_version_from_protocol(protocol: i32) -> std::io::Result<ProtocolVersion> {
    ProtocolVersion::from_protocol(protocol).ok_or_else(|| {
        Error::new(
            ErrorKind::InvalidData,
            format!("unsupported protocol {protocol}"),
        )
    })
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
