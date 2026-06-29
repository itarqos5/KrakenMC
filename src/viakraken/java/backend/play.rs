use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use uuid::Uuid;

use pumpkin_protocol::codec::var_int::VarInt;
use pumpkin_protocol::java::client::play::{
    CCenterChunk, CChunkBatchEnd, CChunkBatchStart, CCommands, CEntityStatus, CGameEvent,
    CKeepAlive, CPlayerAbilities, CPlayerInfoUpdate, CPlayerPosition, CPlayerSpawnPosition,
    CRemoveEntities, CRemovePlayerInfo, CSpawnEntity, CTeleportEntity, GameEvent, Player,
    PlayerAction, PlayerInfoFlags, ProtoNode, ProtoNodeType, CCustomPayload, CHeadRot,
};
use pumpkin_protocol::ser::NetworkWriteExt;
use pumpkin_protocol::PositionFlag;
use pumpkin_util::math::position::BlockPos;
use pumpkin_util::math::vector3::Vector3;
use pumpkin_util::version::MinecraftVersion;

use crate::config::ServerConfig;
use crate::logger::{log_info, log_warn};
use crate::viakraken::java::packets::encode_java_packet;
use crate::viakraken::utils::{read_varint_from_slice, write_framed_payload};
use crate::world::chunk_gen::encode_chunk_packet;
use crate::world::player_store::{load_player, save_player};

use super::handler::handle_play_packet;
use super::state::{
    block_channel, chat_channel, gamemode_abilities, online_players, player_event_channel,
    NEXT_ENTITY_ID, OnlinePlayer, PlayerEvent,
};

pub async fn handle_play(
    stream: &mut TcpStream,
    config: &ServerConfig,
    version: MinecraftVersion,
    protocol_version: i32,
    username: &str,
    uuid: Uuid,
    db: Arc<sled::Db>,
) -> std::io::Result<()> {
    let mut player = load_player(&db, uuid);
    if player.inventory.len() != 46 {
        player.inventory = vec![vec![]; 46];
    }

    let my_entity_id = NEXT_ENTITY_ID.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

    let login_play = pumpkin_protocol::java::client::play::CLogin::new(
        my_entity_id,
        false,
        vec!["minecraft:overworld".to_string()],
        VarInt(3),
        VarInt(8),
        VarInt(8),
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

    {
        let nodes = vec![
            ProtoNode {
                children: vec![VarInt(1)].into_boxed_slice(),
                node_type: ProtoNodeType::Root,
            },
            ProtoNode {
                children: vec![VarInt(2), VarInt(3), VarInt(4), VarInt(5)].into_boxed_slice(),
                node_type: ProtoNodeType::Literal {
                    name: "gamemode",
                    is_executable: false,
                    redirect_target: None,
                    restricted: false,
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
            }
        ];
        let cmds = CCommands::new(nodes.into_boxed_slice(), VarInt(0));
        let cmds_payload = encode_java_packet(&cmds, version)?;
        write_framed_payload(stream, cmds_payload.as_slice()).await?;
    }

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
                Vector3 { x: other.x, y: other.y, z: other.z },
                other.pitch,
                other.yaw,
                other.yaw,
                VarInt(0),
                Vector3 { x: 0.0, y: 0.0, z: 0.0 },
            );
            if let Ok(payload) = encode_java_packet(&spawn_pkt, version) {
                let _ = write_framed_payload(stream, payload.as_slice()).await;
            }
        }
    }

    {
        let mut players_guard = online_players().lock().unwrap();
        players_guard.insert(uuid, OnlinePlayer {
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
        let _ = pkt_buf.write_var_int(&VarInt(pumpkin_data::packet::clientbound::PLAY_CONTAINER_SET_CONTENT.to_id(version) as i32));
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
        let spawn_pkt = CPlayerSpawnPosition::new(spawn_pos, 0.0, 0.0, "minecraft:overworld".to_string());
        let payload = encode_java_packet(&spawn_pkt, version)?;
        write_framed_payload(stream, payload.as_slice()).await?;
    }

    {
        let pos_pkt = CPlayerPosition::new(
            VarInt(1),
            Vector3 { x: player.x, y: player.y, z: player.z },
            Vector3 { x: 0.0, y: 0.0, z: 0.0 },
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

    let chunk_x = (player.x as i32) >> 4;
    let chunk_z = (player.z as i32) >> 4;
    {
        let center = CCenterChunk { chunk_x: VarInt(chunk_x), chunk_z: VarInt(chunk_z) };
        let payload = encode_java_packet(&center, version)?;
        write_framed_payload(stream, payload.as_slice()).await?;
    }

    let batch_start = CChunkBatchStart;
    let start_payload = encode_java_packet(&batch_start, version)?;
    write_framed_payload(stream, start_payload.as_slice()).await?;

    let mut chunk_count = 0u16;
    for dz in -3i32..=3 {
        for dx in -3i32..=3 {
            let cx = chunk_x + dx;
            let cz = chunk_z + dz;
            let chunk_data = encode_chunk_packet(cx, cz, protocol_version, &db);
            write_framed_payload(stream, &chunk_data).await?;
            chunk_count += 1;
        }
    }

    let batch_end = CChunkBatchEnd::new(chunk_count);
    let end_payload = encode_java_packet(&batch_end, version)?;
    write_framed_payload(stream, end_payload.as_slice()).await?;

    {
        let op_status = CEntityStatus::new(my_entity_id, 28);
        let payload = encode_java_packet(&op_status, version)?;
        write_framed_payload(stream, payload.as_slice()).await?;
    }

    {
        let brand_data = b"\x06Kraken";
        let brand_pkt = CCustomPayload::new("minecraft:brand", brand_data);
        let payload = encode_java_packet(&brand_pkt, version)?;
        write_framed_payload(stream, payload.as_slice()).await?;
    }

    log_info!(
        "Login flow completed for {} (protocol={}, max_players={}, pos=({:.1},{:.1},{:.1}))",
        username,
        protocol_version,
        config.max_players,
        player.x, player.y, player.z
    );

    let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
    let mut keep_alive_id = 0i64;
    let mut buf = vec![0u8; 65536];
    let mut pending_bytes = Vec::new();
    let mut chat_rx = chat_channel().subscribe();
    let mut block_rx = block_channel().subscribe();
    let mut event_rx = player_event_channel().subscribe();
    let mut item_rx = crate::viakraken::java::backend::state::item_event_channel().subscribe();

    'play: loop {
        tokio::select! {
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
                    crate::viakraken::java::backend::state::ItemEvent::Spawn { entity_id, item_id: _, x, y, z, vx, vy, vz } => {
                        let spawn_pkt = CSpawnEntity::new(
                            VarInt(entity_id),
                            Uuid::new_v4(),
                            VarInt(pumpkin_data::entity::EntityType::ITEM.id as i32),
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
            Ok(block_data) = block_rx.recv() => {
                let _ = write_framed_payload(stream, &block_data).await;
            }
            _ = interval.tick() => {
                keep_alive_id = keep_alive_id.wrapping_add(1);
                let ka = CKeepAlive::new(keep_alive_id);
                if let Ok(payload) = encode_java_packet(&ka, version) {
                    let _ = write_framed_payload(stream, payload.as_slice()).await;
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
                        }
                    }
                }
            }
        }
    }

    save_player(&db, uuid, &player);
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
