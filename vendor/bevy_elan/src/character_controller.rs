//! A floating-capsule, force-based 3D character controller for Avian.
//!
//! The controller is **dual-mode**:
//!
//! * **Default** — [`CharacterController3dPlugin::default`] wires everything up
//!   for a normal game: it reads the keyboard itself, drives its timers from the
//!   fixed clock, and runs in `FixedPreUpdate`/`FixedUpdate`. No extra setup.
//! * **Driven** — [`CharacterController3dPlugin::in_schedule`] runs every system
//!   chained inside a caller-provided schedule and does **not** read the keyboard
//!   or the clock. The caller feeds [`ControllerInput`] and [`ControllerTime`]
//!   each step. This makes the controller deterministic and re-runnable, which is
//!   what a fixed-tick / rollback engine needs.

use avian3d::prelude::*;
use bevy::{
    ecs::schedule::{InternedScheduleLabel, ScheduleLabel},
    math::FloatPow,
    prelude::*,
};

use crate::fps_camera::apply_look;

/// The character controller systems, so callers can order against them.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ControllerSet;

/// Per-step clock the controller reads instead of [`Time`] directly.
///
/// In default mode a built-in system mirrors the fixed [`Time`] into this every
/// step, so behavior is unchanged. In driven mode the caller sets it (e.g. from
/// a tick counter) so the timers are deterministic under rollback.
#[derive(Resource, Default, Debug, Clone, Copy)]
pub struct ControllerTime {
    /// Seconds elapsed this step.
    pub delta: f32,
    /// Total seconds elapsed. Timers (coyote/cooldown) are compared against this.
    pub elapsed: f32,
}

/// The movement intent the controller acts on.
///
/// In default mode a built-in system fills this from the keyboard. In driven
/// mode the caller writes it (e.g. from a networked input).
#[derive(Component, Default, Debug, Clone, Copy)]
pub struct ControllerInput {
    /// Local-space horizontal move intent: `x` = strafe (right positive),
    /// `y` = forward (forward positive). Need not be normalized.
    pub move_dir: Vec2,
    /// Whether a jump is being requested.
    pub jump: bool,
}

pub struct CharacterController3dPlugin {
    /// If `Some`, all systems run chained in this schedule (driven mode).
    /// If `None`, the default `FixedPreUpdate`/`FixedUpdate` split is used.
    schedule: Option<InternedScheduleLabel>,
    /// Add the built-in keyboard reader that fills [`ControllerInput`].
    gather_keyboard: bool,
    /// Add the built-in system that mirrors [`Time`] into [`ControllerTime`].
    sync_time: bool,
}

impl Default for CharacterController3dPlugin {
    fn default() -> Self {
        Self {
            schedule: None,
            gather_keyboard: true,
            sync_time: true,
        }
    }
}

impl CharacterController3dPlugin {
    /// Run every controller system chained inside `schedule` (driven mode).
    ///
    /// The keyboard reader and clock mirror are **off** by default here — the
    /// caller is expected to supply [`ControllerInput`] and [`ControllerTime`].
    /// Re-enable either with [`with_keyboard_input`](Self::with_keyboard_input)
    /// / [`with_time_sync`](Self::with_time_sync).
    pub fn in_schedule(schedule: impl ScheduleLabel) -> Self {
        Self {
            schedule: Some(schedule.intern()),
            gather_keyboard: false,
            sync_time: false,
        }
    }

    /// Toggle the built-in keyboard reader.
    pub fn with_keyboard_input(mut self, enabled: bool) -> Self {
        self.gather_keyboard = enabled;
        self
    }

    /// Toggle the built-in [`Time`] → [`ControllerTime`] mirror.
    pub fn with_time_sync(mut self, enabled: bool) -> Self {
        self.sync_time = enabled;
        self
    }
}

impl Plugin for CharacterController3dPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ControllerTime>();

        if self.sync_time {
            // FixedFirst so `Time` is the fixed clock, matching the pre-refactor
            // behavior that read `Time` inside the fixed schedules.
            app.add_systems(FixedFirst, sync_controller_time);
        }

        match self.schedule {
            None => {
                if self.gather_keyboard {
                    app.add_systems(FixedPreUpdate, gather_keyboard_input);
                }
                app.add_systems(
                    FixedPreUpdate,
                    (handle_grounded, handle_friction)
                        .chain()
                        .in_set(ControllerSet),
                )
                .add_systems(
                    FixedUpdate,
                    (handle_hover, handle_movement, handle_jump)
                        .chain()
                        .in_set(ControllerSet),
                );
            }
            Some(label) => {
                if self.gather_keyboard {
                    app.add_systems(label, gather_keyboard_input.before(ControllerSet));
                }
                // `apply_look` orients bodies that carry a `Look` before movement
                // reads their rotation; bodies without `Look` are skipped, so it
                // is harmless when the caller drives orientation another way.
                app.add_systems(
                    label,
                    (
                        apply_look,
                        handle_grounded,
                        handle_friction,
                        handle_hover,
                        handle_movement,
                        handle_jump,
                    )
                        .chain()
                        .in_set(ControllerSet),
                );
            }
        }
    }
}

