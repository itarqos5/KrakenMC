use std::io::{Error, ErrorKind};

use pumpkin_protocol::java::client::config::CFinishConfig;
use pumpkin_protocol::java::client::login::{CLoginDisconnect, CLoginSuccess};
use pumpkin_protocol::java::server::config::SAcknowledgeFinishConfig;
use pumpkin_protocol::java::server::login::SLoginAcknowledged;
use pumpkin_protocol::Property;
use pumpkin_util::version::MinecraftVersion;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use uuid::Uuid;

use crate::logger::log_info;
use crate::viakraken::java::packets::build_login_success_packet;
use crate::viakraken::java::support::{packet_id_for_version, strict_error_handling};
use crate::viakraken::java::types::LoginSuccessCore;
use crate::viakraken::utils::{
    packet_id, read_bool_from_slice, read_packet, read_string_from_slice, read_varint_from_slice,
    write_framed_payload,
};

pub(super) async fn relay_login_handoff(
    client: &mut TcpStream,
    backend: &mut TcpStream,
    protocol_version: i32,
    version: MinecraftVersion,
    fallback_username: &str,
) -> std::io::Result<Option<String>> {
    let login_success_id =
        packet_id_for_version::<CLoginSuccess<'static>>(version, "login-success")?;
    let login_disconnect_id =
        packet_id_for_version::<CLoginDisconnect>(version, "login-disconnect")?;
    let login_ack_id = packet_id_for_version::<SLoginAcknowledged>(version, "login-ack")?;
    let config_finish_clientbound_id =
        packet_id_for_version::<CFinishConfig>(version, "config-finish-clientbound")?;
    let config_finish_serverbound_id =
        packet_id_for_version::<SAcknowledgeFinishConfig>(version, "config-finish-serverbound")?;

    let mut player_name: Option<String> = None;
    let mut login_success_forwarded = false;

    loop {
        tokio::select! {
            backend_packet = read_packet(backend) => {
                let backend_packet = backend_packet?;
                let backend_id = packet_id(&backend_packet)?;

                if backend_id == login_success_id {
                    let core = parse_backend_login_success(&backend_packet)?;
                    let strict_error_handling = strict_error_handling(protocol_version);
                    let rebuilt = build_login_success_packet(&core, version, strict_error_handling)?;
                    write_framed_payload(client, rebuilt.as_slice()).await?;

                    player_name = Some(core.username.clone());
                    login_success_forwarded = true;
                    log_info!("Configuration transition armed (state=3) for {}", core.username);
                    continue;
                }

                write_framed_payload(client, &backend_packet).await?;

                if backend_id == login_disconnect_id {
                    return Ok(None);
                }
            }

            client_packet = read_packet(client) => {
                let client_packet = client_packet?;
                let client_id = packet_id(&client_packet)?;
                write_framed_payload(backend, &client_packet).await?;

                if login_success_forwarded && client_id == login_ack_id {
                    log_info!(
                        "Client {} acknowledged login; entering configuration relay",
                        player_name.as_deref().unwrap_or(fallback_username)
                    );
                    break;
                }
            }
        }
    }

    let mut backend_sent_finish_config = false;
    let mut client_sent_finish_config = false;

    loop {
        tokio::select! {
            backend_packet = read_packet(backend) => {
                let backend_packet = backend_packet?;
                let backend_id = packet_id(&backend_packet)?;

                // Transparent configuration relay: forward all backend packets as-is.
                write_framed_payload(client, &backend_packet).await?;

                if backend_id == config_finish_clientbound_id {
                    backend_sent_finish_config = true;
                    if client_sent_finish_config {
                        break;
                    }
                }
            }

            client_packet = read_packet(client) => {
                let client_packet = client_packet?;
                let client_id = packet_id(&client_packet)?;

                // Transparent configuration relay: forward all client packets as-is.
                write_framed_payload(backend, &client_packet).await?;

                if client_id == config_finish_serverbound_id {
                    client_sent_finish_config = true;
                    if backend_sent_finish_config {
                        break;
                    }
                }
            }
        }
    }

    client.flush().await?;
    backend.flush().await?;

    Ok(Some(
        player_name.unwrap_or_else(|| fallback_username.to_owned()),
    ))
}

fn parse_backend_login_success(packet: &[u8]) -> std::io::Result<LoginSuccessCore> {
    let mut offset = 0usize;
    let id = read_varint_from_slice(packet, &mut offset)?;
    if id != 0x02 {
        return Err(Error::new(
            ErrorKind::InvalidData,
            format!("expected login success packet id 0x02, got 0x{id:02x}"),
        ));
    }

    if offset + 16 > packet.len() {
        return Err(Error::new(
            ErrorKind::UnexpectedEof,
            "login success missing 16-byte UUID",
        ));
    }

    let mut uuid_bytes = [0u8; 16];
    uuid_bytes.copy_from_slice(&packet[offset..offset + 16]);
    offset += 16;

    let username = read_string_from_slice(packet, &mut offset)?;
    let properties_count = read_varint_from_slice(packet, &mut offset)?;
    if properties_count < 0 {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "login success properties count cannot be negative",
        ));
    }

    let mut properties = Vec::with_capacity(properties_count as usize);
    for _ in 0..properties_count {
        let name = read_string_from_slice(packet, &mut offset)?;
        let value = read_string_from_slice(packet, &mut offset)?;
        let has_signature = read_bool_from_slice(packet, &mut offset)?;
        let signature = if has_signature {
            Some(read_string_from_slice(packet, &mut offset)?)
        } else {
            None
        };

        properties.push(Property {
            name,
            value,
            signature,
        });
    }

    if offset < packet.len() {
        let _ = read_bool_from_slice(packet, &mut offset)?;
    }

    if offset != packet.len() {
        return Err(Error::new(
            ErrorKind::InvalidData,
            format!("login success has {} trailing bytes", packet.len() - offset),
        ));
    }

    Ok(LoginSuccessCore {
        uuid: Uuid::from_bytes(uuid_bytes),
        username,
        properties,
    })
}
