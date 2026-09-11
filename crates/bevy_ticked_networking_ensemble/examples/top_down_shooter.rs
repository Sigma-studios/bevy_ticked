use avian2d::prelude::*;
use bevy::prelude::*;
use bevy_ensemble::{
    EnsemblePlugin, Host, Lobby, LobbyParticipant, LobbyParticipantOf, LocalMultiplayerPlayerId,
    PendingLobby, PlayerOwned, PlayerOwnedEntities, PublicLobbies, StartHosting,
};
use bevy_ensemble_webrtc::{BevyEnsembleWebrtcPlugin, JoinWebrtcLobby, RefreshLobbyList};
use bevy_ticked::prelude::*;
use bevy_ticked_avian::avian2d::TickedAvianPlugin;
use bevy_ticked_networking::prelude::*;
use bevy_ticked_networking_ensemble::local_session::{self, TickedLocalSessionPlugin};
use bevy_ticked_networking_ensemble::{
    SpawnerSlots, TickedEnsembleSessionPlugin, TickedNetworkingEnsemblePlugin,
};
use serde::{Deserialize, Serialize};

// --- Constants ---

const MOVE_ACCEL: f32 = 8000.0;
const PLAYER_DRAG: f32 = 15.0;
const BULLET_SPEED: f32 = 600.0;
const BULLET_RADIUS: f32 = 4.0;
const PLAYER_RADIUS: f32 = 16.0;
const ARENA_HALF_W: f32 = 400.0;
const ARENA_HALF_H: f32 = 300.0;
const WALL_HALF_W: f32 = 10.0;
const WALL_HALF_H: f32 = 120.0;
const LASER_LENGTH: f32 = 1000.0;
const SHOOT_COOLDOWN_TICKS: u64 = 10;

// --- Input ---

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
struct PlayerInput {
    movement: [f32; 2],
    aim_angle: f32,
    shooting: bool,
}

// --- Networked components ---

#[derive(Component, PartialEq, Clone, Debug, Serialize, Deserialize, Default)]
struct AimAngle(f32);

#[derive(Component, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum EntityKind {
    Player,
    Bullet,
}

#[derive(Component, PartialEq, Clone, Debug, Serialize, Deserialize)]
struct SpawnPoint(Vec2);

#[derive(Component, PartialEq, Clone, Debug, Serialize, Deserialize, Default)]
struct ShootCooldown(u64);

/// The slot the player's peer mints ids under. On the body, networked, so every peer spawns
/// this player's bullets under the same ids: the shooter predicts the bullet, the host mints
/// the same id from the relayed input and confirms it.
#[derive(Component, PartialEq, Clone, Copy, Debug, Serialize, Deserialize)]
struct PlayerSlot(u8);

#[derive(Component)]
struct UiText;

// --- Plugin setup ---

