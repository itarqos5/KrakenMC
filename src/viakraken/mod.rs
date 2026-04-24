use std::sync::Arc;
use tokio::net::TcpListener;

use crate::config::ServerConfig;
use crate::logger::{log_error, log_info, log_warn};

pub struct ViaKraken {
    pub config: Arc<ServerConfig>,
}

impl ViaKraken {
    pub fn new(config: ServerConfig) -> Self {
        Self {
            config: Arc::new(config),
        }
    }

    pub async fn start(&self) {
        log_info!("Loading ViaKraken...");
        
        let addr = format!("{}:{}", self.config.server_ip, self.config.server_port);
        let listener = match TcpListener::bind(&addr).await {
            Ok(l) => l,
            Err(e) => {
                log_error!("Failed to bind to {}: {}", addr, e);
                return;
            }
        };

        log_info!("ViaKraken listening natively on {}", addr);

        loop {
            match listener.accept().await {
                Ok((stream, peer_addr)) => {
                    let config_clone = self.config.clone();
                    tokio::spawn(async move {
                        if let Err(e) = Self::handle_connection(stream, peer_addr, config_clone).await {
                            log_error!("Connection error from {}: {}", peer_addr, e);
                        }
                    });
                }
                Err(e) => {
                    log_error!("Failed to accept incoming connection: {}", e);
                }
            }
        }
    }

