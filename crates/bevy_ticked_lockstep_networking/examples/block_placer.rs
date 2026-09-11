//! A lockstep game in one file: players move, place blocks, remove their own blocks.
//!
//! # Everything that is simulation happens inside the tick
//!
//! Players are spawned *inside* `TickedSimulation`, at the tick the roster says they joined,
//! from the roster alone. They used to be spawned from an `Update` observer on the frame the
//! `ParticipantJoined` arrived — which is a different tick on every peer, so every peer's
//! world had the new body from a different moment on, and the checksums (had there been any)
//! disagreed from the join onwards. The same for blocks: `apply_actions` spawns game state,
//! and `attach_visuals` in `Update` puts a sprite on whatever appeared.
//!
//! A departed player's body stays. There is no tick on which every peer agrees the player
//! left — the roster change lands on each peer's frame clock — so despawning it on that frame
//! would desync exactly as the old spawn did. Part 2 of the lockstep phase puts a leave on the
//! tick timeline; until then a body without a player is an obstacle.
//!
//! # It desyncs loudly
//!
//! `ChecksumLogPlugin` samples the positions, velocities and blocks once a second, and
//! `ChecksumExchangePlugin` compares them across peers. A disagreement is an `error!` and a red
//! line in the UI naming the tick and the section, rather than a building on one screen that
//! is not on the other.

#[cfg(all(feature = "transport-webrtc", feature = "transport-steam"))]
compile_error!("Features `transport-webrtc` and `transport-steam` are mutually exclusive.");

#[cfg(not(any(feature = "transport-webrtc", feature = "transport-steam")))]
compile_error!("One of `transport-webrtc` or `transport-steam` must be enabled.");

use avian2d::prelude::*;
use bevy::prelude::*;
#[cfg(feature = "transport-webrtc")]
use bevy_ensemble::PublicLobbies;
use bevy_ensemble::{
    EnsemblePlugin, Host, Lobby, LobbyParticipant, LobbyParticipantOf, LocalMultiplayerPlayerId,
    PendingLobby, StartHosting,
};
#[cfg(feature = "transport-steam")]
use bevy_ensemble_steam::{BevyEnsembleSteamPlugin, JoinSteamLobby, SteamFriendLobbies};
#[cfg(feature = "transport-webrtc")]
use bevy_ensemble_webrtc::{BevyEnsembleWebrtcPlugin, JoinWebrtcLobby, RefreshLobbyList};
use bevy_ticked::prelude::*;
use bevy_ticked_lockstep_networking::ChecksumLogPlugin;
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

/// A player as it crosses the wire: everything the tick integrates from.
///
/// Velocity included. A snapshot that carried only positions handed the joiner a body at rest
/// where the host's was moving, and the two integrated apart from the first tick after the
/// join — a desync built into the join itself, before anybody pressed a key.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct PlayerSnapshot {
    uuid: u128,
    position: [f32; 2],
    velocity: [f32; 2],
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
struct GameSnapshot {
    players: Vec<PlayerSnapshot>,
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

// --- The world hash ---

/// What has to agree between peers, in two sections so a report says which one does not.
#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
struct GameHash {
    players: u64,
    blocks: u64,
}

/// FNV-1a over the bits, so `-0.0` and `0.0` — which compare equal and integrate the same —
/// hash the same on every peer.
fn fnv(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
}

fn fnv_f32(hash: &mut u64, value: f32) {
    let value = if value == 0.0 { 0.0 } else { value };
    fnv(hash, &value.to_bits().to_le_bytes());
}

impl WorldHash for GameHash {
    fn sample(world: &mut World) -> Self {
        // Sorted by uuid and id: a query's iteration order is not part of the simulation.
        let mut players: Vec<(u128, Vec2, Vec2)> = world
            .query_filtered::<(&PlayerUuid, &Position, &LinearVelocity), With<Player>>()
            .iter(world)
            .map(|(uuid, position, velocity)| (uuid.0, position.0, velocity.0))
            .collect();
        players.sort_by_key(|(uuid, _, _)| *uuid);
        let mut players_hash = 0xcbf2_9ce4_8422_2325;
        for (uuid, position, velocity) in players {
            fnv(&mut players_hash, &uuid.to_le_bytes());
            for value in [position.x, position.y, velocity.x, velocity.y] {
                fnv_f32(&mut players_hash, value);
            }
        }

        let mut blocks: Vec<(u64, u128, Vec2, Vec2)> = world
            .query_filtered::<(&BlockId, &BlockOwner, &Position, &BlockHalfSize), With<Block>>()
            .iter(world)
            .map(|(id, owner, position, half)| (id.0, owner.0, position.0, half.0))
            .collect();
        blocks.sort_by_key(|(id, _, _, _)| *id);
        let mut blocks_hash = 0xcbf2_9ce4_8422_2325;
        fnv(
            &mut blocks_hash,
            &world.resource::<NextBlockId>().0.to_le_bytes(),
        );
        for (id, owner, position, half) in blocks {
            fnv(&mut blocks_hash, &id.to_le_bytes());
            fnv(&mut blocks_hash, &owner.to_le_bytes());
            for value in [position.x, position.y, half.x, half.y] {
                fnv_f32(&mut blocks_hash, value);
            }
        }

        GameHash {
            players: players_hash,
            blocks: blocks_hash,
        }
    }

