use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

/// Persisted player data stored on disconnect, loaded on login.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PlayerData {
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub yaw: f32,
    pub pitch: f32,
    /// 0=survival 1=creative 2=adventure 3=spectator
    pub gamemode: u8,
    pub inventory: Vec<Vec<u8>>,
    /// Runtime cursor stack used by container clicks; never persisted.
    #[serde(skip)]
    pub carried_item: Vec<u8>,
    #[serde(default)]
    pub highest_y: f64,
    #[serde(default)]
    pub held_slot: u8,
    #[serde(default = "default_health")]
    pub health: f32,
    /// Runtime permission state loaded from ops.json; never persisted with player data.
    #[serde(skip)]
    pub operator_level: u8,
}

fn default_health() -> f32 {
    20.0
}

impl Default for PlayerData {
    fn default() -> Self {
        Self {
            x: 0.0,
            y: 70.0,
            z: 0.0,
            yaw: 0.0,
            pitch: 0.0,
            gamemode: 0, // new players start in survival
            inventory: vec![Vec::new(); 46],
            carried_item: Vec::new(),
            highest_y: 70.0,
            held_slot: 0,
            health: 20.0,
            operator_level: 0,
        }
    }
}

pub fn save_player(db: &Arc<sled::Db>, uuid: Uuid, data: &PlayerData) -> sled::Result<()> {
    let key = format!("player:{}", uuid);
    match postcard::to_allocvec(data) {
        Ok(bytes) => db.insert(key.as_bytes(), bytes).map(|_| ()),
        Err(e) => {
            crate::logger::log_error!("Failed to serialize player data for {}: {}", uuid, e);
            Ok(())
        }
    }
}

pub fn load_player(db: &Arc<sled::Db>, uuid: Uuid) -> PlayerData {
    let key = format!("player:{}", uuid);
    if let Ok(Some(bytes)) = db.get(key.as_bytes()) {
        if let Ok(data) = postcard::from_bytes::<PlayerData>(&bytes) {
            return data;
        }
    }
    PlayerData::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn player_transform_and_inventory_round_trip() {
        let db = Arc::new(sled::Config::new().temporary(true).open().unwrap());
        let uuid = Uuid::new_v4();
        let mut player = PlayerData::default();
        player.x = -31.5;
        player.y = 82.0;
        player.z = 144.25;
        player.yaw = 123.0;
        player.pitch = -42.5;
        player.inventory[36] = vec![1, 2, 3, 4];
        player.operator_level = 4;

        save_player(&db, uuid, &player).unwrap();
        let loaded = load_player(&db, uuid);

        assert_eq!((loaded.x, loaded.y, loaded.z), (-31.5, 82.0, 144.25));
        assert_eq!((loaded.yaw, loaded.pitch), (123.0, -42.5));
        assert_eq!(loaded.inventory[36], vec![1, 2, 3, 4]);
        assert_eq!(loaded.operator_level, 0);
    }

    #[test]
    fn new_players_start_in_survival() {
        let db = Arc::new(sled::Config::new().temporary(true).open().unwrap());
        let player = load_player(&db, Uuid::new_v4());

        assert_eq!(player.gamemode, 0);
    }
}
