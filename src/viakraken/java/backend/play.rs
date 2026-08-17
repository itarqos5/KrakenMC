use std::collections::HashSet;
use std::io::Error;
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use uuid::Uuid;

use pumpkin_protocol::codec::var_int::VarInt;
use pumpkin_protocol::java::client::play::{
    CCenterChunk, CChunkBatchEnd, CChunkBatchStart, CCommands, CCustomPayload, CEntityStatus,
    CGameEvent, CHeadRot, CKeepAlive, CPlayerAbilities, CPlayerInfoUpdate, CPlayerPosition,
    CPlayerSpawnPosition, CRemoveEntities, CRemovePlayerInfo, CSpawnEntity, CTeleportEntity,
    CUnloadChunk, GameEvent, Player, PlayerAction, PlayerInfoFlags, ProtoNode, ProtoNodeType,
};
use pumpkin_protocol::ser::NetworkWriteExt;
use pumpkin_protocol::PositionFlag;
use pumpkin_util::math::position::BlockPos;
use pumpkin_util::math::vector3::Vector3;
use pumpkin_util::version::MinecraftVersion;

use crate::config::ServerConfig;
use crate::logger::{log_info, log_warn};
use crate::operator_store::operator_level;
use crate::viakraken::java::packets::encode_java_packet;
use crate::viakraken::utils::{read_varint_from_slice, write_framed_payload};
use crate::world::chunk_gen::encode_chunk_packet;
use crate::world::player_store::{load_player, save_player};

use super::handler::{change_gamemode, handle_play_packet};
use super::state::{
    block_channel, chat_channel, console_command_channel, gamemode_abilities, online_players,
    player_event_channel, ConsoleCommand, OnlinePlayer, PlayerEvent, NEXT_ENTITY_ID,
};

async fn send_permission_status(
    stream: &mut TcpStream,
    version: MinecraftVersion,
    entity_id: i32,
    operator_level: u8,
) -> std::io::Result<()> {
    let status = CEntityStatus::new(entity_id, (24 + operator_level.clamp(0, 4)) as i8);
    let payload = encode_java_packet(&status, version)?;
    write_framed_payload(stream, payload.as_slice()).await
}

async fn send_command_tree(
    stream: &mut TcpStream,
    version: MinecraftVersion,
    is_operator: bool,
) -> std::io::Result<()> {
    let root_children = if is_operator {
        vec![VarInt(1)]
    } else {
        Vec::new()
    };
    let nodes = vec![
        ProtoNode {
            children: root_children.into_boxed_slice(),
            node_type: ProtoNodeType::Root,
        },
        ProtoNode {
            children: vec![VarInt(2), VarInt(3), VarInt(4), VarInt(5)].into_boxed_slice(),
            node_type: ProtoNodeType::Literal {
                name: "gamemode",
                is_executable: false,
                redirect_target: None,
                restricted: true,
            },
        },
        ProtoNode {
            children: vec![].into_boxed_slice(),
            node_type: ProtoNodeType::Literal {
                name: "survival",
                is_executable: true,
                redirect_target: None,
                restricted: false,
            },
        },
        ProtoNode {
            children: vec![].into_boxed_slice(),
            node_type: ProtoNodeType::Literal {
                name: "creative",
                is_executable: true,
                redirect_target: None,
                restricted: false,
            },
        },
        ProtoNode {
            children: vec![].into_boxed_slice(),
            node_type: ProtoNodeType::Literal {
                name: "adventure",
                is_executable: true,
                redirect_target: None,
                restricted: false,
            },
        },
        ProtoNode {
            children: vec![].into_boxed_slice(),
            node_type: ProtoNodeType::Literal {
                name: "spectator",
                is_executable: true,
                redirect_target: None,
                restricted: false,
            },
        },
    ];
    let commands = CCommands::new(nodes.into_boxed_slice(), VarInt(0));
    let payload = encode_java_packet(&commands, version)?;
    write_framed_payload(stream, payload.as_slice()).await
}