/// A floating-capsule character controller. Spawn it with
/// [`character_controller_bundle`], which supplies the rigid body, collider,
/// ground ray and friction/CCD settings.
#[derive(Component)]
#[require(ControllerInput)]
pub struct CharacterController3d {
    /// Ride height: the body hovers this far above the ground.
    pub hover_height: f32,
    /// Horizontal movement strength.
    pub move_speed: f32,
    /// Upward launch velocity (m/s) applied on jump. Set as a velocity rather
    /// than a force so jump height doesn't depend on the body's mass.
    pub jump_velocity: f32,
    /// Multiplier on the velocity-based drag that slows the body down.
    pub drag_multiplier: f32,
    /// Grace period after leaving the ground during which a jump is still allowed.
    pub coyote_time: f32,
    /// Minimum time between jumps.
    pub jump_cooldown: f32,
    /// When `false`, movement and jump input is ignored (the body still hovers
    /// and settles). Useful for toggling control off, e.g. for a free camera.
    pub enabled: bool,
}

impl Default for CharacterController3d {
    fn default() -> Self {
        Self {
            hover_height: 1.0,
            move_speed: 1.0,
            jump_velocity: 5.0,
            drag_multiplier: 1.0,
            coyote_time: 0.1,
            jump_cooldown: 0.5,
            enabled: true,
        }
    }
}

#[derive(Component)]
pub struct Grounded;

/// Wall-clock (or tick-clock) time the body was last grounded. Persists while
/// airborne, so it is snapshotted when the `serialize` feature is on for
/// deterministic coyote-time under rollback.
#[derive(Component, Clone, Copy)]
#[cfg_attr(feature = "serialize", derive(serde::Serialize, serde::Deserialize))]
pub struct LastGrounded(pub f32);

/// Time of the last jump. Persists across the jump cooldown, so it is
/// snapshotted when the `serialize` feature is on for deterministic jumps
/// under rollback.
#[derive(Component, Clone, Copy)]
#[cfg_attr(feature = "serialize", derive(serde::Serialize, serde::Deserialize))]
pub struct LastJump(pub f32);

#[derive(Component)]
pub struct DistanceToGround(pub f32);

/// The components that make up a character controller body.
pub fn character_controller_bundle() -> (
    RigidBody,
    CharacterController3d,
    Collider,
    RayCaster,
    GravityScale,
    LockedAxes,
    Friction,
    SweptCcd,
) {
    let capsule_height = 1.0;
    (
        RigidBody::Dynamic,
        CharacterController3d::default(),
        Collider::capsule(0.15, capsule_height),
        RayCaster::new(
            Vec3::new(0.0, -capsule_height / 2.0, 0.0),
            Dir3::new(-Vec3::Y).unwrap(),
        ),
        GravityScale(2.0),
        LockedAxes::ROTATION_LOCKED,
        Friction::ZERO.with_combine_rule(CoefficientCombine::Min),
        SweptCcd::default(),
    )
}

/// Default-mode clock mirror: copy the (fixed) [`Time`] into [`ControllerTime`].
fn sync_controller_time(time: Res<Time>, mut controller_time: ResMut<ControllerTime>) {
    controller_time.delta = time.delta_secs();
    controller_time.elapsed = time.elapsed_secs();
}

/// Default-mode input reader: fill [`ControllerInput`] from the keyboard.
fn gather_keyboard_input(
    keyboard: Res<ButtonInput<KeyCode>>,
    mut controllers: Query<&mut ControllerInput>,
) {
    let mut move_dir = Vec2::ZERO;
    if keyboard.pressed(KeyCode::KeyW) {
        move_dir.y += 1.0;
    }
    if keyboard.pressed(KeyCode::KeyS) {
        move_dir.y -= 1.0;
    }
    if keyboard.pressed(KeyCode::KeyD) {
        move_dir.x += 1.0;
    }
    if keyboard.pressed(KeyCode::KeyA) {
        move_dir.x -= 1.0;
    }
    let jump = keyboard.pressed(KeyCode::Space);

    for mut input in controllers.iter_mut() {
        input.move_dir = move_dir;
        input.jump = jump;
    }
}