    fn value(&self) -> u64 {
        self.players ^ self.blocks.rotate_left(32)
    }

    fn differences(&self, other: &Self) -> Vec<&'static str> {
        let mut differences = Vec::new();
        if self.players != other.players {
            differences.push("players");
        }
        if self.blocks != other.blocks {
            differences.push("blocks");
        }
        differences
    }
}

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
    let mut app = App::new();
    app.add_plugins(DefaultPlugins).add_plugins(EnsemblePlugin);

    #[cfg(feature = "transport-webrtc")]
    {
        let server_url = std::env::var("SIGNALLING_SERVER_URL")
            .ok()
            .or_else(|| option_env!("SIGNALLING_SERVER_URL").map(String::from))
            .unwrap_or_else(|| "ws://localhost:9090/ws".into());
        app.add_plugins(BevyEnsembleWebrtcPlugin {
            server_url,
            display_name: "Player".into(),
            ..default()
        });
    }

    #[cfg(feature = "transport-steam")]
    app.add_plugins(BevyEnsembleSteamPlugin::default());

    app.add_plugins(TickedPlugin {
        source: TickSource::Hz(64.0),
        ..default()
    })
    .add_plugins(PhysicsPlugins::new(TickedSimulation).with_length_unit(1.0))
    .insert_resource(Gravity(Vec2::ZERO))
    .add_plugins((
        LockstepPlugin::<Action, GameSnapshot>::default(),
        AdaptiveTickBufferPlugin,
        // After physics, so the hash describes a finished tick: positions that were
        // integrated, not the ones the actions were applied to.
        ChecksumLogPlugin::<GameHash>::default().in_set(PhysicsSystems::Last),
        ChecksumExchangePlugin::<GameHash>::default(),
    ))
    .init_resource::<NextBlockId>()
    // Startup
    .add_systems(Startup, setup)
    // Lobby management + input + visuals
    .add_systems(
        Update,
        (
            lobby_host_key,
            lobby_escape_key,
            cleanup_on_lobby_gone,
            capture_local_input,
            capture_join_snapshot.in_set(LockstepJoinSet::CaptureJoinSnapshot),
            apply_join_snapshot.in_set(LockstepJoinSet::ApplyJoinSnapshot),
            attach_visuals,
            sync_visuals,
            update_ui,
        ),
    );

    #[cfg(feature = "transport-webrtc")]
    app.add_systems(Update, (lobby_join_key_webrtc, lobby_refresh_key_webrtc));

    #[cfg(feature = "transport-steam")]
    app.add_systems(Update, (lobby_join_key_steam, lobby_refresh_key_steam));

    // Before physics, so the tick's bodies exist and the tick's velocities are set when it
    // integrates. Chained: a player has to be spawned before its first action can move it.
    app.add_systems(
        TickedSimulation,
        (spawn_joined_players, apply_actions)
            .chain()
            .before(PhysicsSystems::First),
    )
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