fn main() {
    // `SIGNALLING_SERVER_URL`, or the launcher's own in-process server under
    // `TICKED_LOCAL_SESSION=N`, or the local default.
    let server_url = local_session::signalling_url();

    App::new()
        .add_plugins(DefaultPlugins)
        .add_plugins(EnsemblePlugin)
        .add_plugins(BevyEnsembleWebrtcPlugin {
            server_url,
            display_name: "Player".into(),
            ..default()
        })
        // `Hz`, not the default `FixedUpdate`: the networking plugins need a clock they can
        // steer, and refuse to build on Bevy's fixed one.
        .add_plugins(TickedPlugin {
            source: TickSource::Hz(64.0),
            ..default()
        })
        // avian on the tick, replay-safe: the four body components registered under
        // `avian::*`, warm starting off, sleeping off, the contact graph rolled back, bodies
        // placed by `Position`. The example used to write the registrations by hand.
        .add_plugins(PhysicsPlugins::new(TickedSimulation).with_length_unit(1.0))
        .add_plugins(TickedAvianPlugin::default())
        .insert_resource(Gravity(Vec2::ZERO))
        .add_plugins(TickedServerPlugin::<PlayerInput>::new())
        .add_plugins(TickedClientPlugin::<PlayerInput>::new())
        .add_plugins(TickedNetworkingEnsemblePlugin::<PlayerInput>::new())
        // The session plugin adopts the roles, runs the registry handshake and hands each
        // client a spawner slot; the example used to do the first by hand and the rest not at all.
        .add_plugins(TickedEnsembleSessionPlugin::default())
        // `TICKED_LOCAL_SESSION=2 cargo run --example ...`: two windows from one shell.
        .add_plugins(TickedLocalSessionPlugin)
        // The renderer blends each body between its last two tick states, and a correction
        // to a predicted body slides into place instead of blinking there. Neither touches
        // what the simulation reads.
        .add_plugins((TickedInterpolationPlugin, TickedSmoothingPlugin))
        // The local player's input, sampled once per tick inside the loop and filed for the
        // tick about to run: a keypress costs no extra frame, and a frame that runs two
        // ticks samples twice. It used to be an `Update` system stamping `tick + 1`.
        .add_plugins(TickedInputPlugin::<PlayerInput>::new(capture_local_input))
        // Register networked components. The wire name is the type's identity on the
        // wire and must be the same on every peer; registration order does not matter.
        // `Owner` — whose body this is — is the stack's own and is registered by it.
        .register_networked_ticked_component::<AimAngle>("AimAngle")
        .register_networked_ticked_component::<EntityKind>("EntityKind")
        .register_networked_ticked_component::<SpawnPoint>("SpawnPoint")
        .register_networked_ticked_component::<ShootCooldown>("ShootCooldown")
        .register_networked_ticked_component::<PlayerSlot>("PlayerSlot")
        // Startup
        .add_systems(Startup, setup)
        // Lobby management (Update)
        .add_systems(
            Update,
            (
                lobby_host_key,
                lobby_join_key,
                lobby_refresh_key,
                lobby_escape_key,
                cleanup_on_lobby_gone,
                server_spawn_players,
                sync_visuals,
                update_ui,
            ),
        )
        // Simulation systems (run inside TickedSimulation)
        .add_systems(
            TickedSimulation,
            (
                apply_inputs,
                move_bullets,
                bullet_collision,
                sync_bullet_transforms,
            )
                .chain(),
        )
        // React to networked entity lifecycle
        .add_observer(on_entity_spawned)
        .add_observer(on_replication_mode_changed)
        .run();
}

