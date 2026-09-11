//! avian integrates from `Transform` when it changed, so a blended transform left in the
//! component is a position the physics engine adopts. The audit's probe: a body at 64 units per
//! second crossed 75 units in 99 ticks. With `TickedSystems::Restore` it crosses 99.

use std::time::Duration;

use avian3d::prelude::*;
use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy_ticked::prelude::*;
use bevy_ticked::tracked_entity::TickTrackedEntity;

/// Not a whole number of ticks, so every frame ends on a real blend.
const FRAME: Duration = Duration::from_micros(23_437);

#[test]
fn an_avian_body_under_interpolation_keeps_its_speed() {
    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        TransformPlugin,
        AssetPlugin::default(),
        bevy::scene::ScenePlugin,
    ))
    .init_asset::<Mesh>()
    .add_plugins(TickedPlugin {
        source: TickSource::Hz(64.0),
        ..default()
    })
    .add_plugins(PhysicsPlugins::new(TickedSimulation))
    .add_plugins(TickedInterpolationPlugin)
    .insert_resource(Gravity(Vec3::ZERO))
    .insert_resource(TimeUpdateStrategy::ManualDuration(FRAME))
    .register_ticked_component::<Transform>()
    .register_ticked_component::<Position>()
    .register_ticked_component::<LinearVelocity>();
    app.finish();
    app.cleanup();

    app.world_mut().spawn((
        TickTrackedEntity(1),
        RigidBody::Dynamic,
        Collider::sphere(0.5),
        LinearVelocity(Vec3::X * 64.0),
        Transform::default(),
        TickedInterpolation::default(),
    ));

    let mut frames = 0;
    while app.world().resource::<CurrentTick>().0 < 99 {
        app.update();
        frames += 1;
        assert!(frames < 1000);
    }
    // A frame can run two ticks, so land on 99 or 100 and expect that many units.
    let ticks = app.world().resource::<CurrentTick>().0 as f32;

    let mut q = app.world_mut().query::<(&Position, &TickedInterpolation)>();
    let (position, interpolation) = q.single(app.world()).unwrap();
    let simulated = interpolation.current().unwrap().translation.x;
    assert!(
        (position.x - ticks).abs() < 0.5,
        "avian's own position after {ticks} ticks at 64 u/s is x = {}; the audit measured 75.5 \
         at tick 99 when the blend fed back through Transform",
        position.x
    );
    assert!((simulated - ticks).abs() < 0.5, "and the tick's transform agrees: {simulated}");
}
