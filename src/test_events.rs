use valence::prelude::*;
use valence::message::ChatMessageEvent;
use valence_command::CommandExecutionEvent;
use valence::action::DiggingEvent;
use valence::interact_block::InteractBlockEvent;

pub fn test_system(
    mut messages: EventReader<ChatMessageEvent>,
    mut commands: EventReader<CommandExecutionEvent>,
    mut dig: EventReader<DiggingEvent>,
    mut interact: EventReader<InteractBlockEvent>,
) {
}
