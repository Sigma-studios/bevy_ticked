//! A first-person look camera driven by mouse motion.

use bevy::input::mouse::MouseMotion;
use bevy::prelude::*;
use bevy::window::{CursorGrabMode, CursorOptions, PrimaryWindow};

pub struct FpsCameraPlugin;

impl Plugin for FpsCameraPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, (update_cursor, handle_fps_cameras));
    }
}

/// Grabs/hides the cursor for an active [`FpsCamera`] **without** the mouse-look
/// integration. Use this in driven mode, where orientation comes from [`Look`]
/// rather than [`handle_fps_cameras`], but you still want the cursor captured.
pub struct CursorGrabPlugin;

impl Plugin for CursorGrabPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, update_cursor);
    }
}

/// A first-person look camera. Attach it to a `Camera3d`.
///
/// If the camera is a child of a body entity (e.g. the character controller),
/// yaw rotates the parent and pitch rotates the camera, so the body turns with
/// the view. Otherwise both axes are applied to the camera itself.
///
/// Only an **active** camera (`Camera::is_active`) reads input and grabs the
/// cursor, so several controllers can coexist and be toggled by activating one
/// camera at a time.
#[derive(Component)]
#[require(Camera3d)]
pub struct FpsCamera {
    /// Mouse-look sensitivity (radians per pixel per second).
    pub sensitivity: f32,
}

impl FpsCamera {
    pub fn new(sensitivity: f32) -> Self {
        Self { sensitivity }
    }
}

/// When present on an active [`FpsCamera`], keeps the cursor visible and
/// unlocked instead of grabbing it.
#[derive(Component, Debug)]
pub struct ShowCursor;

/// Explicit look orientation stored on a controller **body**.
///
/// This is the driven-mode counterpart to [`handle_fps_cameras`]: instead of the
/// camera integrating raw mouse motion into transforms every frame, the caller
/// keeps absolute `yaw`/`pitch` here (e.g. from a networked input) and
/// [`apply_look`] turns it into the body/camera transforms deterministically. A
/// body without `Look` is untouched, so default-mode cameras are unaffected.
#[derive(Component, Default, Debug, Clone, Copy)]
pub struct Look {
    /// Yaw about the world Y axis, applied to the body. Also the movement basis.
    pub yaw: f32,
    /// Pitch about the local X axis, applied to the child camera (view only).
    pub pitch: f32,
}

/// Applies each body's [`Look`] to its own transform (yaw) and to its child
/// [`FpsCamera`]'s transform (pitch). Runnable in any schedule.
pub fn apply_look(
    bodies: Query<(Entity, &Look, &Children)>,
    is_camera: Query<(), With<FpsCamera>>,
    mut transforms: Query<&mut Transform>,
) {
    for (body, look, children) in &bodies {
        if let Ok(mut body_transform) = transforms.get_mut(body) {
            body_transform.rotation = Quat::from_rotation_y(look.yaw);
        }
        for &child in children {
            if is_camera.get(child).is_ok() {
                if let Ok(mut cam_transform) = transforms.get_mut(child) {
                    cam_transform.rotation = Quat::from_rotation_x(look.pitch);
                }
            }
        }
    }
}

fn update_cursor(
    cameras: Query<(&Camera, Option<&ShowCursor>), With<FpsCamera>>,
    mut windows: Query<&mut CursorOptions, With<PrimaryWindow>>,
) {
    let Ok(mut cursor) = windows.single_mut() else {
        return;
    };
    let Ok((camera, show_cursor)) = cameras.single() else {
        return;
    };
    // An inactive fps camera leaves the cursor alone so it can coexist with
    // other camera controllers.
    if !camera.is_active {
        return;
    }
    if show_cursor.is_none() {
        cursor.grab_mode = CursorGrabMode::Locked;
        cursor.visible = false;
    } else {
        cursor.grab_mode = CursorGrabMode::None;
        cursor.visible = true;
    }
}

fn handle_fps_cameras(
    cameras: Query<(Entity, &FpsCamera, &Camera, Option<&ChildOf>)>,
    mut motion: MessageReader<MouseMotion>,
    mut transforms: Query<&mut Transform>,
    time: Res<Time>,
) {
    // Sum motion once so it isn't drained by the first camera in the loop.
    let delta: Vec2 = motion.read().map(|m| m.delta).sum();
    if delta == Vec2::ZERO {
        return;
    }

    for (entity, fps_camera, camera, maybe_parent) in &cameras {
        if !camera.is_active {
            continue;
        }
        let rotation = -delta * fps_camera.sensitivity * time.delta_secs();
        if let Some(parent) = maybe_parent {
            let Ok([mut transform, mut parent_transform]) =
                transforms.get_many_mut([entity, parent.parent()])
            else {
                continue;
            };
            transform.rotate_axis(Dir3::X, rotation.y);
            parent_transform.rotate_axis(Dir3::Y, rotation.x);
        } else if let Ok(mut transform) = transforms.get_mut(entity) {
            let right = transform.right();
            transform.rotate_axis(right, rotation.y);
            transform.rotate_axis(Dir3::Y, rotation.x);
        }
    }
}
