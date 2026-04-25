use bevy_app::Plugin;
use std::io::{Error, ErrorKind};
use std::sync::Arc;

use tokio::net::{TcpListener, TcpStream};

use crate::config::ServerConfig;
use crate::logger::{log_error, log_info, log_warn};

mod bedrock;
mod java;
mod utils;

pub struct ViaKrakenPlugin {
    pub config: Arc<ServerConfig>,
}

impl Plugin for ViaKrakenPlugin {
    fn build(&self, _app: &mut bevy_app::App) {
        let runtime_config = self.config.clone();
        std::thread::spawn(move || {
            let runtime = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(e) => {
                    log_error!("Failed to create ViaKraken runtime: {}", e);
                    return;
                }
            };

            runtime.block_on(async move {
                let vk = ViaKraken {
                    config: runtime_config,
                };
                vk.start().await;
            });
        });
    }
}

pub struct ViaKraken {
    pub config: Arc<ServerConfig>,
}

impl ViaKraken {
    pub async fn start(&self) {
        let (backend_listener, backend_port) = match bind_with_port_bump(
            "0.0.0.0",
            self.config.server_port.saturating_add(1),
            "backend",
        )
        .await
        {
            Ok(binding) => binding,
            Err(e) => {
                log_error!("Failed to bind backend listener: {}", e);
                return;
            }
        };

        let (proxy_listener, proxy_port) =
            match bind_with_port_bump("0.0.0.0", self.config.server_port, "proxy").await {
                Ok(binding) => binding,
                Err(e) => {
                    log_error!("Failed to bind proxy listener: {}", e);
                    return;
                }
            };

        log_info!(
            "ViaKraken active: proxy=0.0.0.0:{} backend=0.0.0.0:{}",
            proxy_port,
            backend_port
        );

        let backend_config = self.config.clone();
        tokio::spawn(async move {
            if let Err(e) =
                java::run_backend_listener(backend_listener, backend_config, backend_port).await
            {
                log_error!("Backend listener stopped: {}", e);
            }
        });

        loop {
            match proxy_listener.accept().await {
                Ok((stream, peer_addr)) => {
                    let config = self.config.clone();
                    tokio::spawn(async move {
                        if let Err(e) =
                            handle_proxy_connection(stream, peer_addr, config, backend_port).await
                        {
                            log_error!("Proxy connection {} failed: {}", peer_addr, e);
                        }
                    });
                }
                Err(e) => {
                    log_error!("Failed accepting proxy client: {}", e);
                }
            }
        }
    }
}

async fn bind_with_port_bump(
    host: &str,
    start_port: u16,
    label: &str,
) -> std::io::Result<(TcpListener, u16)> {
    let mut port = start_port;
    let mut last_addr_in_use: Option<Error> = None;

    for attempt in 1..=10 {
        match TcpListener::bind((host, port)).await {
            Ok(listener) => {
                if attempt > 1 {
                    log_warn!(
                        "{} listener recovered on attempt {}/10 at {}:{}",
                        label,
                        attempt,
                        host,
                        port
                    );
                }
                return Ok((listener, port));
            }
            Err(e) if e.kind() == ErrorKind::AddrInUse => {
                log_warn!(
                    "{} listener port {} in use (attempt {}/10); trying {}",
                    label,
                    port,
                    attempt,
                    port.saturating_add(1)
                );
                last_addr_in_use = Some(e);
                port = port.saturating_add(1);
            }
            Err(e) => return Err(e),
        }
    }

    if let Some(e) = last_addr_in_use {
        Err(e)
    } else {
        Err(Error::new(
            ErrorKind::AddrInUse,
            format!("{} listener failed to bind after 10 attempts", label),
        ))
    }
}

async fn handle_proxy_connection(
    mut stream: TcpStream,
    peer_addr: std::net::SocketAddr,
    config: Arc<ServerConfig>,
    backend_port: u16,
) -> std::io::Result<()> {
    if bedrock::is_probably_bedrock(&mut stream).await? {
        log_info!("Bedrock ingress detected from {}", peer_addr);
        bedrock::handle_bedrock_disconnect(&mut stream).await?;
        return Ok(());
    }

    java::handle_java_connection(stream, peer_addr, config, backend_port).await
}
