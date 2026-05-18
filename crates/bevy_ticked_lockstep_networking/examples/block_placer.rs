use avian2d::prelude::*;
use bevy::prelude::*;
use bevy_ensemble::{
    EnsemblePlugin, Host, Lobby, LobbyParticipant, LobbyParticipantOf,
    LocalMultiplayerPlayerId, PendingLobby, PublicLobbies, StartHosting,
};
use bevy_ensemble_webrtc::{BevyEnsembleWebrtcPlugin, JoinWebrtcLobby, RefreshLobbyList};
use bevy_ticked::prelude::*;
use bevy_ticked_lockstep_networking::prelude::*;
use serde::{Deserialize, Serialize};

// --- Constants ---

const PLAYER_SPEED: f32 = 200.0;
const PLAYER_RADIUS: f32 = 16.0;
const DEFAULT_BLOCK_HALF_SIZE: f32 = 20.0;
const REMOVE_RANGE: f32 = 200.0;

// --- Action & Snapshot ---

#[derive(Clone, Debug, Serialize, Deserialize)]
enum Action {
    Move {
        direction: [f32; 2],
    },
    PlaceBlock {
        position: [f32; 2],
        half_size: [f32; 2],
    },
    RemoveBlock {
        block_id: u64,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
struct GameSnapshot {
    /// (player_uuid, position_x, position_y)
    players: Vec<(u128, f32, f32)>,
    /// (block_id, pos_x, pos_y, half_w, half_h, owner_uuid)
    blocks: Vec<(u64, f32, f32, f32, f32, u128)>,
    next_block_id: u64,
}

// --- Components ---

#[derive(Component)]
struct Player;

#[derive(Component, Clone, Debug, Serialize, Deserialize)]
struct PlayerUuid(u128);

#[derive(Component)]
struct Block;

#[derive(Component, Clone, Debug)]
struct BlockId(u64);

#[derive(Component)]
struct BlockOwner(u128);

#[derive(Component, Clone, Debug, Serialize, Deserialize)]
struct BlockHalfSize(Vec2);

#[derive(Resource, Default)]
struct NextBlockId(u64);

#[derive(Component)]
struct UiText;

// --- Player colors ---

const PLAYER_COLORS: &[Color] = &[
    Color::srgb(0.2, 0.7, 0.3),
    Color::srgb(0.3, 0.4, 0.9),
    Color::srgb(0.9, 0.3, 0.3),
    Color::srgb(0.9, 0.7, 0.2),
    Color::srgb(0.7, 0.3, 0.8),
    Color::srgb(0.3, 0.8, 0.8),
];

fn player_color(uuid: u128) -> Color {
    PLAYER_COLORS[(uuid % PLAYER_COLORS.len() as u128) as usize]
}

// --- Main ---

fn main() {
    let server_url = std::env::var("SIGNALLING_SERVER_URL")
        .ok()
        .or_else(|| option_env!("SIGNALLING_SERVER_URL").map(String::from))
        .unwrap_or_else(|| "ws://localhost:9090/ws".into());

    App::new()
        .add_plugins(DefaultPlugins)
        .add_plugins(EnsemblePlugin)
        .add_plugins(BevyEnsembleWebrtcPlugin {
            server_url,
            display_name: "Player".into(),
            ..default()
        })
        .add_plugins(TickedPlugin)
        .add_plugins(PhysicsPlugins::new(TickedSimulation).with_length_unit(1.0))
        .insert_resource(Gravity(Vec2::ZERO))
        .add_plugins(LockstepPlugin::<Action, GameSnapshot>::default())
        .init_resource::<NextBlockId>()
        // Startup
        .add_systems(Startup, setup)
        // Lobby management + input + visuals
        .add_systems(
            Update,
            (
                lobby_host_key,
                lobby_join_key,
                lobby_refresh_key,
                lobby_escape_key,
                cleanup_on_lobby_gone,
                spawn_player_on_lockstep_join,
                despawn_disconnected_players,
                capture_local_input,
                capture_join_snapshot.in_set(LockstepJoinSet::CaptureJoinSnapshot),
                apply_join_snapshot.in_set(LockstepJoinSet::ApplyJoinSnapshot),
                sync_visuals,
                update_ui,
            ),
        )
        // Simulation
        .add_systems(TickedSimulation, apply_actions)
        .run();
}

fn setup(mut commands: Commands) {
    commands.spawn(Camera2d);

    // UI
    commands.spawn((
        Text::new("H: Host | J: Join | R: Refresh | Esc: Leave"),
        Node {
            position_type: PositionType::Absolute,
            top: Val::Px(10.0),
            left: Val::Px(10.0),
            ..default()
        },
        UiText,
    ));
}

// --- Lobby management ---

fn lobby_host_key(
    keys: Res<ButtonInput<KeyCode>>,
    mut start_hosting: MessageWriter<StartHosting>,
    lobbies: Query<(), Or<(With<Lobby>, With<PendingLobby>)>>,
) {
    if keys.just_pressed(KeyCode::KeyH) && lobbies.is_empty() {
        start_hosting.write(StartHosting);
    }
}

fn lobby_join_key(
    keys: Res<ButtonInput<KeyCode>>,
    lobby_list: Option<Res<PublicLobbies>>,
    mut join_writer: MessageWriter<JoinWebrtcLobby>,
    lobbies: Query<(), Or<(With<Lobby>, With<PendingLobby>)>>,
) {
    if !keys.just_pressed(KeyCode::KeyJ) || !lobbies.is_empty() {
        return;
    }
    let Some(lobby_list) = lobby_list else { return };
    let Some(first) = lobby_list.0.first() else {
        return;
    };
    join_writer.write(JoinWebrtcLobby(first.lobby_id));
}

fn lobby_refresh_key(
    keys: Res<ButtonInput<KeyCode>>,
    mut refresh: MessageWriter<RefreshLobbyList>,
) {
    if keys.just_pressed(KeyCode::KeyR) {
        refresh.write(RefreshLobbyList);
    }
}

fn lobby_escape_key(
    mut commands: Commands,
    keys: Res<ButtonInput<KeyCode>>,
    lobbies: Query<Entity, Or<(With<Lobby>, With<PendingLobby>)>>,
) {
    if !keys.just_pressed(KeyCode::Escape) {
        return;
    }
    for entity in lobbies.iter() {
        commands.entity(entity).try_despawn();
    }
}

fn cleanup_on_lobby_gone(
    mut commands: Commands,
    mut removed_lobbies: RemovedComponents<Lobby>,
    players: Query<Entity, With<Player>>,
    blocks: Query<Entity, With<Block>>,
) {
    if removed_lobbies.read().next().is_none() {
        return;
    }
    for entity in players.iter().chain(blocks.iter()) {
        commands.entity(entity).try_despawn();
    }
    commands.remove_resource::<LocalMultiplayerPlayerId>();
    commands.init_resource::<NextBlockId>();
}

// --- Spawn player entity when a participant becomes active in lockstep ---

fn spawn_player_on_lockstep_join(
    mut commands: Commands,
    lobbies: Query<Entity, With<Lobby>>,
    new_lockstep_participants: Query<
        (&LobbyParticipant, &LobbyParticipantOf),
        Added<LockstepLobbyParticipant>,
    >,
    existing_players: Query<&PlayerUuid, With<Player>>,
) {
    let Some(lobby_entity) = lobbies.iter().next() else {
        return;
    };

    let mut player_index = existing_players.iter().count();

    for (participant, participant_of) in new_lockstep_participants.iter() {
        if participant_of.0 != lobby_entity {
            continue;
        }

        // Skip if player already exists (e.g. from snapshot)
        if existing_players
            .iter()
            .any(|uuid| uuid.0 == participant.player_uuid)
        {
            continue;
        }

        let spawn_x = if player_index % 2 == 0 { -100.0 } else { 100.0 };
        let spawn_pos = Vec2::new(spawn_x, 0.0);
        let color = player_color(participant.player_uuid);

        commands.spawn((
            Player,
            PlayerUuid(participant.player_uuid),
            RigidBody::Dynamic,
            Collider::circle(PLAYER_RADIUS),
            Position(spawn_pos),
            LockedAxes::ROTATION_LOCKED,
            Sprite {
                color,
                custom_size: Some(Vec2::splat(PLAYER_RADIUS * 2.0)),
                ..default()
            },
            Transform::from_translation(spawn_pos.extend(1.0)),
        ));

        player_index += 1;
    }
}

// --- Despawn players whose participant has left ---

fn despawn_disconnected_players(
    mut commands: Commands,
    lobbies: Query<Entity, With<Lobby>>,
    participants: Query<(&LobbyParticipant, &LobbyParticipantOf)>,
    players: Query<(Entity, &PlayerUuid), With<Player>>,
) {
    let Some(lobby) = lobbies.iter().next() else {
        return;
    };

    for (player_entity, uuid) in players.iter() {
        let still_active = participants
            .iter()
            .any(|(p, pof)| pof.0 == lobby && p.player_uuid == uuid.0);
        if !still_active {
            commands.entity(player_entity).try_despawn();
        }
    }
}

// --- Input capture ---

fn capture_local_input(
    keys: Res<ButtonInput<KeyCode>>,
    mouse_buttons: Res<ButtonInput<MouseButton>>,
    windows: Query<&Window>,
    cameras: Query<(&Camera, &GlobalTransform)>,
    local_player: Option<Res<LocalMultiplayerPlayerId>>,
    mut pending: ResMut<LocalPendingActions<Action>>,
    blocks: Query<(&Position, &BlockId, &BlockOwner), With<Block>>,
) {
    let Some(local_player) = local_player else {
        return;
    };
    let my_uuid = local_player.0;

    // Movement
    let mut dir = Vec2::ZERO;
    if keys.pressed(KeyCode::KeyW) {
        dir.y += 1.0;
    }
    if keys.pressed(KeyCode::KeyS) {
        dir.y -= 1.0;
    }
    if keys.pressed(KeyCode::KeyA) {
        dir.x -= 1.0;
    }
    if keys.pressed(KeyCode::KeyD) {
        dir.x += 1.0;
    }
    if dir != Vec2::ZERO {
        dir = dir.normalize();
    }
    pending.0.push(Action::Move {
        direction: [dir.x, dir.y],
    });

    // Mouse world position
    let world_pos = (|| {
        let window = windows.single().ok()?;
        let cursor_pos = window.cursor_position()?;
        let (camera, camera_transform) = cameras.single().ok()?;
        camera
            .viewport_to_world_2d(camera_transform, cursor_pos)
            .ok()
    })();

    let Some(world_pos) = world_pos else { return };

    // Place block (LMB)
    if mouse_buttons.just_pressed(MouseButton::Left) {
        pending.0.push(Action::PlaceBlock {
            position: [world_pos.x, world_pos.y],
            half_size: [DEFAULT_BLOCK_HALF_SIZE, DEFAULT_BLOCK_HALF_SIZE],
        });
    }

    // Remove closest own block within range (RMB)
    if mouse_buttons.just_pressed(MouseButton::Right) {
        let closest = blocks
            .iter()
            .filter(|(_, _, owner)| owner.0 == my_uuid)
            .filter(|(pos, _, _)| pos.0.distance(world_pos) < REMOVE_RANGE)
            .min_by(|(a, _, _), (b, _, _)| {
                let da = a.0.distance(world_pos);
                let db = b.0.distance(world_pos);
                da.partial_cmp(&db).unwrap()
            });

        if let Some((_, block_id, _)) = closest {
            pending.0.push(Action::RemoveBlock {
                block_id: block_id.0,
            });
        }
    }
}

// --- Simulation: apply actions from tracker ---

fn apply_actions(world: &mut World) {
    let current_tick = world.resource::<CurrentTick>().0;
    let actions: Vec<(u128, Vec<Action>)> = world
        .resource::<ActionTracker<Action>>()
        .actions_for_tick(current_tick)
        .map(|s| s.to_vec())
        .unwrap_or_default();

    for (player_uuid, player_actions) in &actions {
        for action in player_actions {
            match action {
                Action::Move { direction } => {
                    let velocity =
                        Vec2::new(direction[0], direction[1]) * PLAYER_SPEED;
                    let mut query =
                        world.query::<(&PlayerUuid, &mut LinearVelocity)>();
                    for (uuid, mut vel) in query.iter_mut(world) {
                        if uuid.0 == *player_uuid {
                            vel.0 = velocity;
                        }
                    }
                }
                Action::PlaceBlock {
                    position,
                    half_size,
                } => {
                    let new_pos = Vec2::new(position[0], position[1]);
                    let new_half = Vec2::new(half_size[0], half_size[1]);

                    // Check overlap with existing blocks (AABB)
                    let overlaps = {
                        let mut query = world.query_filtered::<(&Position, &BlockHalfSize), With<Block>>();
                        query.iter(world).any(|(pos, bhs)| {
                            aabb_overlap(new_pos, new_half, pos.0, bhs.0)
                        })
                    };

                    // Check overlap with players
                    let overlaps_player = if !overlaps {
                        let mut query = world.query_filtered::<&Position, With<Player>>();
                        query.iter(world).any(|pos| {
                            // Treat player as AABB with PLAYER_RADIUS half-size
                            aabb_overlap(
                                new_pos,
                                new_half,
                                pos.0,
                                Vec2::splat(PLAYER_RADIUS),
                            )
                        })
                    } else {
                        false
                    };

                    if !overlaps && !overlaps_player {
                        let block_id = {
                            let mut next = world.resource_mut::<NextBlockId>();
                            let id = next.0;
                            next.0 += 1;
                            id
                        };

                        let color = player_color(*player_uuid).with_alpha(0.7);

                        world.spawn((
                            Block,
                            BlockId(block_id),
                            BlockOwner(*player_uuid),
                            BlockHalfSize(new_half),
                            RigidBody::Static,
                            Collider::rectangle(new_half.x * 2.0, new_half.y * 2.0),
                            Position(new_pos),
                            Sprite {
                                color,
                                custom_size: Some(new_half * 2.0),
                                ..default()
                            },
                            Transform::from_translation(new_pos.extend(0.0)),
                        ));
                    }
                }
                Action::RemoveBlock { block_id } => {
                    let entity_to_despawn = {
                        let mut query = world.query::<(Entity, &BlockId, &BlockOwner)>();
                        query
                            .iter(world)
                            .find(|(_, bid, owner)| {
                                bid.0 == *block_id && owner.0 == *player_uuid
                            })
                            .map(|(e, _, _)| e)
                    };
                    if let Some(entity) = entity_to_despawn {
                        world.despawn(entity);
                    }
                }
            }
        }
    }
}

fn aabb_overlap(pos_a: Vec2, half_a: Vec2, pos_b: Vec2, half_b: Vec2) -> bool {
    let dx = (pos_a.x - pos_b.x).abs();
    let dy = (pos_a.y - pos_b.y).abs();
    dx < half_a.x + half_b.x && dy < half_a.y + half_b.y
}

// --- Join snapshot ---

fn capture_join_snapshot(
    mut requests: MessageReader<CaptureJoinSnapshot<GameSnapshot>>,
    players: Query<(&PlayerUuid, &Position), With<Player>>,
    blocks: Query<(&BlockId, &Position, &BlockHalfSize, &BlockOwner), With<Block>>,
    next_block_id: Res<NextBlockId>,
    mut responses: MessageWriter<ProvideJoinSnapshot<GameSnapshot>>,
) {
    for request in requests.read() {
        let snapshot = GameSnapshot {
            players: players
                .iter()
                .map(|(uuid, pos)| (uuid.0, pos.0.x, pos.0.y))
                .collect(),
            blocks: blocks
                .iter()
                .map(|(bid, pos, bhs, owner)| {
                    (bid.0, pos.0.x, pos.0.y, bhs.0.x, bhs.0.y, owner.0)
                })
                .collect(),
            next_block_id: next_block_id.0,
        };
        responses.write(ProvideJoinSnapshot {
            requester: request.requester,
            snapshot_tick: request.snapshot_tick,
            snapshot,
        });
    }
}

fn apply_join_snapshot(
    mut commands: Commands,
    mut snapshots: MessageReader<ApplyJoinSnapshot<GameSnapshot>>,
    existing_players: Query<Entity, With<Player>>,
    existing_blocks: Query<Entity, With<Block>>,
    mut current_tick: ResMut<CurrentTick>,
    mut tracker: ResMut<ActionTracker<Action>>,
    mut snapshot_applied: MessageWriter<JoinSnapshotApplied<GameSnapshot>>,
    mut next_block_id: ResMut<NextBlockId>,
) {
    for snapshot in snapshots.read() {
        // Despawn existing game entities
        for entity in existing_players.iter().chain(existing_blocks.iter()) {
            commands.entity(entity).try_despawn();
        }

        // Set tick state
        current_tick.0 = snapshot.snapshot_tick;
        tracker.ticks.clear();
        next_block_id.0 = snapshot.snapshot.next_block_id;

        // Spawn players from snapshot
        for &(uuid, x, y) in &snapshot.snapshot.players {
            let pos = Vec2::new(x, y);
            let color = player_color(uuid);
            commands.spawn((
                Player,
                PlayerUuid(uuid),
                RigidBody::Dynamic,
                Collider::circle(PLAYER_RADIUS),
                Position(pos),
                LockedAxes::ROTATION_LOCKED,
                Sprite {
                    color,
                    custom_size: Some(Vec2::splat(PLAYER_RADIUS * 2.0)),
                    ..default()
                },
                Transform::from_translation(pos.extend(1.0)),
            ));
        }

        // Spawn blocks from snapshot
        for &(bid, x, y, hw, hh, owner) in &snapshot.snapshot.blocks {
            let pos = Vec2::new(x, y);
            let half = Vec2::new(hw, hh);
            let color = player_color(owner).with_alpha(0.7);
            commands.spawn((
                Block,
                BlockId(bid),
                BlockOwner(owner),
                BlockHalfSize(half),
                RigidBody::Static,
                Collider::rectangle(hw * 2.0, hh * 2.0),
                Position(pos),
                Sprite {
                    color,
                    custom_size: Some(half * 2.0),
                    ..default()
                },
                Transform::from_translation(pos.extend(0.0)),
            ));
        }

        snapshot_applied.write(JoinSnapshotApplied::new(snapshot.snapshot_tick));
    }
}

// --- Visuals ---

fn sync_visuals(
    mut players: Query<(&Position, &mut Transform), With<Player>>,
    mut blocks: Query<(&Position, &mut Transform), (With<Block>, Without<Player>)>,
) {
    for (pos, mut transform) in players.iter_mut() {
        transform.translation = pos.0.extend(1.0);
    }
    for (pos, mut transform) in blocks.iter_mut() {
        transform.translation = pos.0.extend(0.0);
    }
}

// --- UI ---

fn update_ui(
    tick: Res<CurrentTick>,
    ticks_paused: Option<Res<TicksPaused>>,
    host_lobbies: Query<(), (With<Lobby>, With<Host>)>,
    client_lobbies: Query<(), (With<Lobby>, Without<Host>)>,
    pending_lobbies: Query<(), With<PendingLobby>>,
    lobby_list: Option<Res<PublicLobbies>>,
    participants: Query<(&LobbyParticipant, &LobbyParticipantOf)>,
    lobbies: Query<Entity, With<Lobby>>,
    blocks: Query<(), With<Block>>,
    mut ui: Query<&mut Text, With<UiText>>,
) {
    let Ok(mut text) = ui.single_mut() else {
        return;
    };

    if !pending_lobbies.is_empty() {
        **text = "Connecting...".to_string();
        return;
    }

    let is_host = !host_lobbies.is_empty();
    let is_client = !client_lobbies.is_empty();

    if !is_host && !is_client {
        let mut s = "H: Host | J: Join | R: Refresh".to_string();
        if let Some(lobby_list) = &lobby_list {
            if lobby_list.0.is_empty() {
                s.push_str("\nNo lobbies available");
            } else {
                for lobby in &lobby_list.0 {
                    s.push_str(&format!(
                        "\n  {} ({}/{})",
                        lobby.host_name, lobby.player_count, lobby.max_players,
                    ));
                }
            }
        }
        **text = s;
        return;
    }

    let role = if is_host { "HOST" } else { "CLIENT" };
    let lobby_entity = lobbies.iter().next();

    let mut player_count = 0;
    if let Some(lobby_entity) = lobby_entity {
        for (_, pof) in participants.iter() {
            if pof.0 == lobby_entity {
                player_count += 1;
            }
        }
    }

    let status = if ticks_paused.is_some() { "WAITING" } else { "PLAYING" };
    let block_count = blocks.iter().count();
    **text = format!(
        "[{}] Tick: {} [{}] | Players: {} | Blocks: {} | WASD: Move | LMB: Place | RMB: Remove | Esc: Leave",
        role, tick.0, status, player_count, block_count
    );
}
