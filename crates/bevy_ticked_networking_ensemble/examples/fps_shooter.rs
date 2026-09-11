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
use bevy_ticked_avian::avian3d::TickedAvianPlugin;
use bevy_ticked_networking::prelude::*;
use bevy_ticked_networking_ensemble::local_session::{self, TickedLocalSessionPlugin};
use bevy_ticked_networking_ensemble::{
    SpawnerSlots, TickedEnsembleSessionPlugin, TickedNetworkingEnsemblePlugin,
};
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
#[derive(Component, PartialEq, Clone, Copy, Debug, Serialize, Deserialize, Default)]
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

#[derive(Component, PartialEq, Clone, Debug, Serialize, Deserialize)]
struct SpawnPoint(Vec3);

#[derive(Component, PartialEq, Clone, Debug, Serialize, Deserialize, Default)]
struct ShootCooldown(u64);

/// The slot the player's peer mints ids under. On the body, networked, so every peer spawns
/// this player's bullets under the same ids: the shooter predicts the bullet, the host mints
/// the same id from the relayed input and confirms it.
#[derive(Component, PartialEq, Clone, Copy, Debug, Serialize, Deserialize)]
struct PlayerSlot(u8);

// --- Local-only marker components ---

#[derive(Component)]
struct UiText;

/// A free camera used only in the menu (before a player entity exists).
#[derive(Component)]
struct MenuCamera;

/// Marks the local player once its first-person camera has been attached, so
/// `attach_local_camera` doesn't add a second one.
#[derive(Component)]
struct CameraAttached;

/// Locally accumulated look, integrated from raw mouse motion each frame and sent
/// as the absolute `look` in `PlayerInput`.
#[derive(Resource, Default)]
struct LocalLook {
    yaw: f32,
    pitch: f32,
}

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
        // `Hz`, not the default `FixedUpdate`: a networked client steers its prediction lead
        // by running a couple of percent fast or slow, which only a clock this crate owns can
        // do. The networking plugins refuse to build on `FixedUpdate`.
        .add_plugins(TickedPlugin {
            source: TickSource::Hz(64.0),
            ..default()
        })
        // avian on the tick, replay-safe: the four body components registered under
        // `avian::*`, warm starting off, sleeping off, the contact graph rolled back, bodies
        // placed by `Position` (the plugin places a body spawned with a `Transform` once).
        .add_plugins(PhysicsPlugins::new(TickedSimulation))
        .add_plugins(TickedAvianPlugin::default())
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
        .init_resource::<LocalLook>()
        // Register networked components. The wire name is the type's identity on the
        // wire and must be the same on every peer; registration order does not matter.
        // `Owner` — whose body this is — is the stack's own and is registered by it.
        .register_networked_ticked_component::<Aim>("Aim")
        .register_networked_ticked_component::<EntityKind>("EntityKind")
        .register_networked_ticked_component::<SpawnPoint>("SpawnPoint")
        .register_networked_ticked_component::<ShootCooldown>("ShootCooldown")
        .register_networked_ticked_component::<PlayerSlot>("PlayerSlot")
        // elan's persistent jump timers: rolling these back keeps the local
        // player's predicted jump from mispredicting and snapping on correction.
        .register_networked_ticked_component::<LastGrounded>("elan::LastGrounded")
        .register_networked_ticked_component::<LastJump>("elan::LastJump")
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
                server_spawn_players,
                attach_local_camera,
                manage_cameras,
                sync_camera_pitch,
                update_ui,
            ),
        )
        // Simulation systems (run inside TickedSimulation).
        // set_controller_time + apply_inputs (which yaws the body via Rotation)
        // feed elan before its ControllerSet; the controller runs before avian's
        // Prepare so forces apply this step; bullets move after physics writeback.
        .add_systems(
            TickedSimulation,
            (set_controller_time, apply_inputs)
                .chain()
                .before(ControllerSet),
        )
        .configure_sets(
            TickedSimulation,
            ControllerSet.before(PhysicsSystems::Prepare),
        )
        .add_systems(
            TickedSimulation,
            (move_bullets, bullet_collision, sync_bullet_transforms)
                .chain()
                .after(PhysicsSystems::Writeback),
        )
        // React to networked entity lifecycle
        .add_observer(on_entity_spawned)
        .add_observer(on_replication_mode_changed)
        .run();
}

