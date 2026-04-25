use std::io::{Error, ErrorKind};

use pumpkin_protocol::java::client::login::{CLoginDisconnect, CLoginSuccess};
use pumpkin_protocol::packet::MultiVersionJavaPacket;
use pumpkin_protocol::ClientPacket;
use pumpkin_util::version::MinecraftVersion;
use tokio::net::TcpStream;

use crate::viakraken::java::support::minecraft_version_from_protocol;
use crate::viakraken::java::types::LoginSuccessCore;
use crate::viakraken::utils::{json_escape, write_framed_payload, write_varint_buffer, ByteBuffer};

pub(super) fn build_login_success_packet(
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

pub(super) fn encode_java_packet<P: ClientPacket>(
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

pub(super) async fn send_login_disconnect_json(
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
