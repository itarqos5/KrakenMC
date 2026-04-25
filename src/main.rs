
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

fn open_world_db() -> Arc<sled::Db> {
    let primary_path = "world_data";
    let mut last_err: Option<sled::Error> = None;

    for attempt in 1..=5 {
        match sled::open(primary_path) {
            Ok(db) => {
                if attempt > 1 {
                    logger::log_warn!(
                        "Recovered Sled DB lock after {} attempt(s) at {}",
                        attempt,
                        primary_path
                    );
                }
                return Arc::new(db);
            }
            Err(err) => {
                last_err = Some(err);
                logger::log_warn!(
                    "Sled DB open failed (attempt {}/5) at {}. Retrying...",
                    attempt,
                    primary_path
                );
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
        }
    }

    let fallback_path = format!("world_data_{}", std::process::id());
    logger::log_warn!(
        "Could not lock {} after retries; falling back to {}",
        primary_path,
        fallback_path
    );

    match sled::open(&fallback_path) {
        Ok(db) => {
            logger::log_warn!(
                "Using fallback Sled DB path {} for this process",
                fallback_path
            );
            Arc::new(db)
        }
        Err(fallback_err) => {
            if let Some(primary_err) = last_err {
                logger::log_error!("Primary Sled DB error: {}", primary_err);
            }
            logger::log_error!("Fallback Sled DB error ({}): {}", fallback_path, fallback_err);
            std::process::exit(1);
        }
    }
}

fn main() {
    let color_supported = if cfg!(windows) {
        std::env::var("WT_SESSION").is_ok()
            || std::env::var("ANSICON").is_ok()
            || std::env::var("ConEmuANSI").map(|v| v.eq_ignore_ascii_case("on")).unwrap_or(false)
            || std::env::var("TERM").map(|v| v.contains("xterm") || v.contains("ansi")).unwrap_or(false)
    } else {
        true
    };
    owo_colors::set_override(color_supported);
    ctrlc::set_handler(move || {
        SHUTDOWN.store(true, Ordering::SeqCst);
        std::process::exit(0);
    }).expect("Error setting Ctrl-C handler");

    let db = open_world_db();
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

