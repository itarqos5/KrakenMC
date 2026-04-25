use bevy_app::Plugin;
use bevy_ecs::prelude::*;
use std::sync::Arc;
use tokio::net::TcpListener;

use crate::config::ServerConfig;
use crate::logger::{log_error, log_info};

mod proxy;

pub struct ViaKrakenPlugin {
    pub config: Arc<ServerConfig>,
}

impl Plugin for ViaKrakenPlugin {
    fn build(&self, _app: &mut bevy_app::App) {
        // Plugin initialization would set up TCP binding, perhaps using a background Tokio runtime
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

