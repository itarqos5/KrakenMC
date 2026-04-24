#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use owo_colors::OwoColorize;

static SHUTDOWN: AtomicBool = AtomicBool::new(false);
use valence::network::{async_trait, HandshakeData, NetworkCallbacks, ServerListPing, SharedNetworkState};
use valence::prelude::*;

mod config;
mod logger;
mod viakraken;
mod world;
mod chunk;
mod mixins;
mod test_events;

struct KrakenCallbacks;

#[async_trait]
impl NetworkCallbacks for KrakenCallbacks {
    async fn server_list_ping(
        &self,
        shared: &SharedNetworkState,
        _remote_addr: SocketAddr,
        handshake_data: &HandshakeData,
    ) -> ServerListPing {
        ServerListPing::Respond {
            online_players: shared.player_count().load(Ordering::Relaxed) as i32,
            max_players: shared.max_players() as i32,
            player_sample: vec![],
            description: "Kraken Powered by ViaKraken".into_text(),
            favicon_png: &[],
            version_name: format!("Kraken 1.20+"),
            protocol: handshake_data.protocol_version,
        }
    }
}

use sled::Db;

#[derive(Resource, Clone)]
pub struct WorldDb(pub Arc<Db>);

pub fn check_shutdown(
    mut commands: Commands,
    mut clients: Query<(&mut Client, Entity)>,
    mut layers: Query<&mut ChunkLayer>,
    mut exit: EventWriter<AppExit>,
) {
    if SHUTDOWN.load(Ordering::SeqCst) {
        log_info!("Initiating server shutdown sequence...");
        for (mut client, entity) in clients.iter_mut() {
            client.send_chat_message("Server is shutting down!");
            commands.entity(entity).insert(valence::prelude::Despawned);
        }
        
        log_info!("Saving world data and unloading chunks...");
        for mut layer in layers.iter_mut() {
            // Unload all chunks
            layer.retain_chunks(|_, _| false);
        }

        log_info!("Gracefully freeing memory and exiting.");
        exit.send(AppExit::Success);
    }
}

pub fn main() {
    ctrlc::set_handler(move || {
        SHUTDOWN.store(true, Ordering::SeqCst);
    }).expect("Error setting Ctrl-C handler");
    let db = Arc::new(sled::open("world_data").expect("Failed to lock sled DB"));
    let ascii = r#"
 _  __ _____            _  __ ______  _   _ 
| |/ //  __ \    /\    | |/ /|  ____|| \ | |
| ' / | |__) |  /  \   | ' / | |__   |  \| |
|  <  |  _  /  / /\ \  |  <  |  __|  | . ` |
| . \ | | \ \ / ____ \ | . \ | |____ | |\  |
|_|\_\|_|  \_\_/    \_\|_|\_\|______||_| \_|
    "#;
    println!("{}", ascii.purple().bold());

    let mut server_config = config::ensure_files_exist();
    if server_config.target_protocol != valence::PROTOCOL_VERSION {
        log_warn!(
            "Configured target protocol {} does not match valence backend {}. Forcing backend target to {}.",
            server_config.target_protocol,
            valence::PROTOCOL_VERSION,
            valence::PROTOCOL_VERSION
        );
        server_config.target_protocol = valence::PROTOCOL_VERSION;
    }

    log_info!(
        "Starting Kraken on {}:{} for protocol {}",
        server_config.server_ip,
        server_config.server_port,
        server_config.target_protocol
    );

    let vk_config = server_config.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            let vk = viakraken::ViaKraken::new(vk_config);
            vk.start().await;
        });
    });

    App::new()
        .insert_resource(NetworkSettings {
            connection_mode: ConnectionMode::Offline, // Internal offline proxy
            callbacks: KrakenCallbacks.into(),
            max_players: server_config.max_players as usize,
            address: "127.0.0.1:25566".parse().unwrap(),
            ..Default::default()
        })
        .add_plugins(DefaultPlugins.build().disable::<bevy_log::LogPlugin>())
                .insert_resource(WorldDb(db.clone()))
        .add_systems(Startup, world::setup_world)
        .add_systems(
            Update,
            (
                (
                    world::init_clients,
                    world::update_client_views,
                    world::send_recv_chunks,
                    world::tick_active_chunks,
                    systems::interactions::interact_blocks,
                    systems::commands::in_game_commands,
                    test_events::test_system,
                    check_shutdown,
                )
                    .chain(),
                despawn_disconnected_clients,
            ),
        )
        .run();
}

mod systems;


