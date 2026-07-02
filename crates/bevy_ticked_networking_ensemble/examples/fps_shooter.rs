//! A minimal **3D first-person** networked shooter, mirroring `top_down_shooter`
//! but in first person using the `bevy_elan` character controller + FPS camera.
//!
//! Everything is drawn with unlit cubes: a large ground plane, a few pillars for
//! cover, cube players and cube bullets. There is no weapon model — just a
//! crosshair. Movement/look/shoot are **input-only** and simulated inside
//! `TickedSimulation`, so the whole thing rolls back deterministically.
//!
//! `bevy_elan` runs in its **driven** mode: the controller reads `ControllerInput`
//! and `ControllerTime` (which we feed from the tick) instead of the keyboard and
//! wall clock, and every controller system runs chained inside `TickedSimulation`.

use avian3d::prelude::*;
use bevy::input::mouse::MouseMotion;
use bevy::prelude::*;
use bevy_elan::prelude::*;
use bevy_ensemble::{
    EnsemblePlugin, Host, Lobby, LobbyParticipant, LobbyParticipantOf, LocalMultiplayerPlayerId,
    PendingLobby, PlayerOwned, PlayerOwnedEntities, PublicLobbies, StartHosting,
};
use bevy_ensemble_webrtc::{BevyEnsembleWebrtcPlugin, JoinWebrtcLobby, RefreshLobbyList};
use bevy_ticked::prelude::*;
use bevy_ticked_networking::prelude::*;
use bevy_ticked_networking_ensemble::TickedNetworkingEnsemblePlugin;
use serde::{Deserialize, Serialize};

// --- Constants ---

const ARENA_HALF: f32 = 50.0;
const PLAYER_HIT_RADIUS: f32 = 0.5;
const BULLET_SPEED: f32 = 150.0;
/// Overrides elan's default `move_speed` (1.0) to make the player noticeably faster.
const PLAYER_MOVE_SPEED: f32 = 4.0;
const BULLET_HIT_RADIUS: f32 = 0.15;
const EYE_HEIGHT: f32 = 0.5;
const MOUSE_SENSITIVITY: f32 = 0.003;
const MAX_PITCH: f32 = 1.5; // radians, just shy of straight up/down
const SHOOT_COOLDOWN_TICKS: u64 = 12;
const SPAWN_RING_RADIUS: f32 = 20.0;

/// Pillar centres on the ground (y is derived from the half-extents so they sit
/// on the floor). Shared by the visuals, the colliders and the bullet AABB test.
const PILLARS: [Vec2; 5] = [
    Vec2::new(0.0, 0.0),
    Vec2::new(14.0, 10.0),
    Vec2::new(-14.0, 10.0),
    Vec2::new(14.0, -10.0),
    Vec2::new(-14.0, -10.0),
];
const PILLAR_HALF: Vec3 = Vec3::new(1.0, 3.0, 1.0);

// --- Input (sent over the wire — input only) ---

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
struct PlayerInput {
    /// `[strafe, forward]`, each in `-1..=1`.
    move_dir: [f32; 2],
    /// Absolute `[yaw, pitch]` in radians.
    look: [f32; 2],
    jump: bool,
    shooting: bool,
}

// --- Networked components (order must match on all peers) ---

/// Absolute view orientation. Source of truth for player aim and bullet travel;
/// elan's (non-networked) `Look` is derived from this every tick.
#[derive(Component, Clone, Copy, Debug, Serialize, Deserialize, Default)]
struct Aim {
    yaw: f32,
    pitch: f32,
}

impl Aim {
    /// Forward unit vector for this orientation (Bevy's -Z convention).
    fn forward(self) -> Vec3 {
        Quat::from_rotation_y(self.yaw) * Quat::from_rotation_x(self.pitch) * Vec3::NEG_Z
    }
}

#[derive(Component, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum EntityKind {
    Player,
    Bullet,
}

#[derive(Component, Clone, Debug, Serialize, Deserialize)]
struct SpawnPoint(Vec3);

#[derive(Component, Clone, Debug, Serialize, Deserialize, Default)]
struct ShootCooldown(u64);

