use bevy_app::Plugin;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::config::ServerConfig;
use crate::logger::{log_error, log_info};

mod proxy;

pub struct ViaKrakenPlugin {
    pub config: Arc<ServerConfig>,
}

impl Plugin for ViaKrakenPlugin {
    fn build(&self, _app: &mut bevy_app::App) {
        let config_clone = self.config.clone();
        
        // Spawn the background backend thread
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async move {
                let backend_addr = format!("0.0.0.0:{}", config_clone.server_port + 1);
                let backend_listener = match TcpListener::bind(&backend_addr).await {
                    Ok(l) => l,
                    Err(e) => {
                        log_error!("Failed to bind backend on {}: {}", backend_addr, e);
                        return;
                    }
                };
                
                log_info!("Backend listening natively on {}", backend_addr);
                
                loop {
                    if let Ok((mut stream, _)) = backend_listener.accept().await {
                        tokio::spawn(async move {
                            let mut buf = vec![0u8; 1024];
                            if let Ok(n) = stream.read(&mut buf).await {
                                if n == 0 { return; }
                                // Very rough status response to prevent Unreachable status in client
                                // Packet format requires precise VarInt framing.
                                // If the handshake ends in `state = 1` it's a ServerboundStatusRequestPacket.
                                // We simulate the response for Protocol = 776 or rewrite.
                                
                                let brand = "Kraken"; 
                                let status_json = format!(
                                    r#"{{"version":{{"name":"{}","protocol":776}},"players":{{"max":100,"online":0}},"description":{{"text":"Kraken Engine 1.21.11"}}}}"#,
                                    brand
                                );
                                
                                // VarInt encoded Status response pseudo-logic:
                                let mut response = vec![];
                                response.push(0x00); // Packet ID for status
                                
                                // Append String (Length + String bytes)
                                let str_bytes = status_json.as_bytes();
                                let mut val = str_bytes.len() as u32;
                                loop {
                                    let mut temp = (val & 0b01111111) as u8;
                                    val >>= 7;
                                    if val != 0 { temp |= 0b10000000; }
                                    response.push(temp);
                                    if val == 0 { break; }
                                }
                                response.extend_from_slice(str_bytes);
                                
                                // Push Length of Response
                                let mut final_packet = vec![];
                                let mut val = response.len() as u32;
                                loop {
                                    let mut temp = (val & 0b01111111) as u8;
                                    val >>= 7;
                                    if val != 0 { temp |= 0b10000000; }
                                    final_packet.push(temp);
                                    if val == 0 { break; }
                                }
                                final_packet.extend_from_slice(&response);
                                
                                let _ = stream.write_all(&final_packet).await;
                                
                                // Also simulate Hello handling inside login
                                // ClientboundLoginFinishedPacket handling logic ...
                            }
                        });
                    }
                }
            });
        });

        // Spawn proxy logic
        let config_clone = self.config.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let vk = ViaKraken::new((*config_clone).clone());
                vk.start().await;
            });
        });
    }
}

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
        log_info!("Loading ViaKraken Proxy layer...");
        
        let addr = format!("0.0.0.0:{}", self.config.server_port);
        let listener = match TcpListener::bind(&addr).await {
            Ok(l) => l,
            Err(e) => {
                log_error!("Failed to bind to {}: {}", addr, e);
                return;
            }
        };

        log_info!("ViaKraken proxy listening on {}", addr);

        loop {
            match listener.accept().await {
                Ok((stream, peer_addr)) => {
                    let config_clone = self.config.clone();
                    tokio::spawn(async move {
                        if let Err(e) = proxy::handle_connection(stream, peer_addr, config_clone).await {
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
}
