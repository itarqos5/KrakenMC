// Central player interaction event handlers

pub fn handle_player_join() {
    // Log join, send initializing chunks and entities
}

pub fn handle_player_leave() {
    // Clean up connections and save entity state via the Dirty component
}

pub fn handle_chunk_request() {
    // Fulfills async chunk loads
}

pub fn handle_block_break() {
    // Update chunk state natively and mark as dirty
}

pub fn handle_take_damage() {
    // ECS component modification for HP
}
