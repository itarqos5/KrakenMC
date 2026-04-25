use std::path::{Path, PathBuf};
use std::sync::Arc;

use azalea_world::ChunkStorage;
use bevy_app::Plugin;
use bevy_ecs::prelude::*;
use noise::{NoiseFn, Perlin};

use crate::logger::{log_error, log_info, log_warn};

pub struct WorldPlugin;

#[derive(Resource)]
pub struct ServerWorld {
    pub storage: ChunkStorage,
    pub terrain: Perlin,
}

impl Plugin for WorldPlugin {
    fn build(&self, app: &mut bevy_app::App) {
        app.insert_resource(ServerWorld {
            storage: ChunkStorage::default(),
            terrain: Perlin::new(42),
        });
        app.add_systems(bevy_app::Update, generate_chunks);
    }
}

pub fn initialize_world_db() -> Arc<sled::Db> {
    let db_path = PathBuf::from("world_data");
    let lock_path_primary = db_path.join("lock");
    let lock_path_nested = db_path.join("db").join("lock");

    for attempt in 1..=3 {
        match sled::open(&db_path) {
            Ok(db) => {
                if attempt > 1 {
                    log_warn!(
                        "Sled DB opened after stale-lock recovery (attempt {}/3)",
                        attempt
                    );
                }
                return Arc::new(db);
            }
            Err(err) => {
                let lock_related = is_lock_error(&err);
                log_warn!(
                    "Failed to open Sled DB at {} (attempt {}/3): {}",
                    db_path.display(),
                    attempt,
                    err
                );

                if lock_related {
                    let removed_primary = try_remove_lock(&lock_path_primary);
                    let removed_nested = try_remove_lock(&lock_path_nested);
                    if removed_primary || removed_nested {
                        log_info!(
                            "Removed stale Sled lock file(s); retrying open on {}",
                            db_path.display()
                        );
                    }
                }

                if attempt < 3 {
                    std::thread::sleep(std::time::Duration::from_millis(300));
                    continue;
                }

                log_error!(
                    "Unable to initialize Sled DB at {}. Ensure no zombie process is holding the lock, then restart.",
                    db_path.display()
                );
                std::process::exit(1);
            }
        }
    }

    log_error!("Unreachable Sled init state reached");
    std::process::exit(1);
}

fn is_lock_error(err: &sled::Error) -> bool {
    let msg = err.to_string().to_ascii_lowercase();
    msg.contains("acquire lock")
        || msg.contains("locked")
        || msg.contains("os { code: 33")
        || msg.contains("wouldblock")
}

fn try_remove_lock(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }

    match std::fs::remove_file(path) {
        Ok(_) => {
            log_warn!("Removed lock file {}", path.display());
            true
        }
        Err(e) => {
            log_warn!("Failed to remove lock file {}: {}", path.display(), e);
            false
        }
    }
}

fn generate_chunks(world: Res<ServerWorld>) {
    let _ = world.terrain.get([0.0, 0.0, 0.0]);
}
