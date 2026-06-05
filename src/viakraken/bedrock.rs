use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

const RAKNET_MAGIC: &[u8; 16] = b"\x00\xff\xff\x00\xfe\xfe\xfe\xfe\xfd\xfd\xfd\xfd\x12\x34\x56\x78";
const BEDROCK_COMING_SOON: &str = "Bedrock Support Coming Soon";

pub async fn is_probably_bedrock(stream: &mut TcpStream) -> std::io::Result<bool> {
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

pub async fn handle_bedrock_disconnect(stream: &mut TcpStream) -> std::io::Result<()> {
    let reason = BEDROCK_COMING_SOON;
    let reason_bytes = reason.as_bytes();
    // Packet ID 0x05 + VarInt reason(2) + bool hide_screen(0) + bool skip_message(0)
    // + String: message + String: filtered_message(empty) + String: disconnect_message2(empty)
    let mut payload = Vec::with_capacity(32 + reason_bytes.len());
    // CDisconnectPlayer packet ID
    payload.push(0x05);
    // Reason enum: 0x02 = kick
    write_unsigned_varint(&mut payload, 2);
    // HideDisconnectionScreen: false
    payload.push(0x00);
    // SkipMessage: false
    payload.push(0x00);
    // Message: string (unsigned varint length prefix)
    write_unsigned_varint(&mut payload, reason_bytes.len() as u32);
    payload.extend_from_slice(reason_bytes);
    // FilteredMessage: empty string
    write_unsigned_varint(&mut payload, 0);
    // DisconnectMessage2: empty string (newer versions)
    write_unsigned_varint(&mut payload, 0);

    stream.write_all(&payload).await?;
    stream.flush().await?;
    let _ = stream.shutdown().await;
    Ok(())
}

fn write_unsigned_varint(buf: &mut Vec<u8>, mut value: u32) {
    loop {
        if value & !0x7F == 0 {
            buf.push(value as u8);
            break;
        }
        buf.push(((value & 0x7F) as u8) | 0x80);
        value >>= 7;
    }
}
