use bevy_app::Plugin;
use bevy_ecs::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Component)]
pub struct Dirty;

#[derive(Component, Serialize, Deserialize, Clone)]
pub struct PlayerState {
    pub position: (f64, f64, f64),
    pub yaw: f32,
    pub pitch: f32,
    pub uuid: uuid::Uuid,
}

#[derive(Resource, Default)]
pub struct FlushTimer(u32);

pub struct PersistencePlugin;

impl Plugin for PersistencePlugin {
    fn build(&self, app: &mut bevy_app::App) {
        app.init_resource::<FlushTimer>();
        app.add_systems(bevy_app::Update, (mark_dirty, flush_dirty));
    }
}

fn mark_dirty(
    mut commands: Commands,
    query: Query<Entity, (With<PlayerState>, Changed<PlayerState>)>,
) {
    for entity in query.iter() {
        commands.entity(entity).insert(Dirty);
    }
}

fn flush_dirty(
    mut commands: Commands,
    query: Query<(Entity, &PlayerState), With<Dirty>>,
    db: Res<crate::WorldDb>,
    mut timer: ResMut<FlushTimer>,
) {
    timer.0 += 1;
    if timer.0 < 100 {
        return;
    }
    timer.0 = 0;

    for (entity, state) in query.iter() {
        let db_key = format!("player_{}", state.uuid);
        if let Ok(encoded) = postcard::to_allocvec(&state) {
            use std::io::Write;
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            if encoder.write_all(&encoded).is_ok() {
                if let Ok(compressed) = encoder.finish() {
                    let _ = db.0.insert(db_key, compressed);
                }
            }
        }
        commands.entity(entity).remove::<Dirty>();
    }
}
