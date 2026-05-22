use avian2d::prelude::*;
use bevy::prelude::*;
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

#[derive(Component, Clone, Debug, Serialize, Deserialize, Default)]
struct AimAngle(f32);

#[derive(Component, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum EntityKind {
    Player,
    Bullet,
}

#[derive(Component, Clone, Debug, Serialize, Deserialize)]
struct SpawnPoint(Vec2);

#[derive(Component, Clone, Debug, Serialize, Deserialize, Default)]
struct ShootCooldown(u64);

/// Links a player's UUID to their game entity.
#[derive(Component, Clone, Debug, Serialize, Deserialize)]
struct PlayerUuid(u128);

#[derive(Component)]
struct UiText;

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
        .add_plugins(PhysicsPlugins::new(TickedSimulation).with_length_unit(1.0))
        .insert_resource(Gravity(Vec2::ZERO))
        .add_plugins(TickedServerPlugin::<PlayerInput>::new())
        .add_plugins(TickedClientPlugin::<PlayerInput>::new())
        .add_plugins(TickedNetworkingEnsemblePlugin::<PlayerInput>::new())
        // Register networked components (order must match on all peers)
        .register_networked_ticked_component::<Position>()
        .register_networked_ticked_component::<Rotation>()
        .register_networked_ticked_component::<LinearVelocity>()
        .register_networked_ticked_component::<AngularVelocity>()
        .register_networked_ticked_component::<AimAngle>()
        .register_networked_ticked_component::<EntityKind>()
        .register_networked_ticked_component::<SpawnPoint>()
        .register_networked_ticked_component::<ShootCooldown>()
        .register_networked_ticked_component::<PlayerUuid>()
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
                on_lobby_ready,
                server_spawn_players,
                capture_local_input,
                sync_visuals,
                update_ui,
            ),
        )
        // Simulation systems (run inside TickedSimulation)
        .add_systems(
            TickedSimulation,
            (apply_inputs, move_bullets, bullet_collision).chain(),
        )
        // React to networked entity lifecycle
        .add_observer(on_entity_spawned)
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

/// When lobby becomes ready, insert the appropriate server/client player resource.
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

        // Alternate spawn sides
        let spawn_x = if player_index % 2 == 0 {
            -ARENA_HALF_W * 0.6
        } else {
            ARENA_HALF_W * 0.6
        };
        let spawn_pos = Vec2::new(spawn_x, 0.0);
        let tracked_id = counter.next();

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
    windows: Query<&Window>,
    cameras: Query<(&Camera, &GlobalTransform)>,
    tick: Res<CurrentTick>,
    local_client: Option<Res<LocalClientPlayer>>,
    local_server: Option<Res<LocalServerPlayer>>,
    mut input_queue: ResMut<InputQueue<PlayerInput>>,
    players: Query<(&Position, &PlayerUuid)>,
) {
    // Determine our UUID
    let my_uuid = local_client
        .as_ref()
        .map(|p| p.0)
        .or_else(|| local_server.as_ref().map(|p| p.0));
    let Some(my_uuid) = my_uuid else { return };

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

    let input = PlayerInput {
        movement: [movement.x, movement.y],
        aim_angle,
        shooting,
    };

    // Write to input queue for the NEXT tick (current tick + 1, since advance hasn't happened yet)
    input_queue.insert(tick.0 + 1, my_uuid, input);
}

// --- Simulation systems (run in TickedSimulation) ---

fn apply_inputs(
    tick: Res<CurrentTick>,
    input_queue: Res<InputQueue<PlayerInput>>,
    mut players: Query<(
        &mut LinearVelocity,
        &mut AimAngle,
        &mut ShootCooldown,
        &PlayerUuid,
        &EntityKind,
    )>,
) {
    let Some(tick_inputs) = input_queue.at_tick(tick.0) else {
        return;
    };

    for (mut vel, mut aim, mut cooldown, uuid, kind) in players.iter_mut() {
        if *kind != EntityKind::Player {
            continue;
        }
        if let Some(input) = tick_inputs.get(&uuid.0) {
            let movement = Vec2::new(input.movement[0], input.movement[1]);
            vel.0 += movement * MOVE_ACCEL * SECONDS_PER_TICK;
            aim.0 = input.aim_angle;

            if cooldown.0 > 0 {
                cooldown.0 -= 1;
            }
        }
    }
}

fn move_bullets(world: &mut World) {
    let dt = SECONDS_PER_TICK;
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
        world.despawn(entity);
    }

    // Spawn new bullets
    let mut player_data: Vec<(u128, Vec2, u64)> = Vec::new();
    {
        let mut query = world.query::<(&PlayerUuid, &Position, &ShootCooldown, &EntityKind)>();
        for (uuid, pos, cooldown, kind) in query.iter(world) {
            if *kind == EntityKind::Player {
                player_data.push((uuid.0, pos.0, cooldown.0));
            }
        }
    }

    let mut counter = world.resource_mut::<TickTrackedEntityCounter>();
    let mut spawns = Vec::new();

    for (uuid, aim_angle) in &shoot_requests {
        if let Some((_, pos, cooldown)) = player_data.iter().find(|(u, _, _)| u == uuid) {
            if *cooldown > 0 {
                continue;
            }
            let tracked_id = counter.next();
            let dir = Vec2::new(aim_angle.cos(), aim_angle.sin());
            let bullet_pos = *pos + dir * (PLAYER_RADIUS + BULLET_RADIUS + 2.0);
            spawns.push((*uuid, tracked_id, bullet_pos, *aim_angle));
        }
    }

    for (owner_uuid, tracked_id, bullet_pos, aim_angle) in spawns {
        world.spawn((
            tracked_id,
            EntityKind::Bullet,
            Position(bullet_pos),
            AimAngle(aim_angle),
            PlayerUuid(owner_uuid),
        ));

        // Reset cooldown on the player
        let mut query = world.query::<(&PlayerUuid, &mut ShootCooldown, &EntityKind)>();
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
        let mut query = world.query::<(Entity, &Position, &PlayerUuid, &EntityKind)>();
        for (entity, pos, uuid, kind) in query.iter(world) {
            if *kind == EntityKind::Bullet {
                bullets.push((entity, pos.0, uuid.0));
            }
        }
    }

    // Collect player positions
    let mut players: Vec<(Entity, Vec2, u128, Vec2)> = Vec::new();
    {
        let mut query = world.query::<(Entity, &Position, &PlayerUuid, &SpawnPoint, &EntityKind)>();
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
        world.despawn(entity);
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

// --- Entity lifecycle observers ---

fn on_entity_spawned(
    trigger: On<Add, TickTrackedEntity>,
    mut commands: Commands,
    query: Query<(&EntityKind, &Position)>,
) {
    let entity = trigger.entity;
    let Ok((kind, pos)) = query.get(entity) else {
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
                // Physics components needed for client-side prediction
                RigidBody::Dynamic,
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
            ));
        }
    }
}

fn sync_visuals(
    mut tracked: Query<
        (
            &Position,
            &AimAngle,
            &EntityKind,
            &mut Transform,
            Option<&Children>,
        ),
        With<TickTrackedEntity>,
    >,
    mut child_transforms: Query<&mut Transform, Without<TickTrackedEntity>>,
) {
    for (pos, aim, kind, mut transform, children) in tracked.iter_mut() {
        transform.translation = pos.0.extend(0.0);
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
        "[{}] Tick: {} [{}] | Players: {} | WASD: Move | Mouse: Aim | LMB: Shoot | Esc: Leave",
        role, tick.0, status, player_count
    );
}