    async fn handle_connection(
        mut stream: tokio::net::TcpStream,
        peer_addr: std::net::SocketAddr,
        config: Arc<ServerConfig>,
    ) -> std::io::Result<()> {
        let target_protocol = config.target_protocol;

        // Read lengths & varints here would be placed here in a real scenario
        // E.g. "peek" handshake
        let mut buf = bytes::BytesMut::with_capacity(512);
        buf.resize(256, 0); // Temporary fill
        let n = stream.peek(&mut buf).await?;
        if n == 0 {
            return Ok(());
        }

        // Helper to read VarInt
        let read_varint = |offset: &mut usize| -> Option<i32> {
            let mut value = 0;
            let mut position = 0;
            loop {
                if *offset >= n { return None; }
                let current_byte = buf[*offset];
                *offset += 1;
                value |= ((current_byte & 0x7F) as i32) << position;
                if (current_byte & 0x80) == 0 { break; }
                position += 7;
                if position >= 32 { return None; }
            }
            Some(value)
        };

        let mut protocol = 0;
        let mut old_protocol_len = 0;
        let mut protocol_offset = 0;
        let mut packet_len = 0;
        let mut packet_len_offset = 0;
        let mut next_state = 0;

        let mut offset = 0;
        if let Some(len) = read_varint(&mut offset) {
            packet_len = len;
            packet_len_offset = offset;
            let _id_offset = offset;
            if let Some(id) = read_varint(&mut offset) {
                if id == 0 {
                    protocol_offset = offset;
                    if let Some(ver) = read_varint(&mut offset) {
                        protocol = ver;
                        old_protocol_len = offset - protocol_offset;
                        if let Some(str_len) = read_varint(&mut offset) {
                            offset += str_len as usize;
                            offset += 2;
                            if let Some(s) = read_varint(&mut offset) {
                                next_state = s;
                            }
                        }
                    }
                } else if id == 254 {
                    // Legacy server list ping fallback
                    protocol = -1;
                }
            }
        }

        // Now we actually consume the packet if it's a normal handshake
        let total_handshake_bytes = packet_len_offset + packet_len as usize;
        let mut handshake_data = vec![0u8; total_handshake_bytes];
        if protocol > 0 {
            use tokio::io::AsyncReadExt;
            stream.read_exact(&mut handshake_data).await?;
            
            log_warn!("Detected connection from {} with protocol {}", peer_addr, protocol);
            // Accepted: backend target directly, or versions we can attempt to rewrite.
            let supported = protocol == target_protocol || (763..=776).contains(&protocol);
            if !supported {
                log_warn!("Rejected connection from {} (Unsupported Protocol: {})", peer_addr, protocol);
                return Ok(()); // Drop connection
            }
            
            if protocol != target_protocol {
                if next_state == 2 {
                    // Quick sleep or consume Login Start so we don't cause a TCP RST
                    use tokio::io::AsyncReadExt;
                    let mut dummy_buf = [0u8; 1024];
                    let _ = tokio::time::timeout(std::time::Duration::from_millis(50), stream.read(&mut dummy_buf)).await;

                    // Return a nice disconnect message rather than closing TCP blindly
                    let reason = format!(r#"{{"text":"ViaKraken protocol translation ({} -> {}) is still a WIP! Please join on 1.20.1."}}"#, protocol, target_protocol);
                    let mut reason_data = vec![];
                    
                    // Disconnect packet in Login State is ID 0x00
                    reason_data.push(0x00);
                    
                    // Encode reason string length
                    let mut val = reason.len() as u32;
                    loop {
                        let mut temp = (val & 0b01111111) as u8;
                        val >>= 7;
                        if val != 0 { temp |= 0b10000000; }
                        reason_data.push(temp);
                        if val == 0 { break; }
                    }
                    
                    reason_data.extend_from_slice(reason.as_bytes());

                    // Encode packet length
                    let mut final_pkt = vec![];
                    let mut val = reason_data.len() as u32;
                    loop {
                        let mut temp = (val & 0b01111111) as u8;
                        val >>= 7;
                        if val != 0 { temp |= 0b10000000; }
                        final_pkt.push(temp);
                        if val == 0 { break; }
                    }
                    final_pkt.extend_from_slice(&reason_data);

                    use tokio::io::AsyncWriteExt;
                    let _ = stream.write_all(&final_pkt).await;
                    let _ = stream.shutdown().await;
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    return Ok(());
                }
                
                log_info!(
                    "ViaKraken starting translation stream for {} -> {}",
                    protocol,
                    target_protocol
                );
                
                // Encode new backend protocol version.
                let mut new_prot_bytes = vec![];
                let mut val = target_protocol as u32;
                loop {
                    let mut temp = (val & 0b01111111) as u8;
                    val >>= 7;
                    if val != 0 { temp |= 0b10000000; }
                    new_prot_bytes.push(temp);
                    if val == 0 { break; }
                }

                // Payload part
                let id_bytes = &handshake_data[packet_len_offset..protocol_offset];
                let rest_of_packet = &handshake_data[(protocol_offset + old_protocol_len)..];
                
                let mut new_payload = vec![];
                new_payload.extend_from_slice(id_bytes);
                new_payload.extend_from_slice(&new_prot_bytes);
                new_payload.extend_from_slice(rest_of_packet);

                // Encode new length
                let mut new_len_bytes = vec![];
                let mut val = new_payload.len() as u32;
                loop {
                    let mut temp = (val & 0b01111111) as u8;
                    val >>= 7;
                    if val != 0 { temp |= 0b10000000; }
                    new_len_bytes.push(temp);
                    if val == 0 { break; }
                }

                handshake_data = vec![];
                handshake_data.extend_from_slice(&new_len_bytes);
                handshake_data.extend_from_slice(&new_payload);
            }
        }

        // Ideally we proxy this connection to valence bound locally (e.g., 25566)
        match tokio::net::TcpStream::connect("127.0.0.1:25566").await {
            Ok(mut backend) => {
                use tokio::io::AsyncWriteExt;
                if protocol > 0 {
                    backend.write_all(&handshake_data).await?;
                }
                match tokio::io::copy_bidirectional(&mut stream, &mut backend).await {
                    Ok((to_client, to_server)) => {
                        let total_kib = (to_client + to_server) / 1024;
                        log_info!(
                            "Connection {} closed. Session transferred {} KiB",
                            peer_addr,
                            total_kib
                        );
                    }
                    Err(e) => {
                        log_warn!("Proxy connection {} closed unexpectedly: {}", peer_addr, e);
                    }
                }
            }
            Err(e) => {
                log_error!("Could not proxy {} to backend: {}", peer_addr, e);
            }
        }

        Ok(())
    }
}