fn chunk_coordinate(block_coordinate: f64) -> i32 {
    (block_coordinate.floor() as i32) >> 4
}

fn chunks_in_view(center_x: i32, center_z: i32, view_distance: i32) -> HashSet<(i32, i32)> {
    let diameter = (view_distance * 2 + 1) as usize;
    let mut chunks = HashSet::with_capacity(diameter * diameter);
    for dz in -view_distance..=view_distance {
        for dx in -view_distance..=view_distance {
            chunks.insert((center_x + dx, center_z + dz));
        }
    }
    chunks
}

async fn stream_chunks(
    stream: &mut TcpStream,
    version: MinecraftVersion,
    protocol_version: i32,
    db: &Arc<sled::Db>,
    center_x: i32,
    center_z: i32,
    sent_chunks: &mut HashSet<(i32, i32)>,
    view_distance: i32,
) -> std::io::Result<()> {
    let center = CCenterChunk {
        chunk_x: VarInt(center_x),
        chunk_z: VarInt(center_z),
    };
    let payload = encode_java_packet(&center, version)?;
    write_framed_payload(stream, payload.as_slice()).await?;

    let desired_chunks = chunks_in_view(center_x, center_z, view_distance);
    for &(chunk_x, chunk_z) in sent_chunks.difference(&desired_chunks) {
        let unload = CUnloadChunk::new(chunk_x, chunk_z);
        let payload = encode_java_packet(&unload, version)?;
        write_framed_payload(stream, payload.as_slice()).await?;
    }

    let mut missing_chunks: Vec<_> = desired_chunks.difference(sent_chunks).copied().collect();
    missing_chunks.sort_unstable_by_key(|(chunk_x, chunk_z)| {
        let dx = chunk_x - center_x;
        let dz = chunk_z - center_z;
        dx * dx + dz * dz
    });

    if !missing_chunks.is_empty() {
        let batch_start = CChunkBatchStart;
        let payload = encode_java_packet(&batch_start, version)?;
        write_framed_payload(stream, payload.as_slice()).await?;

        let generation_db = Arc::clone(db);
        let packets = tokio::task::spawn_blocking(move || {
            missing_chunks
                .into_iter()
                .map(|(chunk_x, chunk_z)| {
                    encode_chunk_packet(chunk_x, chunk_z, protocol_version, &generation_db)
                })
                .collect::<Vec<_>>()
        })
        .await
        .map_err(|error| Error::other(format!("chunk generation task failed: {error}")))?;

        for packet in &packets {
            write_framed_payload(stream, packet.as_slice()).await?;
        }

        let batch_end = CChunkBatchEnd::new(packets.len() as u16);
        let payload = encode_java_packet(&batch_end, version)?;
        write_framed_payload(stream, payload.as_slice()).await?;
    }

    *sent_chunks = desired_chunks;
    Ok(())
}