/// Links a player's UUID to their game entity.
#[derive(Component, Clone, Debug, Serialize, Deserialize)]
struct PlayerUuid(u128);

// --- Local-only marker components ---

#[derive(Component)]
struct UiText;

/// A free camera used only in the menu (before a player entity exists).
#[derive(Component)]
struct MenuCamera;

/// Locally accumulated look, integrated from raw mouse motion each frame and sent
/// as the absolute `look` in `PlayerInput`.
#[derive(Resource, Default)]
struct LocalLook {
    yaw: f32,
    pitch: f32,
}

// --- Plugin setup ---

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
        .add_plugins(PhysicsPlugins::new(TickedSimulation))
        .insert_resource(Gravity(Vec3::NEG_Y * 9.81))
        // bevy_elan in driven mode: every controller system runs chained inside
        // TickedSimulation; it reads ControllerInput / ControllerTime, not devices.
        .add_plugins(CharacterController3dPlugin::in_schedule(TickedSimulation))
        // Cursor grab for the FPS camera (without the default mouse-look, which we
        // replace with deterministic, input-driven look).
        .add_plugins(CursorGrabPlugin)
        .add_plugins(TickedServerPlugin::<PlayerInput>::new())
        .add_plugins(TickedClientPlugin::<PlayerInput>::new())
        .add_plugins(TickedNetworkingEnsemblePlugin::<PlayerInput>::new())
        .init_resource::<LocalLook>()
        // Register networked components (order must match on all peers)
        .register_networked_ticked_component::<Position>()
        .register_networked_ticked_component::<Rotation>()
        .register_networked_ticked_component::<LinearVelocity>()
        .register_networked_ticked_component::<AngularVelocity>()
        .register_networked_ticked_component::<Aim>()
        .register_networked_ticked_component::<EntityKind>()
        .register_networked_ticked_component::<SpawnPoint>()
        .register_networked_ticked_component::<ShootCooldown>()
        .register_networked_ticked_component::<PlayerUuid>()
        // Startup
        .add_systems(Startup, setup)
        // Per-frame (Update)
        .add_systems(
            Update,
            (
                lobby_host_key,
                lobby_join_key,
                lobby_refresh_key,
                lobby_escape_key,
                cleanup_on_lobby_gone,
                on_lobby_ready,
                server_spawn_players,
                capture_local_input,
                manage_cameras,
                sync_visuals,
                update_ui,
            ),
        )
        // Simulation systems (run inside TickedSimulation).
        // set_controller_time + apply_inputs feed elan before its ControllerSet;
        // the controller runs before avian's Prepare so the yaw it writes reaches
        // the physics Rotation; bullets move after physics writeback.
        .add_systems(
            TickedSimulation,
            (set_controller_time, apply_inputs)
                .chain()
                .before(ControllerSet),
        )
        .configure_sets(TickedSimulation, ControllerSet.before(PhysicsSystems::Prepare))
        .add_systems(
            TickedSimulation,
            (move_bullets, bullet_collision)
                .chain()
                .after(PhysicsSystems::Writeback),
        )
        // React to networked entity lifecycle
        .add_observer(on_entity_spawned)
        .run();
}

