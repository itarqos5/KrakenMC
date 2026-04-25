pub mod handlers;

// This module orchestrates Azalea packet handling logic.
// You'll want to decode `Clientbound` vs `Serverbound` packets
// and dispatch them to the correct ECS systems or handlers.

pub struct PacketPlugin;

impl bevy_app::Plugin for PacketPlugin {
    fn build(&self, _app: &mut bevy_app::App) {
        // Register your packet receiving/sending ECS systems here
    }
}