fn handle_grounded(
    time: Res<ControllerTime>,
    mut commands: Commands,
    controllers: Query<(Entity, &RayHits, &CharacterController3d)>,
) {
    for (entity, hits, controller) in controllers.iter() {
        let hit = hits.iter_sorted().next();
        let distance = hit.map(|hit| hit.distance).unwrap_or(f32::INFINITY);
        commands.entity(entity).insert(DistanceToGround(distance));
        if distance < controller.hover_height {
            commands.entity(entity).insert(Grounded);
            commands.entity(entity).insert(LastGrounded(time.elapsed));
        } else {
            commands.entity(entity).remove::<Grounded>();
        }
    }
}

fn handle_hover(
    time: Res<ControllerTime>,
    mut controllers: Query<(
        &CharacterController3d,
        Forces,
        &DistanceToGround,
        Option<&LastJump>,
    )>,
) {
    for (controller, mut forces, distance_to_ground, last_jump) in controllers.iter_mut() {
        // Suppress the ground spring briefly after a jump so it doesn't cancel
        // the launch velocity before the body clears hover range.
        if last_jump.is_some_and(|last_jump| time.elapsed - last_jump.0 < 0.2) {
            continue;
        }
        let distance = distance_to_ground.0;
        if distance >= controller.hover_height {
            continue;
        }

        let diff = controller.hover_height - distance;
        let vertical_velocity = forces.linear_velocity().y;
        let bounce_force = Vec3::Y * diff * 100.0 * 60.0;
        let damp_force = Vec3::Y * -(vertical_velocity) * 10.0 * 60.0;
        let total_force = (damp_force + bounce_force) * 0.005;
        forces.apply_force(total_force);
    }
}

/// Moves relative to where the body faces: its avian `Rotation`, which is the simulation's —
/// not its `Transform`, which belongs to whoever draws it and, under a rollback stack that
/// blends it between ticks, holds a picture rather than a state.
fn handle_movement(
    mut controllers: Query<(
        &Rotation,
        Forces,
        &CharacterController3d,
        &ControllerInput,
        Option<&Grounded>,
    )>,
) {
    for (rotation, mut forces, controller, input, grounded) in controllers.iter_mut() {
        if !controller.enabled {
            continue;
        }
        // Forward is -Z in local space, matching Bevy's camera convention.
        let mut dir = Vec3::new(input.move_dir.x, 0.0, -input.move_dir.y);
        if dir != Vec3::ZERO {
            dir = dir.normalize();
        } else {
            continue;
        }
        let grounded_mult = if grounded.is_some() { 1.0 } else { 0.2 };
        let force = rotation.0 * dir * grounded_mult * controller.move_speed * 7.0;
        if force != Vec3::ZERO {
            forces.apply_force(force);
        }
    }
}

fn handle_jump(
    time: Res<ControllerTime>,
    mut commands: Commands,
    mut controllers: Query<(
        Entity,
        Forces,
        &CharacterController3d,
        &ControllerInput,
        &LastGrounded,
        Option<&LastJump>,
    )>,
) {
    for (entity, mut forces, controller, input, last_grounded, last_jump) in controllers.iter_mut()
    {
        if !controller.enabled {
            continue;
        }
        if time.elapsed - last_grounded.0 > controller.coyote_time {
            continue;
        }
        if last_jump.is_some_and(|last_jump| time.elapsed - last_jump.0 < controller.jump_cooldown)
        {
            continue;
        }
        if input.jump {
            // Set the launch velocity directly: a fixed force would send a light
            // body flying, since acceleration is force / mass.
            forces.linear_velocity_mut().y = controller.jump_velocity;
            commands.entity(entity).insert(LastJump(time.elapsed));
        }
    }
}

fn handle_friction(mut controllers: Query<(&CharacterController3d, Option<&Grounded>, Forces)>) {
    for (controller, grounded, mut forces) in controllers.iter_mut() {
        let velocity = forces.linear_velocity().with_y(0.0);
        let speed = velocity.length();
        if speed == 0.0 {
            continue;
        }
        let dir = -velocity.normalize();
        let grounded_mult = if grounded.is_some() { 1.0 } else { 0.05 };
        let fixed_friction_force = dir * if grounded.is_some() { 0.3 } else { 0.0 };
        let friction = speed.squared();

        let friction_force =
            friction * dir * grounded_mult * controller.drag_multiplier + fixed_friction_force;
        forces.apply_force(friction_force);
    }
}
