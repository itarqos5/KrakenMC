use std::sync::Arc;
use serde::{Deserialize, Serialize};
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
    #[serde(default)]
    pub highest_y: f64,
    #[serde(default)]
    pub held_slot: u8,
}

impl Default for PlayerData {
    fn default() -> Self {
        Self {
            x: 0.0,
            y: 70.0,
            z: 0.0,
            yaw: 0.0,
            pitch: 0.0,
            gamemode: 1, // creative by default
            inventory: vec![Vec::new(); 46],
            highest_y: 70.0,
            held_slot: 0,
        }
    }
}

pub fn save_player(db: &Arc<sled::Db>, uuid: Uuid, data: &PlayerData) {
    let key = format!("player:{}", uuid);
    match postcard::to_allocvec(data) {
        Ok(bytes) => {
            let _ = db.insert(key.as_bytes(), bytes);
            let _ = db.flush();
        }
        Err(e) => {
            crate::logger::log_error!("Failed to serialize player data for {}: {}", uuid, e);
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