fn setup(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    // Ground: a large flat unlit cube whose top face sits at y = 0.
    let ground_size = ARENA_HALF * 2.0;
    commands.spawn((
        Mesh3d(meshes.add(Cuboid::new(ground_size, 1.0, ground_size))),
        MeshMaterial3d(materials.add(unlit(Color::srgb(0.12, 0.13, 0.16)))),
        Transform::from_xyz(0.0, -0.5, 0.0),
        RigidBody::Static,
        Collider::cuboid(ground_size, 1.0, ground_size),
    ));

    // Pillars: tall unlit cubes used as cover.
    let pillar_mesh = meshes.add(Cuboid::new(
        PILLAR_HALF.x * 2.0,
        PILLAR_HALF.y * 2.0,
        PILLAR_HALF.z * 2.0,
    ));
    let pillar_mat = materials.add(unlit(Color::srgb(0.45, 0.47, 0.55)));
    for p in PILLARS {
        commands.spawn((
            Mesh3d(pillar_mesh.clone()),
            MeshMaterial3d(pillar_mat.clone()),
            Transform::from_xyz(p.x, PILLAR_HALF.y, p.y),
            RigidBody::Static,
            Collider::cuboid(PILLAR_HALF.x * 2.0, PILLAR_HALF.y * 2.0, PILLAR_HALF.z * 2.0),
        ));
    }

    // A free menu camera so the UI renders before we join. `manage_cameras`
    // swaps it out for the first-person camera once the local player exists.
    commands.spawn((
        MenuCamera,
        Camera3d::default(),
        Transform::from_xyz(0.0, 35.0, 45.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));

    // UI text.
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

    // Crosshair: a small white square centred on screen.
    commands
        .spawn(Node {
            position_type: PositionType::Absolute,
            width: Val::Percent(100.0),
            height: Val::Percent(100.0),
            justify_content: JustifyContent::Center,
            align_items: AlignItems::Center,
            ..default()
        })
        .with_children(|parent| {
            parent.spawn((
                Node {
                    width: Val::Px(6.0),
                    height: Val::Px(6.0),
                    ..default()
                },
                BackgroundColor(Color::srgba(1.0, 1.0, 1.0, 0.8)),
            ));
        });
}

fn unlit(color: Color) -> StandardMaterial {
    StandardMaterial {
        base_color: color,
        unlit: true,
        ..default()
    }
}

// --- Lobby management (identical in spirit to top_down_shooter) ---

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

fn on_lobby_ready(
    mut commands: Commands,
    local_player: Option<Res<LocalMultiplayerPlayerId>>,
    server_player: Option<Res<LocalServerPlayer>>,
    client_player: Option<Res<LocalClientPlayer>>,
    host_lobbies: Query<(), (With<Lobby>, With<Host>)>,
    client_lobbies: Query<(), (With<Lobby>, Without<Host>)>,
) {
    let Some(local_player) = local_player else {
        return;
    };

    if !host_lobbies.is_empty() && server_player.is_none() {
        commands.insert_resource(LocalServerPlayer(local_player.0));
    }
    if !client_lobbies.is_empty() && client_player.is_none() {
        commands.insert_resource(LocalClientPlayer(local_player.0));
    }
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
    mut counter: ResMut<TickTrackedEntityCounter>,
) {
    let Some(lobby_entity) = host_lobbies.iter().next() else {
        return;
    };

    let mut player_index = existing_players.iter().count();

    for (participant_entity, participant, participant_of) in new_participants.iter() {
        if participant_of.0 != lobby_entity {
            continue;
        }

        // Spread players around a ring; they drop onto the floor and hover.
        let angle = player_index as f32 * std::f32::consts::TAU / 6.0;
        let spawn_pos = Vec3::new(
            angle.cos() * SPAWN_RING_RADIUS,
            2.0,
            angle.sin() * SPAWN_RING_RADIUS,
        );
        // Face roughly toward the arena centre.
        let yaw = angle + std::f32::consts::PI;
        let tracked_id = counter.next();

        commands.spawn((
            tracked_id,
            EntityKind::Player,
            character_controller_bundle(),
            Position(spawn_pos),
            Aim { yaw, pitch: 0.0 },
            SpawnPoint(spawn_pos),
            ShootCooldown::default(),
            PlayerUuid(participant.player_uuid),
            PlayerOwned(participant_entity),
        ));

        player_index += 1;
    }
}

// --- Client: capture local input each frame and write to InputQueue ---

fn capture_local_input(
    keys: Res<ButtonInput<KeyCode>>,
    mouse_buttons: Res<ButtonInput<MouseButton>>,
    mut motion: MessageReader<MouseMotion>,
    tick: Res<CurrentTick>,
    local_client: Option<Res<LocalClientPlayer>>,
    local_server: Option<Res<LocalServerPlayer>>,
    mut local_look: ResMut<LocalLook>,
    mut input_queue: ResMut<InputQueue<PlayerInput>>,
) {
    let my_uuid = local_client
        .as_ref()
        .map(|p| p.0)
        .or_else(|| local_server.as_ref().map(|p| p.0));
    let Some(my_uuid) = my_uuid else {
        motion.clear();
        return;
    };

    // Integrate raw mouse motion into an absolute look. Sent as an absolute value
    // so replaying the same input is deterministic.
    let delta: Vec2 = motion.read().map(|m| m.delta).sum();
    local_look.yaw -= delta.x * MOUSE_SENSITIVITY;
    local_look.pitch = (local_look.pitch - delta.y * MOUSE_SENSITIVITY).clamp(-MAX_PITCH, MAX_PITCH);

    // Movement: x = strafe (right positive), y = forward (forward positive).
    let mut move_dir = Vec2::ZERO;
    if keys.pressed(KeyCode::KeyW) {
        move_dir.y += 1.0;
    }
    if keys.pressed(KeyCode::KeyS) {
        move_dir.y -= 1.0;
    }
    if keys.pressed(KeyCode::KeyD) {
        move_dir.x += 1.0;
    }
    if keys.pressed(KeyCode::KeyA) {
        move_dir.x -= 1.0;
    }

    let input = PlayerInput {
        move_dir: [move_dir.x, move_dir.y],
        look: [local_look.yaw, local_look.pitch],
        jump: keys.pressed(KeyCode::Space),
        shooting: mouse_buttons.pressed(MouseButton::Left),
    };

    input_queue.insert(tick.0 + 1, my_uuid, input);
}

// --- Simulation systems (run in TickedSimulation) ---

/// Feed elan its clock from the tick counter so its timers are deterministic.
fn set_controller_time(tick: Res<CurrentTick>, mut controller_time: ResMut<ControllerTime>) {
    controller_time.delta = SECONDS_PER_TICK;
    controller_time.elapsed = tick.0 as f32 * SECONDS_PER_TICK;
}

/// Apply this tick's inputs to each player: drive elan's `ControllerInput`,
/// update `Aim`, mirror it into elan's `Look`, and tick down the shoot cooldown.
fn apply_inputs(
    tick: Res<CurrentTick>,
    input_queue: Res<InputQueue<PlayerInput>>,
    mut players: Query<(
        &mut ControllerInput,
        &mut Look,
        &mut Aim,
        &mut ShootCooldown,
        &PlayerUuid,
        &EntityKind,
    )>,
) {
    let Some(tick_inputs) = input_queue.at_tick(tick.0) else {
        return;
    };

    for (mut input, mut look, mut aim, mut cooldown, uuid, kind) in players.iter_mut() {
        if *kind != EntityKind::Player {
            continue;
        }
        if let Some(player_input) = tick_inputs.get(&uuid.0) {
            input.move_dir = Vec2::new(player_input.move_dir[0], player_input.move_dir[1]);
            input.jump = player_input.jump;
            aim.yaw = player_input.look[0];
            aim.pitch = player_input.look[1];
            // elan's apply_look turns Look into the body yaw + camera pitch.
            look.yaw = aim.yaw;
            look.pitch = aim.pitch;

            if cooldown.0 > 0 {
                cooldown.0 -= 1;
            }
        }
    }
}

fn move_bullets(world: &mut World) {
    let dt = SECONDS_PER_TICK;
    let tick = world.resource::<CurrentTick>().0;

    // Collect this tick's shooting requests.
    let mut shoot_requests: Vec<u128> = Vec::new();
    {
        let input_queue = world.resource::<InputQueue<PlayerInput>>();
        if let Some(tick_inputs) = input_queue.at_tick(tick) {
            for (uuid, input) in tick_inputs {
                if input.shooting {
                    shoot_requests.push(*uuid);
                }
            }
        }
    }

    // Move existing bullets and cull them against the arena and pillars.
    let mut bullets_to_despawn = Vec::new();
    {
        let mut query =
            world.query::<(Entity, &mut Position, &Aim, &EntityKind, &TickTrackedEntity)>();
        for (entity, mut pos, aim, kind, _) in query.iter_mut(world) {
            if *kind != EntityKind::Bullet {
                continue;
            }
            pos.0 += aim.forward() * BULLET_SPEED * dt;

            let out_of_arena = pos.0.x.abs() > ARENA_HALF + 5.0
                || pos.0.z.abs() > ARENA_HALF + 5.0
                || pos.0.y < 0.0
                || pos.0.y > 20.0;
            if out_of_arena || hits_pillar(pos.0) {
                bullets_to_despawn.push(entity);
            }
        }
    }
    for entity in bullets_to_despawn {
        world.despawn(entity);
    }

    // Gather player state for spawning bullets.
    let mut players: Vec<(u128, Vec3, Aim, u64)> = Vec::new();
    {
        let mut query = world.query::<(&PlayerUuid, &Position, &Aim, &ShootCooldown, &EntityKind)>();
        for (uuid, pos, aim, cooldown, kind) in query.iter(world) {
            if *kind == EntityKind::Player {
                players.push((uuid.0, pos.0, *aim, cooldown.0));
            }
        }
    }

    let mut spawns: Vec<(u128, TickTrackedEntity, Vec3, Aim)> = Vec::new();
    {
        let mut counter = world.resource_mut::<TickTrackedEntityCounter>();
        for uuid in &shoot_requests {
            if let Some((_, pos, aim, cooldown)) = players.iter().find(|(u, ..)| u == uuid) {
                if *cooldown > 0 {
                    continue;
                }
                let muzzle = *pos + Vec3::Y * EYE_HEIGHT + aim.forward() * (PLAYER_HIT_RADIUS + 0.2);
                spawns.push((*uuid, counter.next(), muzzle, *aim));
            }
        }
    }

    for (owner_uuid, tracked_id, bullet_pos, aim) in spawns {
        world.spawn((
            tracked_id,
            EntityKind::Bullet,
            Position(bullet_pos),
            aim,
            PlayerUuid(owner_uuid),
        ));

        // Reset the shooter's cooldown.
        let mut query = world.query::<(&PlayerUuid, &mut ShootCooldown, &EntityKind)>();
        for (uuid, mut cooldown, kind) in query.iter_mut(world) {
            if *kind == EntityKind::Player && uuid.0 == owner_uuid {
                cooldown.0 = SHOOT_COOLDOWN_TICKS;
            }
        }
    }
}

/// AABB test against every pillar (they span `y ∈ [0, 2*PILLAR_HALF.y]`).
fn hits_pillar(p: Vec3) -> bool {
    if p.y < 0.0 || p.y > PILLAR_HALF.y * 2.0 {
        return false;
    }
    PILLARS.iter().any(|c| {
        (p.x - c.x).abs() < PILLAR_HALF.x + BULLET_HIT_RADIUS
            && (p.z - c.y).abs() < PILLAR_HALF.z + BULLET_HIT_RADIUS
    })
}

fn bullet_collision(world: &mut World) {
    let mut bullets: Vec<(Entity, Vec3, u128)> = Vec::new();
    {
        let mut query = world.query::<(Entity, &Position, &PlayerUuid, &EntityKind)>();
        for (entity, pos, uuid, kind) in query.iter(world) {
            if *kind == EntityKind::Bullet {
                bullets.push((entity, pos.0, uuid.0));
            }
        }
    }

    let mut players: Vec<(Entity, Vec3, u128, Vec3)> = Vec::new();
    {
        let mut query = world.query::<(Entity, &Position, &PlayerUuid, &SpawnPoint, &EntityKind)>();
        for (entity, pos, uuid, spawn, kind) in query.iter(world) {
            if *kind == EntityKind::Player {
                players.push((entity, pos.0, uuid.0, spawn.0));
            }
        }
    }

    let mut bullets_to_despawn = Vec::new();
    let mut players_to_respawn: Vec<(Entity, Vec3)> = Vec::new();

    for (bullet_entity, bullet_pos, bullet_owner) in &bullets {
        for (player_entity, player_pos, player_uuid, spawn_point) in &players {
            if *bullet_owner == *player_uuid {
                continue;
            }
            // Compare against the player's vertical centre (body origin + eye).
            let center = *player_pos + Vec3::Y * EYE_HEIGHT;
            if bullet_pos.distance(center) < PLAYER_HIT_RADIUS + BULLET_HIT_RADIUS {
                bullets_to_despawn.push(*bullet_entity);
                players_to_respawn.push((*player_entity, *spawn_point));
            }
        }
    }

    for entity in bullets_to_despawn {
        world.despawn(entity);
    }

    for (entity, spawn_pos) in players_to_respawn {
        if let Some(mut pos) = world.entity_mut(entity).get_mut::<Position>() {
            pos.0 = spawn_pos;
        }
        if let Some(mut vel) = world.entity_mut(entity).get_mut::<LinearVelocity>() {
            vel.0 = Vec3::ZERO;
        }
    }
}

// --- Entity lifecycle observers ---

fn on_entity_spawned(
    trigger: On<Add, TickTrackedEntity>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    query: Query<(&EntityKind, &PlayerUuid)>,
    local_client: Option<Res<LocalClientPlayer>>,
    local_server: Option<Res<LocalServerPlayer>>,
) {
    let entity = trigger.entity;
    let Ok((kind, uuid)) = query.get(entity) else {
        return;
    };

    match kind {
        EntityKind::Player => {
            commands.entity(entity).insert((
                // Controller bundle + Look are needed on every peer so clients can
                // predict the body locally (snapshots only carry networked state).
                character_controller_bundle(),
                Look::default(),
                Mesh3d(meshes.add(Cuboid::new(0.4, 1.3, 0.4))),
                MeshMaterial3d(materials.add(unlit(Color::srgb(0.2, 0.7, 0.35)))),
                Transform::default(),
            ));

            // Override the controller's speed (default is 1.0). Applied here, on
            // every peer, so it stays consistent under rollback.
            commands.entity(entity).insert(CharacterController3d {
                move_speed: PLAYER_MOVE_SPEED,
                ..default()
            });

            let my_uuid = local_client
                .as_ref()
                .map(|p| p.0)
                .or_else(|| local_server.as_ref().map(|p| p.0));
            if my_uuid == Some(uuid.0) {
                // The local player owns the first-person camera (an eye-height child).
                commands.entity(entity).with_children(|parent| {
                    parent.spawn((
                        FpsCamera::new(0.1),
                        Transform::from_xyz(0.0, EYE_HEIGHT, 0.0),
                    ));
                });
            }
        }
        EntityKind::Bullet => {
            commands.entity(entity).insert((
                Mesh3d(meshes.add(Cuboid::from_length(0.2))),
                MeshMaterial3d(materials.add(unlit(Color::srgb(1.0, 0.85, 0.2)))),
                Transform::default(),
            ));
        }
    }
}

/// Copy simulated `Position` onto the render `Transform` for bullets. Player
/// bodies and the camera are oriented by avian (`Rotation`) and elan's
/// `apply_look` inside the sim, so we deliberately do not touch their transforms
/// here (writing them would fight avian's transform sync during rollback).
fn sync_visuals(
    mut bullets: Query<(&Position, &EntityKind, &mut Transform), With<TickTrackedEntity>>,
) {
    for (pos, kind, mut transform) in bullets.iter_mut() {
        if *kind == EntityKind::Bullet {
            transform.translation = pos.0;
        }
    }
}

/// Swap the free menu camera for the first-person camera and back, so exactly one
/// camera is active at a time.
fn manage_cameras(
    mut commands: Commands,
    fps_cameras: Query<(), With<FpsCamera>>,
    menu_cameras: Query<Entity, With<MenuCamera>>,
) {
    let has_fps = !fps_cameras.is_empty();
    let has_menu = !menu_cameras.is_empty();

    if has_fps {
        // First-person camera is live; drop the menu camera.
        for entity in menu_cameras.iter() {
            commands.entity(entity).despawn();
        }
    } else if !has_menu {
        // No camera at all (startup or after leaving a lobby): restore the menu one.
        commands.spawn((
            MenuCamera,
            Camera3d::default(),
            Transform::from_xyz(0.0, 35.0, 45.0).looking_at(Vec3::ZERO, Vec3::Y),
        ));
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

    let status = if ticks_paused.is_some() { "PAUSED" } else { "PLAYING" };
    **text = format!(
        "[{}] Tick: {} [{}] | Players: {} | WASD: Move | Mouse: Look | Space: Jump | LMB: Shoot | Esc: Leave",
        role, tick.0, status, player_count
    );
}
