//! avian under the tick never reads or writes `Transform`: whatever is done to one between ticks,
//! the bodies move exactly as they would have if nobody had touched it.

use ::avian3d::math::{ToF32Precision, ToRealPrecision};
use ::avian3d::prelude::*;
use bevy::prelude::*;
use bevy_ticked::checksum::WorldHash;
use bevy_ticked::tracked_entity::TickTrackedEntity;
use bevy_ticked_avian::avian3d::TickedAvianPlugin;
use bevy_ticked_testing::fixtures::avian::{self, AvianHash, spawn_stack};
use bevy_ticked_testing::prelude::*;

const BOXES: usize = 6;
const FRAMES: usize = 240;

/// What a renderer, a camera or a careless system might do to a body's transform between
/// ticks: move it, turn it, scale it — a different garbage value every frame.
fn scramble(mut frame: Local<u32>, mut bodies: Query<&mut Transform, With<TickTrackedEntity>>) {
    *frame += 1;
    for (i, mut transform) in bodies.iter_mut().enumerate() {
        let k = (*frame * 31 + i as u32 * 17) as f32;
        transform.translation = Vec3::new(k.sin() * 50.0, k.cos() * 50.0, k * 0.01);
        transform.rotation = Quat::from_rotation_y(k);
        transform.scale = Vec3::splat(1.0 + (k * 0.37).sin().abs() * 3.0);
    }
}

/// The stack after four seconds, scrambled between ticks or not. Under the transform guard, which
/// a debug build installs: avian itself is checked to leave transforms alone inside the tick.
fn run(scrambled: bool) -> AvianHash {
    let mut app = peer_app(HOST_UUID, avian::install);
    if scrambled {
        app.add_systems(Update, scramble);
    }
    spawn_stack(app.world_mut(), BOXES);
    for _ in 0..FRAMES {
        app.update();
    }
    // Two runs that agree prove nothing if both fell through the floor.
    let mut boxes = app
        .world_mut()
        .query_filtered::<&Position, With<TickTrackedEntity>>();
    let lowest = boxes
        .iter(app.world())
        .map(|position| position.0.y.f32())
        .fold(f32::MAX, f32::min);
    assert!(
        lowest > 0.0,
        "the stack fell through the ground: lowest box at {lowest}"
    );
    AvianHash::sample(app.world_mut())
}

#[test]
fn a_scrambled_transform_never_reaches_the_simulation() {
    assert_eq!(
        run(false),
        run(true),
        "the simulation must not depend on `Transform`"
    );
}

/// A body spawned with a `Position` and no `Transform` — what a headless peer has of every body —
/// collides like one that has both. avian attaches a collider to its body only through a
/// `GlobalTransform`, and sizes it from one; without the frame the plugin gives every collider,
/// it would be attached to nothing, scaled to nothing, and fall through the world.
#[test]
fn a_body_without_a_transform_lands_on_the_ground() {
    let mut app = peer_app(HOST_UUID, avian::install);
    app.world_mut().spawn((
        RigidBody::Static,
        Collider::cuboid(40.0, 1.0, 40.0),
        Position(Vec3::new(0.0, -0.5, 0.0).real()),
    ));
    let body = app
        .world_mut()
        .spawn((
            RigidBody::Dynamic,
            Collider::cuboid(1.0, 1.0, 1.0),
            Position(Vec3::new(0.0, 3.0, 0.0).real()),
        ))
        .id();
    assert!(
        app.world().get::<Transform>(body).is_none(),
        "the test is about a body with no transform"
    );
    for _ in 0..FRAMES {
        app.update();
    }
    let y = app.world().get::<Position>(body).unwrap().0.y;
    assert!(
        (y - 0.5).abs() < 0.05,
        "the box should rest on the ground at 0.5, not {y}"
    );
}

#[test]
#[should_panic(expected = "is attached to body")]
fn a_child_collider_panics() {
    let mut app = peer_app(HOST_UUID, avian::install);
    let body = app
        .world_mut()
        .spawn((RigidBody::Dynamic, Transform::default()))
        .id();
    app.world_mut().spawn((
        Collider::cuboid(1.0, 1.0, 1.0),
        Transform::from_xyz(1.0, 0.0, 0.0),
        ChildOf(body),
    ));
    app.update();
}

/// The plugin turns a game's own `PhysicsPlugins` down if they were built with avian's transform
/// handling on: by then a `Position` already requires a `Transform`.
#[test]
#[should_panic(expected = "presentation_transform_config")]
fn physics_added_with_the_transform_sync_on_is_refused() {
    let _ = peer_app(HOST_UUID, |app| {
        app.add_plugins((AssetPlugin::default(), bevy::scene::ScenePlugin))
            .init_asset::<Mesh>()
            .add_plugins(
                PhysicsPlugins::new(bevy_ticked::TickedSimulation)
                    .build()
                    .disable::<::avian3d::dynamics::solver::islands::IslandPlugin>()
                    .disable::<::avian3d::dynamics::solver::islands::IslandSleepingPlugin>(),
            )
            .add_plugins(TickedAvianPlugin::default().physics_added_by_the_game());
    });
}