fn setup(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    // Ground visual: a large flat unlit cube whose top face sits at y = 0.
    let ground_size = ARENA_HALF * 2.0;
    commands.spawn((
        Mesh3d(meshes.add(Cuboid::new(ground_size, 1.0, ground_size))),
        MeshMaterial3d(materials.add(unlit(Color::srgb(0.12, 0.13, 0.16)))),
        Transform::from_xyz(0.0, -0.5, 0.0),
    ));
    // Ground collider: an infinite half-space with its surface at y = 0. A thin
    // box collider is unreliable here — the floating-capsule controller relies on
    // a downward raycast hitting the ground to hover, and both the ray and solid
    // contact can miss a thin box, letting the body fall straight through.
    commands.spawn((
        RigidBody::Static,
        Collider::half_space(Vec3::Y),
        Transform::from_xyz(0.0, 0.0, 0.0),
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
            Collider::cuboid(
                PILLAR_HALF.x * 2.0,
                PILLAR_HALF.y * 2.0,
                PILLAR_HALF.z * 2.0,
            ),
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

    // Crosshair: a small hollow white circle centred on screen.
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
                    width: Val::Px(14.0),
                    height: Val::Px(14.0),
                    border: UiRect::all(Val::Px(2.0)),
                    border_radius: BorderRadius::MAX,
                    ..default()
                },
                BorderColor::all(Color::srgba(1.0, 1.0, 1.0, 0.85)),
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

        // Spread players around a ring; they drop onto the floor and hover.
        let angle = player_index as f32 * std::f32::consts::TAU / 6.0;
        let spawn_pos = Vec3::new(
            angle.cos() * SPAWN_RING_RADIUS,
            2.0,
            angle.sin() * SPAWN_RING_RADIUS,
        );
        // Face roughly toward the arena centre.
        let yaw = angle + std::f32::consts::PI;
        let tracked_id = counter.next_authority();

        commands.spawn((
            tracked_id,
            EntityKind::Player,
            character_controller_bundle(),
            Position(spawn_pos),
            Aim { yaw, pitch: 0.0 },
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
    mut motion: MessageReader<MouseMotion>,
    local: Res<LocalPlayer>,
    mut local_look: ResMut<LocalLook>,
) -> Option<PlayerInput> {
    // No session, no body to drive.
    if local.0 == 0 {
        motion.clear();
        return None;
    }

    // Integrate raw mouse motion into an absolute look. Sent as an absolute value
    // so replaying the same input is deterministic.
    let delta: Vec2 = motion.read().map(|m| m.delta).sum();
    local_look.yaw -= delta.x * MOUSE_SENSITIVITY;
    local_look.pitch =
        (local_look.pitch - delta.y * MOUSE_SENSITIVITY).clamp(-MAX_PITCH, MAX_PITCH);

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

    Some(PlayerInput {
        move_dir: [move_dir.x, move_dir.y],
        look: [local_look.yaw, local_look.pitch],
        jump: keys.pressed(KeyCode::Space),
        shooting: mouse_buttons.pressed(MouseButton::Left),
    })
}

// --- Simulation systems (run in TickedSimulation) ---

/// Feed elan its clock from the tick counter so its timers are deterministic.
///
/// From `Time`, which inside a tick *is* the tick clock: `delta` is one tick and `elapsed` is
/// `tick * timestep`, on the first run and on every replay.
fn set_controller_time(time: Res<Time>, mut controller_time: ResMut<ControllerTime>) {
    controller_time.delta = time.delta_secs();
    controller_time.elapsed = time.elapsed_secs();
}

/// Apply this tick's inputs to each player this peer drives: feed elan's `ControllerInput`,
/// update `Aim`, yaw the physics body, and tick down the shoot cooldown.
///
/// This tick's input, or the newest one before it: a player whose input for this tick has not
/// arrived is still pressing what they last pressed, far more often than nothing. With
/// nothing, a predicted remote body stood still for every tick past the relayed inputs and
/// snapped forward at the snapshot.
///
/// Only the bodies this peer drives: on the host that is everyone; on a client it is the local
/// player's body and any the game marked `Predicted`. Everybody else's body is interpolated —
/// put at the authority's state after each snapshot — and takes no input here. That is the
/// whole of what used to be the "remote body with no input" workaround.
///
/// The body's yaw is written to avian's `Rotation` (not the `Transform`): avian
/// owns the body Transform, so we let it sync `Rotation -> Transform`, and elan's
/// `handle_movement` then moves relative to `transform.rotation` — i.e. relative
/// to where the player is looking. Writing the Transform directly would fight
/// avian's transform sync and clobber the rolled-back `Position`. Pitch is
/// view-only and handled per-frame on the camera child by `sync_camera_pitch`.
fn apply_inputs(
    tick: Res<CurrentTick>,
    input_queue: Res<InputQueue<PlayerInput>>,
    local_client: Option<Res<LocalClientPlayer>>,
    mut players: Query<(
        &mut ControllerInput,
        &mut Rotation,
        &mut Aim,
        &mut ShootCooldown,
        &Owner,
        &EntityKind,
        Option<&ReplicationMode>,
    )>,
) {
    let tick_inputs = input_queue.at_tick_or_last(tick.0);

    for (mut input, mut rotation, mut aim, mut cooldown, uuid, kind, mode) in players.iter_mut() {
        if *kind != EntityKind::Player || !drives(local_client.as_deref(), mode) {
            continue;
        }
        if let Some(player_input) = tick_inputs.get(&uuid.0) {
            input.move_dir = Vec2::new(player_input.move_dir[0], player_input.move_dir[1]);
            input.jump = player_input.jump;
            aim.yaw = player_input.look[0];
            aim.pitch = player_input.look[1];
            rotation.0 = Quat::from_rotation_y(aim.yaw);

            if cooldown.0 > 0 {
                cooldown.0 -= 1;
            }
        }
    }
}

fn move_bullets(world: &mut World) {
    let dt = world.resource::<Time>().delta_secs();
    let tick = world.resource::<CurrentTick>().0;

    // Bullets are spawned only on the host and replicated to clients via
    // snapshots. If every peer spawned its own bullets during the rolled-back
    // sim, the shared entity-id counter would diverge (the host has every
    // player's input, a client only its own), so a host bullet's id would
    // collide with a client's predicted bullet and never replicate. Clients
    // still ADVANCE existing bullets below for smooth motion between snapshots.

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
        world.entity_mut(entity).despawn_ticked();
    }

    // Gather player state for spawning bullets. Every peer spawns, under the shooter's slot,
    // in uuid order, so every peer mints the same ids.
    let mut players: Vec<(u128, Vec3, Aim, u64, u8)> = Vec::new();
    {
        let mut query = world.query::<(
            &Owner,
            &Position,
            &Aim,
            &ShootCooldown,
            &PlayerSlot,
            &EntityKind,
        )>();
        for (uuid, pos, aim, cooldown, slot, kind) in query.iter(world) {
            if *kind == EntityKind::Player {
                players.push((uuid.0, pos.0, *aim, cooldown.0, slot.0));
            }
        }
    }
    shoot_requests.sort_unstable();

    let mut spawns: Vec<(u128, u8, Vec3, Aim)> = Vec::new();
    for uuid in &shoot_requests {
        if let Some((_, pos, aim, cooldown, slot)) = players.iter().find(|(u, ..)| u == uuid) {
            if *cooldown > 0 {
                continue;
            }
            let muzzle = *pos + Vec3::Y * EYE_HEIGHT + aim.forward() * (PLAYER_HIT_RADIUS + 0.2);
            spawns.push((*uuid, *slot, muzzle, *aim));
        }
    }

    for (owner_uuid, slot, bullet_pos, aim) in spawns {
        world.spawn_tracked_by(
            SpawnerSlot(slot),
            (
                EntityKind::Bullet,
                Position(bullet_pos),
                aim,
                Owner(owner_uuid),
            ),
        );

        // Reset the shooter's cooldown.
        let mut query = world.query::<(&Owner, &mut ShootCooldown, &EntityKind)>();
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
    // Hits and respawns are authoritative on the host; clients receive the
    // resulting despawns/respawns via snapshots. This avoids a client
    // mispredicting a respawn off its snapshot-lagged copy of a bullet.
    if world.get_resource::<LocalServerPlayer>().is_none() {
        return;
    }

    let mut bullets: Vec<(Entity, Vec3, u128)> = Vec::new();
    {
        let mut query = world.query::<(Entity, &Position, &Owner, &EntityKind)>();
        for (entity, pos, uuid, kind) in query.iter(world) {
            if *kind == EntityKind::Bullet {
                bullets.push((entity, pos.0, uuid.0));
            }
        }
    }

    let mut players: Vec<(Entity, Vec3, u128, Vec3)> = Vec::new();
    {
        let mut query = world.query::<(Entity, &Position, &Owner, &SpawnPoint, &EntityKind)>();
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
        world.entity_mut(entity).despawn_ticked();
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
/// would fight that restore every tick, falling under gravity, colliding, integrating from a
/// velocity the host has since changed. Kinematic, it goes where it is put and coasts on the
/// velocity it was given until the next restore. The host owns its motion; this peer only
/// shows it.
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
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    query: Query<(&EntityKind, Option<&Owner>, Option<&ReplicationMode>)>,
    local_client: Option<Res<LocalClientPlayer>>,
) {
    let entity = trigger.entity;
    let Ok((kind, owner, mode)) = query.get(entity) else {
        return;
    };

    match kind {
        EntityKind::Player => {
            commands.entity(entity).insert((
                // The controller bundle is needed on every peer: the host and a predicting
                // client simulate the body, an interpolating client shows it. Its dynamic
                // body is overridden below for a body this peer only shows.
                // Rotation is present up front so apply_inputs can yaw the body
                // from the first tick.
                character_controller_bundle(),
                body_kind(local_client.as_deref(), owner, mode),
                Rotation::default(),
                Mesh3d(meshes.add(Cuboid::new(0.4, 1.3, 0.4))),
                MeshMaterial3d(materials.add(unlit(Color::srgb(0.2, 0.7, 0.35)))),
                Transform::default(),
                TickedInterpolation::default(),
                // The local player's body is exempt on its own: a correction to what you
                // are steering should be felt. The default caps at two metres; a correction
                // bigger than that is a respawn, and is meant to be seen.
                CorrectionSmoothing::default(),
            ));

            // Override the controller's speed (default is 1.0). Applied here, on
            // every peer, so it stays consistent under rollback.
            commands.entity(entity).insert(CharacterController3d {
                move_speed: PLAYER_MOVE_SPEED,
                ..default()
            });
            // The first-person camera is attached separately by
            // `attach_local_camera`, which retries every frame until the local
            // player resource is known (it may not be set yet when this fires).
        }
        EntityKind::Bullet => {
            commands.entity(entity).insert((
                Mesh3d(meshes.add(Cuboid::from_length(0.2))),
                MeshMaterial3d(materials.add(unlit(Color::srgb(1.0, 0.85, 0.2)))),
                Transform::default(),
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

/// Copy simulated `Position` onto the render `Transform` for bullets, inside the tick and
/// after they moved, so `TickedInterpolation` records one state per tick and blends between
/// them. Player bodies are positioned and oriented by avian's `Position`/`Rotation ->
/// Transform` sync inside the sim, so their transforms are deliberately not touched here.
fn sync_bullet_transforms(
    mut bullets: Query<(&Position, &EntityKind, &mut Transform), With<TickTrackedEntity>>,
) {
    for (pos, kind, mut transform) in bullets.iter_mut() {
        if *kind == EntityKind::Bullet {
            transform.translation = pos.0;
        }
    }
}

/// Pitch the local first-person camera from the locally-accumulated look. Yaw
/// comes from the parent body (avian `Rotation`), so the camera child only needs
/// the view pitch. Done per-frame for a smooth view, independent of tick rate.
fn sync_camera_pitch(
    local_look: Res<LocalLook>,
    mut cameras: Query<&mut Transform, With<FpsCamera>>,
) {
    for mut transform in cameras.iter_mut() {
        transform.rotation = Quat::from_rotation_x(local_look.pitch);
    }
}

/// Attach the first-person camera to the local player. Runs every frame and
/// retries until the local-player resource exists and the local body has spawned,
/// so a joiner whose entity arrives before its `LocalClientPlayer` is set still
/// gets a camera (instead of being stuck on the menu view). The `CameraAttached`
/// marker makes it idempotent.
fn attach_local_camera(
    mut commands: Commands,
    local_client: Option<Res<LocalClientPlayer>>,
    local_server: Option<Res<LocalServerPlayer>>,
    players: Query<(Entity, &Owner, &EntityKind), Without<CameraAttached>>,
) {
    let Some(my_uuid) = local_client
        .as_ref()
        .map(|p| p.0)
        .or_else(|| local_server.as_ref().map(|p| p.0))
    else {
        return;
    };
    for (entity, uuid, kind) in &players {
        if *kind == EntityKind::Player && uuid.0 == my_uuid {
            commands
                .entity(entity)
                .insert(CameraAttached)
                .with_children(|parent| {
                    parent.spawn((
                        FpsCamera::new(0.1),
                        Transform::from_xyz(0.0, EYE_HEIGHT, 0.0),
                    ));
                });
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
        "[{}] Tick: {} [{}] | Players: {} | WASD: Move | Mouse: Look | Space: Jump | LMB: Shoot | Esc: Leave",
        role, tick.0, status, player_count
    );
}
