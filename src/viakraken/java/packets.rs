use tokio::net::TcpStream;

use crate::viakraken::java::types::LoginSuccessCore;
use crate::viakraken::utils::{
    json_escape, write_framed_payload, write_string, write_varint_buffer, ByteBuffer,
};

pub(super) fn build_login_success_packet(
    core: &LoginSuccessCore,
    strict_error_handling: bool,
) -> std::io::Result<ByteBuffer> {
    let mut payload = ByteBuffer::new();
    // Packet ID: LoginSuccess = 0x02
    write_varint_buffer(&mut payload, 0x02);
    // UUID (16 bytes)
    payload.extend_from_slice(core.uuid.as_bytes());
    // Username
    let mut username_buf = Vec::new();
    write_string(&mut username_buf, &core.username)?;
    payload.extend_from_slice(&username_buf);
    // Properties
    write_varint_buffer(
        &mut payload,
        core.properties.len() as i32,
    );
    for prop in &core.properties {
        let mut name_buf = Vec::new();
        write_string(&mut name_buf, &prop.name)?;
        payload.extend_from_slice(&name_buf);
        let mut value_buf = Vec::new();
        write_string(&mut value_buf, &prop.value)?;
        payload.extend_from_slice(&value_buf);
        if let Some(sig) = &prop.signature {
            let mut sig_buf = Vec::new();
            write_string(&mut sig_buf, sig)?;
            let mut entry = Vec::new();
            entry.push(1); // has_signature = true
            entry.extend_from_slice(&sig_buf);
            payload.extend_from_slice(&entry);
        } else {
            payload.push(0); // has_signature = false
        }
    }
    // Strict error handling flag
    payload.push(if strict_error_handling { 1 } else { 0 });
    Ok(payload)
}

pub(super) fn build_disconnect_packet(reason_json: &str) -> std::io::Result<ByteBuffer> {
    let mut payload = ByteBuffer::new();
    // Packet ID: Disconnect = 0x00
    write_varint_buffer(&mut payload, 0x00);
    // Reason JSON string
    let mut reason_buf = Vec::new();
    write_string(&mut reason_buf, reason_json)?;
    payload.extend_from_slice(&reason_buf);
    Ok(payload)
}

pub(super) fn build_known_packs_packet() -> ByteBuffer {
    let mut payload = ByteBuffer::new();
    // Packet ID: KnownPacks = 0x0E
    write_varint_buffer(&mut payload, 0x0E);
    // Empty known packs array
    write_varint_buffer(&mut payload, 0);
    payload
}

pub(super) fn build_finish_config_packet() -> ByteBuffer {
    let mut payload = ByteBuffer::new();
    // Packet ID: FinishConfiguration = 0x03
    write_varint_buffer(&mut payload, 0x03);
    payload
}

pub(super) async fn send_login_disconnect_json(
    stream: &mut TcpStream,
    reason: &str,
) -> std::io::Result<()> {
    let json_reason = format!(r#"{{"text":"{}"}}"#, json_escape(reason));
    let payload = build_disconnect_packet(&json_reason)?;
    write_framed_payload(stream, payload.as_slice()).await
}