fn setup(mut commands: Commands) {
    commands.spawn(Camera2d);

    // Arena border (visual only)
    commands.spawn((
        Sprite {
            color: Color::srgb(0.15, 0.15, 0.2),
            custom_size: Some(Vec2::new(ARENA_HALF_W * 2.0, ARENA_HALF_H * 2.0)),
            ..default()
        },
        Transform::from_xyz(0.0, 0.0, -1.0),
    ));

    // Arena walls (static colliders)
    let wall_thickness = 20.0;
    // Top
    commands.spawn((
        RigidBody::Static,
        Collider::rectangle(ARENA_HALF_W * 2.0 + wall_thickness * 2.0, wall_thickness),
        Position(Vec2::new(0.0, ARENA_HALF_H + wall_thickness / 2.0)),
    ));
    // Bottom
    commands.spawn((
        RigidBody::Static,
        Collider::rectangle(ARENA_HALF_W * 2.0 + wall_thickness * 2.0, wall_thickness),
        Position(Vec2::new(0.0, -ARENA_HALF_H - wall_thickness / 2.0)),
    ));
    // Left
    commands.spawn((
        RigidBody::Static,
        Collider::rectangle(wall_thickness, ARENA_HALF_H * 2.0),
        Position(Vec2::new(-ARENA_HALF_W - wall_thickness / 2.0, 0.0)),
    ));
    // Right
    commands.spawn((
        RigidBody::Static,
        Collider::rectangle(wall_thickness, ARENA_HALF_H * 2.0),
        Position(Vec2::new(ARENA_HALF_W + wall_thickness / 2.0, 0.0)),
    ));

    // Wall in the middle
    commands.spawn((
        Sprite {
            color: Color::srgb(0.5, 0.5, 0.6),
            custom_size: Some(Vec2::new(WALL_HALF_W * 2.0, WALL_HALF_H * 2.0)),
            ..default()
        },
        RigidBody::Static,
        Collider::rectangle(WALL_HALF_W * 2.0, WALL_HALF_H * 2.0),
    ));

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

/// When the lobby is removed (local leave or host disconnect), clean up all game entities.
fn cleanup_on_lobby_gone(
    mut commands: Commands,
    mut removed_lobbies: RemovedComponents<Lobby>,
    game_entities: Query<Entity, With<TickTrackedEntity>>,
) {
    if removed_lobbies.read().next().is_none() {
        return;
    }
    for entity in game_entities.iter() {
        commands.entity(entity).try_despawn();
    }
    commands.remove_resource::<LocalMultiplayerPlayerId>();
    commands.remove_resource::<LocalServerPlayer>();
    commands.remove_resource::<LocalClientPlayer>();
}

// --- Server: spawn player entities when participants join ---

fn server_spawn_players(
    mut commands: Commands,
    host_lobbies: Query<Entity, (With<Lobby>, With<Host>)>,
    new_participants: Query<
        (Entity, &LobbyParticipant, &LobbyParticipantOf),
        Without<PlayerOwnedEntities>,
    >,
    existing_players: Query<(), (With<EntityKind>, With<PlayerOwned>)>,
    mut counter: ResMut<TrackedIdAllocator>,
    slots: Option<Res<SpawnerSlots>>,
    local_player: Option<Res<LocalMultiplayerPlayerId>>,
) {
    let Some(lobby_entity) = host_lobbies.iter().next() else {
        return;
    };

    let mut player_index = existing_players.iter().count();

    for (participant_entity, participant, participant_of) in new_participants.iter() {
        if participant_of.0 != lobby_entity {
            continue;
        }
        // A body carries its player's spawner slot, so it waits for the slot: the host's own
        // is 0, a client's arrives with the registry handshake.
        let slot = if local_player
            .as_ref()
            .is_some_and(|me| me.0 == participant.player_uuid)
        {
            0
        } else {
            match slots
                .as_ref()
                .and_then(|slots| slots.slot_of(participant.player_uuid))
            {
                Some(slot) => slot,
                None => continue,
            }
        };

        // Alternate spawn sides
        let spawn_x = if player_index % 2 == 0 {
            -ARENA_HALF_W * 0.6
        } else {
            ARENA_HALF_W * 0.6
        };
        let spawn_pos = Vec2::new(spawn_x, 0.0);
        let tracked_id = counter.next_authority();

        commands.spawn((
            tracked_id,
            EntityKind::Player,
            RigidBody::Dynamic,
            Collider::circle(PLAYER_RADIUS),
            Position(spawn_pos),
            LinearDamping(PLAYER_DRAG),
            LockedAxes::ROTATION_LOCKED,
            AimAngle(0.0),
            SpawnPoint(spawn_pos),
            ShootCooldown::default(),
            Owner(participant.player_uuid),
            PlayerSlot(slot),
            PlayerOwned(participant_entity),
        ));

        player_index += 1;
    }
}

// --- The local player's input, sampled by `TickedInputPlugin` once per tick ---

fn capture_local_input(
    keys: Res<ButtonInput<KeyCode>>,
    mouse_buttons: Res<ButtonInput<MouseButton>>,
    windows: Query<&Window>,
    cameras: Query<(&Camera, &GlobalTransform)>,
    local: Res<LocalPlayer>,
    players: Query<(&Position, &Owner)>,
) -> Option<PlayerInput> {
    // No session, no body to drive.
    if local.0 == 0 {
        return None;
    }
    let my_uuid = local.0;

    // Movement from WASD
    let mut movement = Vec2::ZERO;
    if keys.pressed(KeyCode::KeyW) {
        movement.y += 1.0;
    }
    if keys.pressed(KeyCode::KeyS) {
        movement.y -= 1.0;
    }
    if keys.pressed(KeyCode::KeyA) {
        movement.x -= 1.0;
    }
    if keys.pressed(KeyCode::KeyD) {
        movement.x += 1.0;
    }
    if movement != Vec2::ZERO {
        movement = movement.normalize();
    }

    // Aim angle from mouse position relative to player
    let mut aim_angle = 0.0;
    if let Ok(window) = windows.single() {
        if let Some(cursor_pos) = window.cursor_position() {
            if let Ok((camera, camera_transform)) = cameras.single() {
                if let Ok(world_pos) = camera.viewport_to_world_2d(camera_transform, cursor_pos) {
                    // Find our player entity to get position
                    for (pos, uuid) in players.iter() {
                        if uuid.0 == my_uuid {
                            let dir = world_pos - pos.0;
                            aim_angle = dir.y.atan2(dir.x);
                            break;
                        }
                    }
                }
            }
        }
    }

    let shooting = mouse_buttons.pressed(MouseButton::Left);

    Some(PlayerInput {
        movement: [movement.x, movement.y],
        aim_angle,
        shooting,
    })
}

// --- Simulation systems (run in TickedSimulation) ---

/// Drive every body this peer simulates from its owner's input for this tick — or the newest
/// one before it: a player whose input for this tick has not arrived is still pressing what
/// they last pressed, far more often than nothing. With nothing, a predicted remote body
/// stood still for every tick past the relayed inputs and snapped forward at the snapshot.
///
/// Only the bodies this peer drives: on the host that is everyone; on a client it is the local
/// player's body and any the game marked `Predicted`. Everybody else's body is interpolated —
/// put at the authority's state after each snapshot — and takes no input here at all. That
/// is the whole of what used to be the "remote body with no input" workaround.
fn apply_inputs(
    tick: Res<CurrentTick>,
    time: Res<Time>,
    input_queue: Res<InputQueue<PlayerInput>>,
    local_client: Option<Res<LocalClientPlayer>>,
    mut players: Query<(
        &mut LinearVelocity,
        &mut AimAngle,
        &mut ShootCooldown,
        &Owner,
        &EntityKind,
        Option<&ReplicationMode>,
    )>,
) {
    let tick_inputs = input_queue.at_tick_or_last(tick.0);
    let dt = time.delta_secs();

    for (mut vel, mut aim, mut cooldown, uuid, kind, mode) in players.iter_mut() {
        if *kind != EntityKind::Player || !drives(local_client.as_deref(), mode) {
            continue;
        }
        if let Some(input) = tick_inputs.get(&uuid.0) {
            let movement = Vec2::new(input.movement[0], input.movement[1]);
            vel.0 += movement * MOVE_ACCEL * dt;
            aim.0 = input.aim_angle;

            if cooldown.0 > 0 {
                cooldown.0 -= 1;
            }
        }
    }
}

fn move_bullets(world: &mut World) {
    let dt = world.resource::<Time>().delta_secs();
    let tick = world.resource::<CurrentTick>().0;
    let input_queue = world.resource::<InputQueue<PlayerInput>>();

    // Collect shooting requests from this tick's inputs
    let mut shoot_requests: Vec<(u128, f32)> = Vec::new();
    if let Some(tick_inputs) = input_queue.at_tick(tick) {
        for (uuid, input) in tick_inputs {
            if input.shooting {
                shoot_requests.push((*uuid, input.aim_angle));
            }
        }
    }

    // Move existing bullets
    let mut bullets_to_despawn = Vec::new();
    {
        let mut query = world.query::<(
            Entity,
            &mut Position,
            &AimAngle,
            &EntityKind,
            &TickTrackedEntity,
        )>();
        for (entity, mut pos, aim, kind, _) in query.iter_mut(world) {
            if *kind != EntityKind::Bullet {
                continue;
            }
            let dir = Vec2::new(aim.0.cos(), aim.0.sin());
            pos.0 += dir * BULLET_SPEED * dt;

            // Despawn if out of arena
            if pos.0.x.abs() > ARENA_HALF_W + 50.0 || pos.0.y.abs() > ARENA_HALF_H + 50.0 {
                bullets_to_despawn.push(entity);
            }

            // Wall collision — despawn bullet
            if pos.0.x > -WALL_HALF_W - BULLET_RADIUS
                && pos.0.x < WALL_HALF_W + BULLET_RADIUS
                && pos.0.y > -WALL_HALF_H - BULLET_RADIUS
                && pos.0.y < WALL_HALF_H + BULLET_RADIUS
            {
                bullets_to_despawn.push(entity);
            }
        }
    }

    for entity in bullets_to_despawn {
        world.entity_mut(entity).despawn_ticked();
    }

    // Spawn new bullets, on every peer, under the shooter's slot. Sorted by uuid so every
    // peer mints in the same order.
    let mut player_data: Vec<(u128, Vec2, u64, u8)> = Vec::new();
    {
        let mut query =
            world.query::<(&Owner, &Position, &ShootCooldown, &PlayerSlot, &EntityKind)>();
        for (uuid, pos, cooldown, slot, kind) in query.iter(world) {
            if *kind == EntityKind::Player {
                player_data.push((uuid.0, pos.0, cooldown.0, slot.0));
            }
        }
    }
    shoot_requests.sort_by_key(|(uuid, _)| *uuid);

    let mut spawns = Vec::new();
    for (uuid, aim_angle) in &shoot_requests {
        if let Some((_, pos, cooldown, slot)) = player_data.iter().find(|(u, ..)| u == uuid) {
            if *cooldown > 0 {
                continue;
            }
            let dir = Vec2::new(aim_angle.cos(), aim_angle.sin());
            let bullet_pos = *pos + dir * (PLAYER_RADIUS + BULLET_RADIUS + 2.0);
            spawns.push((*uuid, *slot, bullet_pos, *aim_angle));
        }
    }

    for (owner_uuid, slot, bullet_pos, aim_angle) in spawns {
        world.spawn_tracked_by(
            SpawnerSlot(slot),
            (
                EntityKind::Bullet,
                Position(bullet_pos),
                AimAngle(aim_angle),
                Owner(owner_uuid),
            ),
        );

        // Reset cooldown on the player
        let mut query = world.query::<(&Owner, &mut ShootCooldown, &EntityKind)>();
        for (uuid, mut cooldown, kind) in query.iter_mut(world) {
            if *kind == EntityKind::Player && uuid.0 == owner_uuid {
                cooldown.0 = SHOOT_COOLDOWN_TICKS;
            }
        }
    }
}

fn bullet_collision(world: &mut World) {
    // Collect bullet positions
    let mut bullets: Vec<(Entity, Vec2, u128)> = Vec::new();
    {
        let mut query = world.query::<(Entity, &Position, &Owner, &EntityKind)>();
        for (entity, pos, uuid, kind) in query.iter(world) {
            if *kind == EntityKind::Bullet {
                bullets.push((entity, pos.0, uuid.0));
            }
        }
    }

    // Collect player positions
    let mut players: Vec<(Entity, Vec2, u128, Vec2)> = Vec::new();
    {
        let mut query = world.query::<(Entity, &Position, &Owner, &SpawnPoint, &EntityKind)>();
        for (entity, pos, uuid, spawn, kind) in query.iter(world) {
            if *kind == EntityKind::Player {
                players.push((entity, pos.0, uuid.0, spawn.0));
            }
        }
    }

    let mut bullets_to_despawn = Vec::new();
    let mut players_to_respawn: Vec<(Entity, Vec2)> = Vec::new();

    for (bullet_entity, bullet_pos, bullet_owner) in &bullets {
        for (player_entity, player_pos, player_uuid, spawn_point) in &players {
            // Don't hit the shooter
            if *bullet_owner == *player_uuid {
                continue;
            }
            let dist = bullet_pos.distance(*player_pos);
            if dist < PLAYER_RADIUS + BULLET_RADIUS {
                bullets_to_despawn.push(*bullet_entity);
                players_to_respawn.push((*player_entity, *spawn_point));
            }
        }
    }

    for entity in bullets_to_despawn {
        world.entity_mut(entity).despawn_ticked();
    }

    for (entity, spawn_pos) in players_to_respawn {
        if let Some(mut pos) = world.entity_mut(entity).get_mut::<Position>() {
            pos.0 = spawn_pos;
        }
        if let Some(mut vel) = world.entity_mut(entity).get_mut::<LinearVelocity>() {
            vel.0 = Vec2::ZERO;
        }
    }
}

// --- Who simulates what ---

/// Whether this peer simulates a body from input: the host simulates every body, a client
/// only the ones it predicts. `ReplicationMode` is a client-side marker; absent means
/// interpolated, so on a client only an explicit `Predicted` counts.
fn drives(local_client: Option<&LocalClientPlayer>, mode: Option<&ReplicationMode>) -> bool {
    local_client.is_none() || matches!(mode, Some(ReplicationMode::Predicted))
}

/// The physics body a player gets on this peer.
///
/// On the host every body is dynamic: the host simulates everyone from their inputs. On a
/// client only the local player's is — the body the replay predicts from inputs this client
/// has — and so is anything the game marks `Predicted`. Every other player's body is
/// interpolated: after each snapshot it is put at the authority's state, and a dynamic body
/// would fight that restore every tick, damping and colliding and integrating from a velocity
/// the host has since changed. Kinematic, it goes where it is put and coasts on the velocity
/// it was given until the next restore. The host owns its motion; this peer only shows it.
fn body_kind(
    local_client: Option<&LocalClientPlayer>,
    owner: Option<&Owner>,
    mode: Option<&ReplicationMode>,
) -> RigidBody {
    let Some(local) = local_client else {
        return RigidBody::Dynamic;
    };
    let mine = owner.is_some_and(|owner| owner.0 == local.0);
    if mine || matches!(mode, Some(ReplicationMode::Predicted)) {
        RigidBody::Dynamic
    } else {
        RigidBody::Kinematic
    }
}

// --- Entity lifecycle observers ---

/// A snapshot-spawned entity gets its `TickTrackedEntity` last, after every networked component
/// including `Owner`, so this is where the owner is known and the body kind can be chosen.
fn on_entity_spawned(
    trigger: On<Add, TickTrackedEntity>,
    mut commands: Commands,
    query: Query<(
        &EntityKind,
        &Position,
        Option<&Owner>,
        Option<&ReplicationMode>,
    )>,
    local_client: Option<Res<LocalClientPlayer>>,
) {
    let entity = trigger.entity;
    let Ok((kind, pos, owner, mode)) = query.get(entity) else {
        return;
    };
    let transform = Transform::from_translation(pos.0.extend(0.0));

    match kind {
        EntityKind::Player => {
            commands.entity(entity).insert((
                Sprite {
                    color: Color::srgb(0.2, 0.7, 0.3),
                    custom_size: Some(Vec2::splat(PLAYER_RADIUS * 2.0)),
                    ..default()
                },
                transform,
                TickedInterpolation::default(),
                // The local player's body is exempt on its own: a correction to what you
                // are steering should be felt. A correction bigger than a few body widths is
                // a respawn, and is meant to be seen.
                CorrectionSmoothing {
                    max_offset: PLAYER_RADIUS * 4.0,
                    ..default()
                },
                // Physics components needed on every peer: the host and a predicting client
                // simulate the body, an interpolating client shows it. Which kind, below.
                body_kind(local_client.as_deref(), owner, mode),
                Collider::circle(PLAYER_RADIUS),
                LinearDamping(PLAYER_DRAG),
                LockedAxes::ROTATION_LOCKED,
            ));

            // Spawn laser child
            commands.entity(entity).with_children(|parent| {
                parent.spawn((
                    Sprite {
                        color: Color::srgba(1.0, 0.2, 0.2, 0.3),
                        custom_size: Some(Vec2::new(LASER_LENGTH, 2.0)),
                        ..default()
                    },
                    bevy::sprite::Anchor::CENTER_LEFT,
                    Transform::default(),
                ));
            });
        }
        EntityKind::Bullet => {
            commands.entity(entity).insert((
                Sprite {
                    color: Color::srgb(1.0, 0.9, 0.2),
                    custom_size: Some(Vec2::splat(BULLET_RADIUS * 2.0)),
                    ..default()
                },
                transform,
                TickedInterpolation::default(),
            ));
        }
    }
}

/// A body handed from one mode to the other after it spawned — a game predicting a remote
/// player it is wrestling with, or giving up on that — changes physics body with it.
fn on_replication_mode_changed(
    trigger: On<Insert, ReplicationMode>,
    mut commands: Commands,
    players: Query<(&EntityKind, Option<&Owner>, &ReplicationMode)>,
    local_client: Option<Res<LocalClientPlayer>>,
) {
    let Ok((EntityKind::Player, owner, mode)) = players.get(trigger.entity) else {
        return;
    };
    commands
        .entity(trigger.entity)
        .insert(body_kind(local_client.as_deref(), owner, Some(mode)));
}

/// Bullets have no physics body, so nothing writes their transform inside the tick the way
/// avian writes a player's; done here, after they moved, so `TickedInterpolation` records one
/// state per tick and blends between them.
fn sync_bullet_transforms(
    mut bullets: Query<(&Position, &EntityKind, &mut Transform), With<TickTrackedEntity>>,
) {
    for (pos, kind, mut transform) in bullets.iter_mut() {
        if *kind == EntityKind::Bullet {
            transform.translation = pos.0.extend(0.0);
        }
    }
}

/// Per-frame visuals that are not the position: avian writes a player's transform inside the
/// tick and bullets get theirs from `sync_bullet_transforms`; `TickedInterpolation` blends
/// both for the renderer, so writing translations here would only fight it.
fn sync_visuals(
    mut tracked: Query<
        (&AimAngle, &EntityKind, &mut Transform, Option<&Children>),
        With<TickTrackedEntity>,
    >,
    mut child_transforms: Query<&mut Transform, Without<TickTrackedEntity>>,
) {
    for (aim, kind, mut transform, children) in tracked.iter_mut() {
        match kind {
            EntityKind::Player => {
                // Rotate the laser child instead of the player entity.
                // Avian owns the player's Transform.rotation (via physics Rotation),
                // so writing to it here would fight with avian's transform sync and
                // cause jitter during rollback+replay on the client.
                if let Some(children) = children {
                    for child in children.iter() {
                        if let Ok(mut ct) = child_transforms.get_mut(child) {
                            ct.rotation = Quat::from_rotation_z(aim.0);
                        }
                    }
                }
            }
            EntityKind::Bullet => {
                transform.rotation = Quat::from_rotation_z(aim.0);
            }
        }
    }
}

// --- UI ---

fn update_ui(
    tick: Res<CurrentTick>,
    holds: Res<TickHolds>,
    host_lobbies: Query<(), (With<Lobby>, With<Host>)>,
    client_lobbies: Query<(), (With<Lobby>, Without<Host>)>,
    pending_lobbies: Query<(), With<PendingLobby>>,
    lobby_list: Option<Res<PublicLobbies>>,
    participants: Query<(&LobbyParticipant, &LobbyParticipantOf)>,
    lobbies: Query<Entity, With<Lobby>>,
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

    let status = if holds.is_held() { "PAUSED" } else { "PLAYING" };
    **text = format!(
        "[{}] Tick: {} [{}] | Players: {} | WASD: Move | Mouse: Aim | LMB: Shoot | Esc: Leave",
        role, tick.0, status, player_count
    );
}
