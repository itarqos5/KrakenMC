use valence::prelude::*;
use valence::message::ChatMessageEvent;
use valence_command::CommandExecutionEvent;
use valence::action::DiggingEvent;
use valence::interact_block::InteractBlockEvent;

pub fn test_system(
    mut _messages: EventReader<ChatMessageEvent>,
    mut _commands: EventReader<CommandExecutionEvent>,
    mut dig_events: EventReader<DiggingEvent>,
    mut _interact: EventReader<InteractBlockEvent>,
    mut layers: Query<&mut ChunkLayer>,
) {
    for event in dig_events.read() {
        if event.state == valence::action::DiggingState::Stop {
            if let Ok(mut layer) = layers.get_single_mut() {
                layer.set_block(event.position, BlockState::AIR);
            }
        }
    }
}
