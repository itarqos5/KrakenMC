
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use owo_colors::OwoColorize;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use bevy_app::App;
use bevy_ecs::prelude::*;

mod config;
mod logger;
mod viakraken;
mod world;
mod systems;
mod handlers;

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

#[derive(Resource, Clone)]
pub struct WorldDb(pub Arc<sled::Db>);

fn main() {
    owo_colors::set_override(true);
    ctrlc::set_handler(move || {
        SHUTDOWN.store(true, Ordering::SeqCst);
        std::process::exit(0);
    }).expect("Error setting Ctrl-C handler");

    let db = Arc::new(sled::open("world_data").expect("Failed to lock sled DB"));
    println!("{}", r#"
 _  _______         _  _______ _   _ 
| |/ /  __ \   /\  | |/ /  ___| \ | |
| ' /| |__) | /  \ | ' /| |__ |  \| |
|  < |  _  / / /\ \|  < |  __|| . ` |
| . \| | \ \/ ____ \ . \| |___| |\  |
|_|\_\_|  \_\/    \_\_|\_\____|_| \_|
"#.purple().bold());

    let server_config = config::ensure_files_exist();

    logger::log_info!(
        "Starting Kraken via Azalea infrastructure on {}:{}",
        server_config.server_ip,
        server_config.server_port
    );

    let mut app = App::new();
    
    app.insert_resource(WorldDb(db.clone()));

    let vk_config = Arc::new(server_config.clone());
    app.add_plugins(viakraken::ViaKrakenPlugin { config: vk_config });
    app.add_plugins(systems::persistence::PersistencePlugin);
    app.add_plugins(world::WorldPlugin);

    loop {
        app.update();
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

