//! # bevy_elan
//!
//! A small, force-based **first-person character controller** and **FPS look
//! camera** for [Bevy](https://bevyengine.org), built on the
//! [Avian](https://github.com/Jondolf/avian) physics engine.
//!
//! The character is a floating capsule: it hovers above the ground with a spring
//! force (raycast-based), moves and jumps via forces, and locks its rotation so
//! it stays upright. The camera does the mouse look — yaw on the body, pitch on
//! the camera — when parented to the body.
//!
//! ```no_run
//! use bevy::prelude::*;
//! use bevy_elan::prelude::*;
//!
//! fn spawn_player(mut commands: Commands) {
//!     commands.spawn((
//!         character_controller_bundle(),
//!         Transform::from_xyz(0.0, 2.0, 0.0),
//!         children![(
//!             Transform::from_translation(Vec3::Y * 0.3),
//!             FpsCamera::new(0.1),
//!         )],
//!     ));
//! }
//! ```
//!
//! Add [`ElanPlugin`] (or the two sub-plugins individually) to your app.

pub mod character_controller;
pub mod fps_camera;
pub mod prelude;

use bevy::prelude::*;

/// Adds both the [`FpsCameraPlugin`](fps_camera::FpsCameraPlugin) and the
/// [`CharacterController3dPlugin`](character_controller::CharacterController3dPlugin).
pub struct ElanPlugin;

impl Plugin for ElanPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins((
            fps_camera::FpsCameraPlugin,
            character_controller::CharacterController3dPlugin::default(),
        ));
    }
}
