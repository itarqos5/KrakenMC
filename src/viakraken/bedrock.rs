use std::io::{Error, ErrorKind};

use pumpkin_protocol::bedrock::{client::disconnect_player::CDisconnectPlayer, RAKNET_MAGIC};
use pumpkin_protocol::{packet::Packet, BClientPacket};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use crate::viakraken::utils::ByteBuffer;

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
