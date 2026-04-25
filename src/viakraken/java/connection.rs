use std::sync::Arc;

use tokio::net::TcpStream;
use tokio::time::{timeout, Duration};

use crate::config::ServerConfig;
use crate::logger::{log_error, log_info, log_warn};
use crate::viakraken::java::handoff::relay_login_handoff;
use crate::viakraken::java::packets::send_login_disconnect_json;
use crate::viakraken::java::protocol::{parse_handshake, parse_login_start_username};
use crate::viakraken::java::support::{
    infer_bridge_failure_direction, is_decoder_exception, is_supported_login_protocol,
    minecraft_version_from_protocol,
};
use crate::viakraken::java::types::LoginStartInfo;
use crate::viakraken::utils::{read_packet, write_framed_payload};

pub async fn handle_java_connection(
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

    if handshake.next_state == 2 && !is_supported_login_protocol(handshake.protocol_version) {
        send_login_disconnect_json(
            &mut client,
            handshake.protocol_version,
            "Unsupported Java protocol. Supported: 766, 767, 774.",
        )
        .await?;
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
            if handshake.next_state == 2 {
                let _ = send_login_disconnect_json(
                    &mut client,
                    handshake.protocol_version,
                    "Backend unavailable. Please retry in a few seconds.",
                )
                .await;
            }
            log_error!("Failed to connect proxy backend for {}: {}", peer_addr, e);
            return Ok(());
        }
    };

    if let Err(e) = client.set_nodelay(true) {
        log_warn!("Failed to set TCP_NODELAY for client {}: {}", peer_addr, e);
    }
    if let Err(e) = backend.set_nodelay(true) {
        log_warn!("Failed to set TCP_NODELAY for backend {}: {}", peer_addr, e);
    }

    write_framed_payload(&mut backend, &first_packet).await?;

    if handshake.next_state == 1 {
        relay_status_exchange(&mut client, &mut backend).await?;
        return Ok(());
    }

    let login_start = read_login_start_and_forward(&mut client, &mut backend).await?;
    log_info!(
        "Player {} joining with Protocol {}",
        login_start.username,
        handshake.protocol_version
    );

    let version = minecraft_version_from_protocol(handshake.protocol_version)?;
    let player = match relay_login_handoff(
        &mut client,
        &mut backend,
        handshake.protocol_version,
        version,
        &login_start.username,
    )
    .await
    {
        Ok(Some(name)) => name,
        Ok(None) => return Ok(()),
        Err(e) if is_decoder_exception(&e) => {
            let _ = send_login_disconnect_json(
                &mut client,
                handshake.protocol_version,
                "DecoderException: malformed login packet",
            )
            .await;
            log_warn!(
                "DecoderException during login handoff for {} (protocol {}): {}",
                peer_addr,
                handshake.protocol_version,
                e
            );
            return Ok(());
        }
        Err(e) => return Err(e),
    };

    log_info!("Bridge established for {} ({})", player, peer_addr);

    let mut bridge = Box::pin(tokio::io::copy_bidirectional(&mut client, &mut backend));
    let bridge_result = bridge.as_mut().await;
    drop(bridge);

    match bridge_result {
        Ok((to_backend, to_client)) => {
            log_info!(
                "Bridge closed for {} ({}) (to_backend={} bytes, to_client={} bytes)",
                player,
                peer_addr,
                to_backend,
                to_client
            );
        }
        Err(e) => {
            let direction = infer_bridge_failure_direction(&mut client, &mut backend).await;
            log_warn!(
                "Bridge failed for {} ({}) direction={} error={}",
                player,
                peer_addr,
                direction,
                e
            );
        }
    }

    Ok(())
}

async fn relay_status_exchange(
    client: &mut TcpStream,
    backend: &mut TcpStream,
) -> std::io::Result<()> {
    let status_request = read_packet(client).await?;
    write_framed_payload(backend, &status_request).await?;

    let status_response = read_packet(backend).await?;
    write_framed_payload(client, &status_response).await?;

    if let Ok(Ok(ping_request)) = timeout(Duration::from_secs(10), read_packet(client)).await {
        write_framed_payload(backend, &ping_request).await?;
        let pong_response = read_packet(backend).await?;
        write_framed_payload(client, &pong_response).await?;
    }

    Ok(())
}

async fn read_login_start_and_forward(
    client: &mut TcpStream,
    backend: &mut TcpStream,
) -> std::io::Result<LoginStartInfo> {
    let login_start = read_packet(client).await?;
    let username =
        parse_login_start_username(&login_start).unwrap_or_else(|_| "Literal_u".to_owned());
    write_framed_payload(backend, &login_start).await?;
    Ok(LoginStartInfo { username })
}