pub async fn handle_play(
    stream: &mut TcpStream,
    config: &ServerConfig,
    version: MinecraftVersion,
    protocol_version: i32,
    username: &str,
    uuid: Uuid,
    db: Arc<sled::Db>,
    view_distance: i32,
) -> std::io::Result<()> {
    let mut player = load_player(&db, uuid);
    player.operator_level = operator_level(uuid, username);
    if player.inventory.len() != 46 {
        player.inventory = vec![vec![]; 46];
    }

    let my_entity_id = NEXT_ENTITY_ID.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

    let login_play = pumpkin_protocol::java::client::play::CLogin::new(
        my_entity_id,
        false,
        vec!["minecraft:overworld".to_string()],
        VarInt(3),
        VarInt(view_distance),
        VarInt(view_distance.min(12)),
        false,
        true,
        false,
        pumpkin_data::dimension::Dimension::OVERWORLD,
        player.x as i64,
        player.gamemode,
        1,
        false,
        true,
        None,
        VarInt(0),
        VarInt(63),
        false,
    );
    let login_play_payload = encode_java_packet(&login_play, version)?;
    write_framed_payload(stream, login_play_payload.as_slice()).await?;

    send_permission_status(stream, version, my_entity_id, player.operator_level).await?;
    send_command_tree(stream, version, player.operator_level > 0).await?;

    {
        let flags = (PlayerInfoFlags::ADD_PLAYER
            | PlayerInfoFlags::UPDATE_LISTED
            | PlayerInfoFlags::UPDATE_GAME_MODE
            | PlayerInfoFlags::UPDATE_LATENCY)
            .bits();

        let actions = vec![
            PlayerAction::AddPlayer {
                name: username,
                properties: &[],
            },
            PlayerAction::UpdateGameMode(VarInt(player.gamemode as i32)),
            PlayerAction::UpdateListed(true),
            PlayerAction::UpdateLatency(VarInt(0)),
        ];

        let players = vec![Player {
            uuid,
            actions: &actions,
        }];

        let player_info = CPlayerInfoUpdate::new(flags, &players);
        let player_info_payload = encode_java_packet(&player_info, version)?;
        write_framed_payload(stream, player_info_payload.as_slice()).await?;
    }

    {
        let other_players: Vec<OnlinePlayer> = {
            let players_guard = online_players().lock().unwrap();
            players_guard.values().cloned().collect()
        };

        for other in other_players {
            let flags = (PlayerInfoFlags::ADD_PLAYER
                | PlayerInfoFlags::UPDATE_LISTED
                | PlayerInfoFlags::UPDATE_GAME_MODE
                | PlayerInfoFlags::UPDATE_LATENCY)
                .bits();

            let actions = vec![
                PlayerAction::AddPlayer {
                    name: &other.username,
                    properties: &[],
                },
                PlayerAction::UpdateGameMode(VarInt(other.gamemode as i32)),
                PlayerAction::UpdateListed(true),
                PlayerAction::UpdateLatency(VarInt(0)),
            ];

            let players = vec![Player {
                uuid: other.uuid,
                actions: &actions,
            }];

            let player_info = CPlayerInfoUpdate::new(flags, &players);
            if let Ok(payload) = encode_java_packet(&player_info, version) {
                let _ = write_framed_payload(stream, payload.as_slice()).await;
            }

            let spawn_pkt = CSpawnEntity::new(
                VarInt(other.entity_id),
                other.uuid,
                VarInt(pumpkin_data::entity::EntityType::PLAYER.id as i32),
                Vector3 {
                    x: other.x,
                    y: other.y,
                    z: other.z,
                },
                other.pitch,
                other.yaw,
                other.yaw,
                VarInt(0),
                Vector3 {
                    x: 0.0,
                    y: 0.0,
                    z: 0.0,
                },
            );
            if let Ok(payload) = encode_java_packet(&spawn_pkt, version) {
                let _ = write_framed_payload(stream, payload.as_slice()).await;
            }
        }
    }

    {
        let mut players_guard = online_players().lock().unwrap();
        players_guard.insert(
            uuid,
            OnlinePlayer {
                entity_id: my_entity_id,
                uuid,
                username: username.to_string(),
                x: player.x,
                y: player.y,
                z: player.z,
                yaw: player.yaw,
                pitch: player.pitch,
                gamemode: player.gamemode,
            },
        );
    }

    {
        let _ = player_event_channel().send(PlayerEvent::Join {
            entity_id: my_entity_id,
            uuid,
            username: username.to_string(),
            x: player.x,
            y: player.y,
            z: player.z,
            yaw: player.yaw,
            pitch: player.pitch,
            gamemode: player.gamemode,
        });
        let _ = chat_channel().send(format!("{} joined the game", username));
    }

    {
        use pumpkin_protocol::java::client::play::CSetHealth;
        let hp = CSetHealth::new(player.health, VarInt(20), 20.0);
        let hp_payload = encode_java_packet(&hp, version)?;
        write_framed_payload(stream, hp_payload.as_slice()).await?;
    }

    {
        let mut buf = Vec::new();
        let _ = buf.write_var_int(&VarInt(0));
        let _ = buf.write_var_int(&VarInt(0));
        let _ = buf.write_var_int(&VarInt(46));
        for i in 0..46 {
            if player.inventory[i].is_empty() {
                let _ = buf.write_var_int(&VarInt(0));
            } else {
                buf.extend_from_slice(&player.inventory[i]);
            }
        }
        let _ = buf.write_var_int(&VarInt(0));

        let mut pkt_buf = Vec::new();
        let _ = pkt_buf.write_var_int(&VarInt(
            pumpkin_data::packet::clientbound::PLAY_CONTAINER_SET_CONTENT.to_id(version),
        ));
        pkt_buf.extend_from_slice(&buf);
        let _ = write_framed_payload(stream, &pkt_buf).await;
    }

    {
        let (flags, fly_speed) = gamemode_abilities(player.gamemode);
        let abilities = CPlayerAbilities::new(flags, fly_speed, 0.1);
        let payload = encode_java_packet(&abilities, version)?;
        write_framed_payload(stream, payload.as_slice()).await?;
    }

    {
        let spawn_pos = BlockPos(Vector3 { x: 0, y: 70, z: 0 });
        let spawn_pkt =
            CPlayerSpawnPosition::new(spawn_pos, 0.0, 0.0, "minecraft:overworld".to_string());
        let payload = encode_java_packet(&spawn_pkt, version)?;
        write_framed_payload(stream, payload.as_slice()).await?;
    }

    {
        let pos_pkt = CPlayerPosition::new(
            VarInt(1),
            Vector3 {
                x: player.x,
                y: player.y,
                z: player.z,
            },
            Vector3 {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
            player.yaw,
            player.pitch,
            vec![],
        );
        let payload = encode_java_packet(&pos_pkt, version)?;
        write_framed_payload(stream, payload.as_slice()).await?;
    }

    {
        let waiting = CGameEvent::new(GameEvent::StartWaitingChunks, 0.0);
        let payload = encode_java_packet(&waiting, version)?;
        write_framed_payload(stream, payload.as_slice()).await?;
    }

    let mut center_chunk = (chunk_coordinate(player.x), chunk_coordinate(player.z));
    let mut sent_chunks = HashSet::new();
    stream_chunks(
        stream,
        version,
        protocol_version,
        &db,
        center_chunk.0,
        center_chunk.1,
        &mut sent_chunks,
        view_distance,
    )
    .await?;

    {
        let brand_data = b"\x06Kraken";
        let brand_pkt = CCustomPayload::new("minecraft:brand", brand_data);
        let payload = encode_java_packet(&brand_pkt, version)?;
        write_framed_payload(stream, payload.as_slice()).await?;
    }

    log_info!(
        "Login flow completed for {} (protocol={}, max_players={}, operator={}, pos=({:.1},{:.1},{:.1}))",
        username,
        protocol_version,
        config.max_players,
        player.operator_level > 0,
        player.x, player.y, player.z
    );

    let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
    let mut save_interval = tokio::time::interval(std::time::Duration::from_secs(30));
    save_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    save_interval.reset();
    let mut keep_alive_id = 0i64;
    let mut buf = vec![0u8; 65536];
    let mut pending_bytes = Vec::new();
    let mut chat_rx = chat_channel().subscribe();
    let mut block_rx = block_channel().subscribe();
    let mut event_rx = player_event_channel().subscribe();
    let mut item_rx = crate::viakraken::java::backend::state::item_event_channel().subscribe();
    let mut console_rx = console_command_channel().subscribe();

    'play: loop {
        tokio::select! {
            Ok(command) = console_rx.recv() => {
                match command {
                    ConsoleCommand::OperatorLevel { uuid: target, level } if target == uuid => {
                        player.operator_level = level.clamp(0, 4);
                        send_permission_status(stream, version, my_entity_id, player.operator_level).await?;
                        send_command_tree(stream, version, player.operator_level > 0).await?;
                    }
                    ConsoleCommand::Kill { uuid: target } if target == uuid => {
                        player.health = 0.0;
                        let health = pumpkin_protocol::java::client::play::CSetHealth::new(
                            0.0,
                            VarInt(20),
                            20.0,
                        );
                        let payload = encode_java_packet(&health, version)?;
                        write_framed_payload(stream, payload.as_slice()).await?;
                        log_info!("Killed {} from the console.", username);
                    }
                    ConsoleCommand::Gamemode { uuid: target, gamemode } if target == uuid => {
                        change_gamemode(stream, version, &mut player, uuid, gamemode).await?;
                        log_info!("Set {} to game mode {} from the console.", username, gamemode);
                    }
                    ConsoleCommand::Summon { entity_id, entity_type, x, y, z } => {
                        let network_entity_type = pumpkin_data::entity_id_remap::remap_entity_id_for_version(
                            entity_type,
                            version,
                        );
                        let packet = CSpawnEntity::new(
                            VarInt(entity_id),
                            Uuid::new_v4(),
                            VarInt(network_entity_type as i32),
                            Vector3 { x, y, z },
                            0.0,
                            0.0,
                            0.0,
                            VarInt(0),
                            Vector3::new(0.0, 0.0, 0.0),
                        );
                        let payload = encode_java_packet(&packet, version)?;
                        write_framed_payload(stream, payload.as_slice()).await?;
                    }
                    _ => {}
                }
            }
            Ok(event) = event_rx.recv() => {
                match event {
                    PlayerEvent::Join { entity_id, uuid: other_uuid, username: other_name, x, y, z, yaw, pitch, gamemode } => {
                        if other_uuid != uuid {
                            let flags = (PlayerInfoFlags::ADD_PLAYER
                                | PlayerInfoFlags::UPDATE_LISTED
                                | PlayerInfoFlags::UPDATE_GAME_MODE
                                | PlayerInfoFlags::UPDATE_LATENCY)
                                .bits();
                            let actions = vec![
                                PlayerAction::AddPlayer {
                                    name: &other_name,
                                    properties: &[],
                                },
                                PlayerAction::UpdateGameMode(VarInt(gamemode as i32)),
                                PlayerAction::UpdateListed(true),
                                PlayerAction::UpdateLatency(VarInt(0)),
                            ];
                            let players = vec![Player {
                                uuid: other_uuid,
                                actions: &actions,
                            }];
                            let player_info = CPlayerInfoUpdate::new(flags, &players);
                            if let Ok(payload) = encode_java_packet(&player_info, version) {
                                let _ = write_framed_payload(stream, payload.as_slice()).await;
                            }
                            let spawn_pkt = CSpawnEntity::new(
                                VarInt(entity_id),
                                other_uuid,
                                VarInt(pumpkin_data::entity::EntityType::PLAYER.id as i32),
                                Vector3 { x, y, z },
                                pitch,
                                yaw,
                                yaw,
                                VarInt(0),
                                Vector3 { x: 0.0, y: 0.0, z: 0.0 },
                            );
                            if let Ok(payload) = encode_java_packet(&spawn_pkt, version) {
                                let _ = write_framed_payload(stream, payload.as_slice()).await;
                            }
                        }
                    }
                    PlayerEvent::Move { entity_id, uuid: other_uuid, x, y, z, yaw, pitch } => {
                        if other_uuid != uuid {
                            let relative_flags: &[PositionFlag] = &[];
                            let tp_pkt = CTeleportEntity::new(
                                VarInt(entity_id),
                                Vector3 { x, y, z },
                                Vector3 { x: 0.0, y: 0.0, z: 0.0 },
                                yaw,
                                pitch,
                                relative_flags,
                                true,
                            );
                            if let Ok(payload) = encode_java_packet(&tp_pkt, version) {
                                let _ = write_framed_payload(stream, payload.as_slice()).await;
                            }
                            let head_yaw = (yaw * 256.0 / 360.0).rem_euclid(256.0) as u8;
                            let head_rot = CHeadRot::new(VarInt(entity_id), head_yaw);
                            if let Ok(payload) = encode_java_packet(&head_rot, version) {
                                let _ = write_framed_payload(stream, payload.as_slice()).await;
                            }
                        }
                    }
                    PlayerEvent::Leave { entity_id, uuid: other_uuid } => {
                        if other_uuid != uuid {
                            let ids = [VarInt(entity_id)];
                            let remove_pkt = CRemoveEntities::new(&ids);
                            if let Ok(payload) = encode_java_packet(&remove_pkt, version) {
                                let _ = write_framed_payload(stream, payload.as_slice()).await;
                            }
                            let uuids = [other_uuid];
                            let remove_tab_pkt = CRemovePlayerInfo::new(&uuids);
                            if let Ok(payload) = encode_java_packet(&remove_tab_pkt, version) {
                                let _ = write_framed_payload(stream, payload.as_slice()).await;
                            }
                        }
                    }
                    PlayerEvent::GamemodeChange { uuid: other_uuid, gamemode } => {
                        if other_uuid != uuid {
                            let actions = vec![
                                PlayerAction::UpdateGameMode(VarInt(gamemode as i32)),
                            ];
                            let players = vec![Player {
                                uuid: other_uuid,
                                actions: &actions,
                            }];
                            let info_update = CPlayerInfoUpdate::new(PlayerInfoFlags::UPDATE_GAME_MODE.bits(), &players);
                            if let Ok(payload) = encode_java_packet(&info_update, version) {
                                let _ = write_framed_payload(stream, payload.as_slice()).await;
                            }
                        }
                    }
                    PlayerEvent::Hurt { entity_id, uuid: target_uuid, damage, x, y, z, attacker_x, attacker_z } => {
                        let (kb_x, kb_z) = if let (Some(ax), Some(az)) = (attacker_x, attacker_z) {
                            let dx = x - ax;
                            let dz = z - az;
                            let len = (dx * dx + dz * dz).sqrt();
                            if len > 0.0 {
                                (dx / len * 0.4, dz / len * 0.4)
                            } else {
                                (0.0, 0.0)
                            }
                        } else {
                            (0.0, 0.0)
                        };

                        if target_uuid == uuid {
                            player.health -= damage;
                            if player.health < 0.0 { player.health = 0.0; }
                            use pumpkin_protocol::java::client::play::CSetHealth;
                            let hp = CSetHealth::new(player.health, VarInt(20), 20.0);
                            if let Ok(payload) = encode_java_packet(&hp, version) {
                                let _ = write_framed_payload(stream, payload.as_slice()).await;
                            }

                            use pumpkin_protocol::java::client::play::CEntityVelocity;
                            let vel = CEntityVelocity::new(
                                VarInt(my_entity_id),
                                Vector3::new(kb_x, 0.35, kb_z),
                            );
                            if let Ok(payload) = encode_java_packet(&vel, version) {
                                let _ = write_framed_payload(stream, payload.as_slice()).await;
                            }
                        }
                        let status = CEntityStatus::new(entity_id, 2);
                        if let Ok(payload) = encode_java_packet(&status, version) {
                            let _ = write_framed_payload(stream, payload.as_slice()).await;
                        }
                        use pumpkin_protocol::java::client::play::CSoundEffect;
                        use pumpkin_protocol::IdOr;
                        use pumpkin_data::sound::SoundCategory;
                        let hurt_sound = CSoundEffect::new(
                            IdOr::Id(pumpkin_data::sound::Sound::EntityPlayerHurt as u16),
                            SoundCategory::Players,
                            &Vector3::new(x, y, z),
                            1.0,
                            1.0,
                            12345.0,
                        );
                        if let Ok(payload) = encode_java_packet(&hurt_sound, version) {
                            let _ = write_framed_payload(stream, payload.as_slice()).await;
                        }
                        if attacker_x.is_some() {
                            let attack_sound = CSoundEffect::new(
                                IdOr::Id(pumpkin_data::sound::Sound::EntityPlayerAttackStrong as u16),
                                SoundCategory::Players,
                                &Vector3::new(x, y, z),
                                1.0,
                                1.0,
                                12345.0,
                            );
                            if let Ok(payload) = encode_java_packet(&attack_sound, version) {
                                let _ = write_framed_payload(stream, payload.as_slice()).await;
                            }
                        }
                    }
                }
            }
            Ok(msg) = chat_rx.recv() => {
                let text_comp = pumpkin_util::text::TextComponent::text(msg);
                let pkt = pumpkin_protocol::java::client::play::CSystemChatMessage::new(&text_comp, false);
                if let Ok(payload) = encode_java_packet(&pkt, version) {
                    let _ = write_framed_payload(stream, payload.as_slice()).await;
                }
            }
            Ok(item_event) = item_rx.recv() => {
                match item_event {
                    crate::viakraken::java::backend::state::ItemEvent::Spawn { entity_id, item_id, count, x, y, z, vx, vy, vz } => {
                        let network_entity_type = pumpkin_data::entity_id_remap::remap_entity_id_for_version(
                            pumpkin_data::entity::EntityType::ITEM.id,
                            version,
                        );
                        let spawn_pkt = CSpawnEntity::new(
                            VarInt(entity_id),
                            Uuid::new_v4(),
                            VarInt(network_entity_type as i32),
                            Vector3 { x, y, z },
                            0.0,
                            0.0,
                            0.0,
                            VarInt(0),
                            Vector3 { x: 0.0, y: 0.0, z: 0.0 },
                        );
                        if let Ok(payload) = encode_java_packet(&spawn_pkt, version) {
                            let _ = write_framed_payload(stream, payload.as_slice()).await;
                        }

                        if let Some(item) = pumpkin_data::item::Item::from_id(item_id) {
                            use pumpkin_data::{meta_data_type::MetaDataType, tracked_data::TrackedData};
                            use pumpkin_protocol::codec::item_stack_seralizer::ItemStackSerializer;
                            use pumpkin_protocol::java::client::play::{CSetEntityMetadata, Metadata};

                            let stack = pumpkin_data::item_stack::ItemStack::new(count, item);
                            let serialized = ItemStackSerializer::from(stack);
                            let metadata = Metadata::new(
                                TrackedData::ITEM,
                                MetaDataType::ITEM_STACK,
                                &serialized,
                            );
                            let mut metadata_bytes = Vec::new();
                            if metadata.write(&mut metadata_bytes, &version).is_ok() {
                                metadata_bytes.push(0xff);
                                let packet = CSetEntityMetadata::new(
                                    VarInt(entity_id),
                                    metadata_bytes.into_boxed_slice(),
                                );
                                if let Ok(payload) = encode_java_packet(&packet, version) {
                                    let _ = write_framed_payload(stream, payload.as_slice()).await;
                                }
                            }
                        }

                        use pumpkin_protocol::java::client::play::CEntityVelocity;
                        let vel = CEntityVelocity::new(
                            VarInt(entity_id),
                            Vector3::new(vx, vy, vz),
                        );
                        if let Ok(payload) = encode_java_packet(&vel, version) {
                            let _ = write_framed_payload(stream, payload.as_slice()).await;
                        }
                    }
                    crate::viakraken::java::backend::state::ItemEvent::Pickup { item_entity_id: _, player_entity_id: _ } => {
                        // We could send CPickupItem here if we implement picking up
                    }
                }
            }
            Ok(block_update) = block_rx.recv() => {
                let network_state = pumpkin_data::block_state_remap::remap_block_state_for_version(
                    block_update.state_id,
                    version,
                );
                let packet = pumpkin_protocol::java::client::play::CBlockUpdate::new(
                    BlockPos(Vector3 {
                        x: block_update.x,
                        y: block_update.y,
                        z: block_update.z,
                    }),
                    VarInt(network_state as i32),
                );
                if let Ok(payload) = encode_java_packet(&packet, version) {
                    let _ = write_framed_payload(stream, payload.as_slice()).await;
                }
            }
            _ = interval.tick() => {
                keep_alive_id = keep_alive_id.wrapping_add(1);
                let ka = CKeepAlive::new(keep_alive_id);
                if let Ok(payload) = encode_java_packet(&ka, version) {
                    let _ = write_framed_payload(stream, payload.as_slice()).await;
                }
            }
            _ = save_interval.tick() => {
                if let Err(error) = save_player(&db, uuid, &player) {
                    log_warn!("Failed to save player {}: {}", username, error);
                } else if let Err(error) = db.flush_async().await {
                    log_warn!("Failed to flush world data: {}", error);
                }
            }
            res = stream.read(&mut buf) => {
                match res {
                    Ok(0) => break 'play,
                    Err(_) => break 'play,
                    Ok(n) => {
                        pending_bytes.extend_from_slice(&buf[..n]);
                        loop {
                            let mut offset = 0usize;
                            let pkt_len = match read_varint_from_slice(&pending_bytes, &mut offset) {
                                Ok(len) if len > 0 => len as usize,
                                Ok(_) => { pending_bytes.clear(); break; }
                                Err(_) => break,
                            };

                            if offset + pkt_len > pending_bytes.len() {
                                break;
                            }

                            let pkt_data = pending_bytes[offset..offset + pkt_len].to_vec();
                            pending_bytes.drain(..offset + pkt_len);

                            if let Err(e) = handle_play_packet(
                                stream, version, &pkt_data,
                                &mut player, username, uuid, my_entity_id, &db,
                            ).await {
                                log_warn!("Play packet error for {}: {}", username, e);
                            }

                            let current_chunk = (
                                chunk_coordinate(player.x),
                                chunk_coordinate(player.z),
                            );
                            if current_chunk != center_chunk {
                                match stream_chunks(
                                    stream,
                                    version,
                                    protocol_version,
                                    &db,
                                    current_chunk.0,
                                    current_chunk.1,
                                    &mut sent_chunks,
                                    view_distance,
                                )
                                .await
                                {
                                    Ok(()) => center_chunk = current_chunk,
                                    Err(error) => {
                                        log_warn!("Failed to stream chunks for {}: {}", username, error);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    if let Err(error) = save_player(&db, uuid, &player) {
        log_warn!(
            "Failed to save player {} on disconnect: {}",
            username,
            error
        );
    }
    if let Err(error) = db.flush_async().await {
        log_warn!("Failed to flush world data on disconnect: {}", error);
    }
    {
        let mut players_guard = online_players().lock().unwrap();
        players_guard.remove(&uuid);
    }
    let _ = player_event_channel().send(PlayerEvent::Leave {
        entity_id: my_entity_id,
        uuid,
    });
    let _ = chat_channel().send(format!("{} left the game", username));

    log_info!("Player {} disconnected, state saved.", username);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_coordinates_floor_negative_positions() {
        assert_eq!(chunk_coordinate(0.0), 0);
        assert_eq!(chunk_coordinate(15.99), 0);
        assert_eq!(chunk_coordinate(16.0), 1);
        assert_eq!(chunk_coordinate(-0.01), -1);
        assert_eq!(chunk_coordinate(-16.0), -1);
        assert_eq!(chunk_coordinate(-16.01), -2);
    }

    #[test]
    fn moving_one_chunk_only_requests_the_new_edge() {
        let old_view = chunks_in_view(0, 0, 3);
        let new_view = chunks_in_view(1, 0, 3);

        assert_eq!(old_view.len(), 49);
        assert_eq!(new_view.difference(&old_view).count(), 7);
        assert_eq!(old_view.difference(&new_view).count(), 7);
    }

    #[test]
    fn client_view_distance_controls_streaming_radius() {
        assert_eq!(chunks_in_view(0, 0, 12).len(), 625);
    }
}
