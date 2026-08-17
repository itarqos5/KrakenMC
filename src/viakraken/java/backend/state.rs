use std::collections::HashMap;
use std::sync::atomic::AtomicI32;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;
use uuid::Uuid;

pub fn chat_channel() -> &'static tokio::sync::broadcast::Sender<String> {
    static CHANNEL: OnceLock<tokio::sync::broadcast::Sender<String>> = OnceLock::new();
    CHANNEL.get_or_init(|| {
        let (tx, _) = tokio::sync::broadcast::channel(100);
        tx
    })
}

#[derive(Clone, Copy, Debug)]
pub struct BlockUpdateEvent {
    pub x: i32,
    pub y: i32,
    pub z: i32,
    /// Canonical block-state ID from Pumpkin's current registry.
    pub state_id: u16,
}

pub fn block_channel() -> &'static tokio::sync::broadcast::Sender<BlockUpdateEvent> {
    static CHANNEL: OnceLock<tokio::sync::broadcast::Sender<BlockUpdateEvent>> = OnceLock::new();
    CHANNEL.get_or_init(|| {
        let (tx, _) = tokio::sync::broadcast::channel(1024);
        tx
    })
}

#[derive(Clone, Debug)]
pub enum PlayerEvent {
    Join {
        entity_id: i32,
        uuid: Uuid,
        username: String,
        x: f64,
        y: f64,
        z: f64,
        yaw: f32,
        pitch: f32,
        gamemode: u8,
    },
    Move {
        entity_id: i32,
        uuid: Uuid,
        x: f64,
        y: f64,
        z: f64,
        yaw: f32,
        pitch: f32,
    },
    Leave {
        entity_id: i32,
        uuid: Uuid,
    },
    GamemodeChange {
        uuid: Uuid,
        gamemode: u8,
    },
    Hurt {
        entity_id: i32,
        uuid: Uuid,
        damage: f32,
        x: f64,
        y: f64,
        z: f64,
        attacker_x: Option<f64>,
        attacker_z: Option<f64>,
    },
}

#[derive(Clone)]
pub struct OnlinePlayer {
    pub entity_id: i32,
    pub uuid: Uuid,
    pub username: String,
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub yaw: f32,
    pub pitch: f32,
    pub gamemode: u8,
}

pub fn player_event_channel() -> &'static tokio::sync::broadcast::Sender<PlayerEvent> {
    static CHANNEL: OnceLock<tokio::sync::broadcast::Sender<PlayerEvent>> = OnceLock::new();
    CHANNEL.get_or_init(|| {
        let (tx, _) = tokio::sync::broadcast::channel(200);
        tx
    })
}

pub fn online_players() -> &'static Mutex<HashMap<Uuid, OnlinePlayer>> {
    static PLAYERS: OnceLock<Mutex<HashMap<Uuid, OnlinePlayer>>> = OnceLock::new();
    PLAYERS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub static NEXT_ENTITY_ID: AtomicI32 = AtomicI32::new(1);

#[derive(Clone, Debug)]
pub struct SummonedEntity {
    pub entity_id: i32,
    pub entity_type: u16,
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub health: f32,
    pub burning: bool,
    pub last_burn_damage: Instant,
}

pub fn summoned_entities() -> &'static Mutex<HashMap<i32, SummonedEntity>> {
    static ENTITIES: OnceLock<Mutex<HashMap<i32, SummonedEntity>>> = OnceLock::new();
    ENTITIES.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn register_summoned_entity(entity_id: i32, entity_type: u16, x: f64, y: f64, z: f64) {
    if let Ok(mut entities) = summoned_entities().lock() {
        entities.insert(
            entity_id,
            SummonedEntity {
                entity_id,
                entity_type,
                x,
                y,
                z,
                health: 20.0,
                burning: false,
                last_burn_damage: Instant::now(),
            },
        );
    }
}

#[derive(Clone, Copy, Debug)]
pub enum SummonedEntityEvent {
    Burning { entity_id: i32, burning: bool },
    Hurt { entity_id: i32 },
    Remove { entity_id: i32 },
}

pub fn summoned_entity_channel() -> &'static tokio::sync::broadcast::Sender<SummonedEntityEvent> {
    static CHANNEL: OnceLock<tokio::sync::broadcast::Sender<SummonedEntityEvent>> = OnceLock::new();
    CHANNEL.get_or_init(|| {
        let (tx, _) = tokio::sync::broadcast::channel(128);
        tx
    })
}

#[derive(Clone, Debug)]
pub enum ConsoleCommand {
    OperatorLevel {
        uuid: Uuid,
        level: u8,
    },
    Kill {
        uuid: Uuid,
    },
    Gamemode {
        uuid: Uuid,
        gamemode: u8,
    },
    Summon {
        entity_id: i32,
        entity_type: u16,
        x: f64,
        y: f64,
        z: f64,
    },
}

pub fn console_command_channel() -> &'static tokio::sync::broadcast::Sender<ConsoleCommand> {
    static CHANNEL: OnceLock<tokio::sync::broadcast::Sender<ConsoleCommand>> = OnceLock::new();
    CHANNEL.get_or_init(|| {
        let (tx, _) = tokio::sync::broadcast::channel(64);
        tx
    })
}

pub fn gamemode_abilities(gamemode: u8) -> (i8, f32) {
    match gamemode {
        1 => (0x01 | 0x04 | 0x08, 0.05), // invulnerable + allow fly + instant break
        3 => (0x02 | 0x04, 0.05),        // flying + allow fly (spectator)
        _ => (0, 0.05),                  // survival/adventure: nothing
    }
}

#[derive(Clone, Debug)]
pub enum ItemEvent {
    Spawn {
        entity_id: i32,
        item_id: u16,
        count: u8,
        x: f64,
        y: f64,
        z: f64,
        vx: f64,
        vy: f64,
        vz: f64,
    },
    Pickup {
        item_entity_id: i32,
        player_entity_id: i32,
    },
}

pub fn item_event_channel() -> &'static tokio::sync::broadcast::Sender<ItemEvent> {
    static CHANNEL: OnceLock<tokio::sync::broadcast::Sender<ItemEvent>> = OnceLock::new();
    CHANNEL.get_or_init(|| {
        let (tx, _) = tokio::sync::broadcast::channel(500);
        tx
    })
}