#[cfg(feature = "transport-webrtc")]
fn lobby_join_key_webrtc(
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

#[cfg(feature = "transport-steam")]
fn lobby_join_key_steam(
    keys: Res<ButtonInput<KeyCode>>,
    lobby_list: Option<Res<SteamFriendLobbies>>,
    mut join_writer: MessageWriter<JoinSteamLobby>,
    lobbies: Query<(), Or<(With<Lobby>, With<PendingLobby>)>>,
) {
    if !keys.just_pressed(KeyCode::KeyJ) || !lobbies.is_empty() {
        return;
    }
    let Some(lobby_list) = lobby_list else { return };
    let Some(first) = lobby_list.0.first() else {
        return;
    };
    join_writer.write(JoinSteamLobby(first.lobby_id));
}

#[cfg(feature = "transport-webrtc")]
fn lobby_refresh_key_webrtc(
    keys: Res<ButtonInput<KeyCode>>,
    mut refresh: MessageWriter<RefreshLobbyList>,
) {
    if keys.just_pressed(KeyCode::KeyR) {
        refresh.write(RefreshLobbyList);
    }
}

#[cfg(feature = "transport-steam")]
fn lobby_refresh_key_steam(mut commands: Commands, keys: Res<ButtonInput<KeyCode>>) {
    if keys.just_pressed(KeyCode::KeyR) {
        commands.remove_resource::<SteamFriendLobbies>();
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

/// The whole world goes with the lobby. Frame-side, and deterministic in the only sense that
/// matters here: there is nobody left to agree with.
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

// --- Simulation: the roster ---

/// Spawn a body for every participant whose `joined_at_tick` has come, once.
///
/// From the roster, in the tick, so every peer spawns it on the same tick from the same
/// number. `joined_at_tick` is what the host chose and told everybody; the roster reaches a
/// client before the authoritative tick it names (both travel the same ordered link, in that
/// order), and the lockstep plugin applies it before the tick loop runs.
///
/// `<=` rather than `==`, and "no body yet" rather than "the tick just arrived": the host's own
/// `joined_at_tick` is the tick it was already on when it started hosting, and a joiner's
/// snapshot already holds every body whose tick is before the snapshot's — so what this
/// spawns is exactly the bodies that are due and not yet there. Sorted by uuid, so two players
/// who loaded in the same frame spawn in the same order on every peer.
fn spawn_joined_players(
    mut commands: Commands,
    current_tick: Res<CurrentTick>,
    lobbies: Query<Entity, With<Lobby>>,
    participants: Query<(
        &LobbyParticipant,
        &LockstepLobbyParticipant,
        &LobbyParticipantOf,
    )>,
    existing_players: Query<&PlayerUuid, With<Player>>,
) {
    let Some(lobby_entity) = lobbies.iter().next() else {
        return;
    };

    let mut due: Vec<u128> = participants
        .iter()
        .filter(|(_, lockstep, of)| {
            of.0 == lobby_entity && lockstep.joined_at_tick <= current_tick.0
        })
        .map(|(participant, _, _)| participant.player_uuid)
        .filter(|uuid| !existing_players.iter().any(|existing| existing.0 == *uuid))
        .collect();
    due.sort_unstable();
    due.dedup();

    let mut player_index = existing_players.iter().count();
    for uuid in due {
        let spawn_x = if player_index % 2 == 0 { -100.0 } else { 100.0 };
        let spawn_pos = Vec2::new(spawn_x, 0.0);
        commands.spawn((
            Player,
            PlayerUuid(uuid),
            RigidBody::Dynamic,
            Collider::circle(PLAYER_RADIUS),
            Position(spawn_pos),
            LinearVelocity::ZERO,
            LockedAxes::ROTATION_LOCKED,
        ));
        player_index += 1;
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

/// Game state only. A block is a body with an id and an owner; its sprite is
/// `attach_visuals`'s business, on the frame side.
fn apply_actions(world: &mut World) {
    let current_tick = world.resource::<CurrentTick>().0;
    let actions: Vec<(u128, Vec<Action>)> = world
        .resource::<ActionTracker<Action>>()
        .actions_for_tick(current_tick)
        .map(|btree| btree.iter().map(|(k, v)| (*k, v.clone())).collect())
        .unwrap_or_default();

    for (player_uuid, player_actions) in &actions {
        for action in player_actions {
            match action {
                Action::Move { direction } => {
                    let velocity = Vec2::new(direction[0], direction[1]) * PLAYER_SPEED;
                    let mut query = world.query::<(&PlayerUuid, &mut LinearVelocity)>();
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
                        let mut query =
                            world.query_filtered::<(&Position, &BlockHalfSize), With<Block>>();
                        query
                            .iter(world)
                            .any(|(pos, bhs)| aabb_overlap(new_pos, new_half, pos.0, bhs.0))
                    };

                    // Check overlap with players
                    let overlaps_player = if !overlaps {
                        let mut query = world.query_filtered::<&Position, With<Player>>();
                        query.iter(world).any(|pos| {
                            // Treat player as AABB with PLAYER_RADIUS half-size
                            aabb_overlap(new_pos, new_half, pos.0, Vec2::splat(PLAYER_RADIUS))
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

                        world.spawn((
                            Block,
                            BlockId(block_id),
                            BlockOwner(*player_uuid),
                            BlockHalfSize(new_half),
                            RigidBody::Static,
                            Collider::rectangle(new_half.x * 2.0, new_half.y * 2.0),
                            Position(new_pos),
                        ));
                    }
                }
                Action::RemoveBlock { block_id } => {
                    let entity_to_despawn = {
                        let mut query = world.query::<(Entity, &BlockId, &BlockOwner)>();
                        query
                            .iter(world)
                            .find(|(_, bid, owner)| bid.0 == *block_id && owner.0 == *player_uuid)
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
    players: Query<(&PlayerUuid, &Position, &LinearVelocity), With<Player>>,
    blocks: Query<(&BlockId, &Position, &BlockHalfSize, &BlockOwner), With<Block>>,
    next_block_id: Res<NextBlockId>,
    mut responses: MessageWriter<ProvideJoinSnapshot<GameSnapshot>>,
) {
    for request in requests.read() {
        let snapshot = GameSnapshot {
            players: players
                .iter()
                .map(|(uuid, pos, vel)| PlayerSnapshot {
                    uuid: uuid.0,
                    position: [pos.0.x, pos.0.y],
                    velocity: [vel.0.x, vel.0.y],
                })
                .collect(),
            blocks: blocks
                .iter()
                .map(|(bid, pos, bhs, owner)| (bid.0, pos.0.x, pos.0.y, bhs.0.x, bhs.0.y, owner.0))
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
        for player in &snapshot.snapshot.players {
            let pos = Vec2::new(player.position[0], player.position[1]);
            let vel = Vec2::new(player.velocity[0], player.velocity[1]);
            commands.spawn((
                Player,
                PlayerUuid(player.uuid),
                RigidBody::Dynamic,
                Collider::circle(PLAYER_RADIUS),
                Position(pos),
                LinearVelocity(vel),
                LockedAxes::ROTATION_LOCKED,
            ));
        }

        // Spawn blocks from snapshot
        for &(bid, x, y, hw, hh, owner) in &snapshot.snapshot.blocks {
            let pos = Vec2::new(x, y);
            let half = Vec2::new(hw, hh);
            commands.spawn((
                Block,
                BlockId(bid),
                BlockOwner(owner),
                BlockHalfSize(half),
                RigidBody::Static,
                Collider::rectangle(hw * 2.0, hh * 2.0),
                Position(pos),
            ));
        }

        snapshot_applied.write(JoinSnapshotApplied::new(snapshot.snapshot_tick));
    }
}

// --- Visuals ---

/// Put a sprite on every body the tick spawned bare. Frame-side: a headless peer has no
/// sprites and ticks identically without them.
fn attach_visuals(
    mut commands: Commands,
    players: Query<(Entity, &PlayerUuid, &Position), (With<Player>, Without<Sprite>)>,
    blocks: Query<(Entity, &BlockOwner, &Position, &BlockHalfSize), (With<Block>, Without<Sprite>)>,
) {
    for (entity, uuid, position) in players.iter() {
        commands.entity(entity).insert((
            Sprite {
                color: player_color(uuid.0),
                custom_size: Some(Vec2::splat(PLAYER_RADIUS * 2.0)),
                ..default()
            },
            Transform::from_translation(position.0.extend(1.0)),
        ));
    }
    for (entity, owner, position, half) in blocks.iter() {
        commands.entity(entity).insert((
            Sprite {
                color: player_color(owner.0).with_alpha(0.7),
                custom_size: Some(half.0 * 2.0),
                ..default()
            },
            Transform::from_translation(position.0.extend(0.0)),
        ));
    }
}

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

fn format_in_game_ui(
    tick: u64,
    is_paused: bool,
    is_host: bool,
    lobby_entity: Entity,
    participants: &Query<(&LobbyParticipant, &LobbyParticipantOf)>,
    block_count: usize,
    desync: Option<&Desync<GameHash>>,
) -> String {
    let role = if is_host { "HOST" } else { "CLIENT" };
    let player_count = participants
        .iter()
        .filter(|(_, pof)| pof.0 == lobby_entity)
        .count();
    let status = if is_paused { "WAITING" } else { "PLAYING" };
    let mut line = format!(
        "[{role}] Tick: {tick} [{status}] | Players: {player_count} | Blocks: {block_count} | WASD: Move | LMB: Place | RMB: Remove | Esc: Leave"
    );
    if let Some(desync) = desync {
        line.push_str(&format!(
            "\nDESYNC against {}: {}",
            desync.peer, desync.divergence
        ));
    }
    line
}

#[cfg(feature = "transport-webrtc")]
fn update_ui(
    tick: Res<CurrentTick>,
    holds: Res<TickHolds>,
    host_lobbies: Query<(), (With<Lobby>, With<Host>)>,
    client_lobbies: Query<(), (With<Lobby>, Without<Host>)>,
    pending_lobbies: Query<(), With<PendingLobby>>,
    lobby_list: Option<Res<PublicLobbies>>,
    participants: Query<(&LobbyParticipant, &LobbyParticipantOf)>,
    lobbies: Query<Entity, With<Lobby>>,
    blocks: Query<(), With<Block>>,
    desync: Option<Res<Desync<GameHash>>>,
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

    let Some(lobby_entity) = lobbies.iter().next() else {
        return;
    };
    **text = format_in_game_ui(
        tick.0,
        holds.is_held(),
        is_host,
        lobby_entity,
        &participants,
        blocks.iter().count(),
        desync.as_deref(),
    );
}

#[cfg(feature = "transport-steam")]
fn update_ui(
    tick: Res<CurrentTick>,
    holds: Res<TickHolds>,
    host_lobbies: Query<(), (With<Lobby>, With<Host>)>,
    client_lobbies: Query<(), (With<Lobby>, Without<Host>)>,
    pending_lobbies: Query<(), With<PendingLobby>>,
    lobby_list: Option<Res<SteamFriendLobbies>>,
    participants: Query<(&LobbyParticipant, &LobbyParticipantOf)>,
    lobbies: Query<Entity, With<Lobby>>,
    blocks: Query<(), With<Block>>,
    desync: Option<Res<Desync<GameHash>>>,
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
        let mut s = "H: Host | J: Join (friend lobby) | R: Refresh".to_string();
        if let Some(lobby_list) = &lobby_list {
            if lobby_list.0.is_empty() {
                s.push_str("\nNo friend lobbies found");
            } else {
                for lobby in &lobby_list.0 {
                    s.push_str(&format!(
                        "\n  {} ({} players)",
                        lobby.host_name, lobby.member_count,
                    ));
                }
            }
        }
        **text = s;
        return;
    }

    let Some(lobby_entity) = lobbies.iter().next() else {
        return;
    };
    **text = format_in_game_ui(
        tick.0,
        holds.is_held(),
        is_host,
        lobby_entity,
        &participants,
        blocks.iter().count(),
        desync.as_deref(),
    );
}
