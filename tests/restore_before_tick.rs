//! The blend the renderer sees never reaches the simulation.
//!
//! `TickedInterpolationPlugin` writes a blended `Transform` for the frame. Before this suite
//! that value stayed in the component into the next tick, so the tick integrated from a
//! transform that was `fraction` of the way back toward the previous tick. The audit's probe
//! measured it: a body meant to cross 99 units in 99 ticks crossed 75. `TickedSystems::Restore`
//! puts the true transform back before anything in the loop reads it.

use std::time::Duration;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy_ticked::prelude::*;
use bevy_ticked::tracked_entity::TickTrackedEntity;

#[derive(Component, Clone, Copy)]
struct Speed(f32);

/// One unit per tick along x, integrated from `Time` so a wrong clock shows too.
fn integrate(time: Res<Time>, mut bodies: Query<(&mut Transform, &Speed)>) {
    let dt = time.delta_secs();
    for (mut transform, speed) in &mut bodies {
        transform.translation.x += speed.0 * dt;
    }
}

/// A frame that is not a whole number of ticks, so every frame ends with a real blend.
const FRAME: Duration = Duration::from_micros(23_437);

fn app() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, TransformPlugin))
        .add_plugins(TickedPlugin {
            source: TickSource::Hz(64.0),
            ..default()
        })
        .add_plugins(TickedInterpolationPlugin)
        .insert_resource(TimeUpdateStrategy::ManualDuration(FRAME))
        .register_ticked_component::<Transform>()
        .add_systems(TickedSimulation, integrate);
    app.world_mut().spawn((
        TickTrackedEntity(1),
        Transform::default(),
        Speed(64.0),
        TickedInterpolation::default(),
    ));
    app
}

fn x(app: &mut App) -> f32 {
    let mut q = app.world_mut().query::<&Transform>();
    q.single(app.world()).unwrap().translation.x
}

#[test]
fn interpolation_never_feeds_the_blend_back_into_the_simulation() {
    let mut app = app();
    let mut frames = 0;
    while app.world().resource::<CurrentTick>().0 < 99 {
        app.update();
        frames += 1;
        assert!(frames < 1000, "the clock never reached tick 99");
    }
    // A frame can run two ticks, so land on 99 or 100 and expect that many units.
    let ticks = app.world().resource::<CurrentTick>().0 as f32;

    // What the simulation holds, not what the renderer was shown.
    let mut q = app.world_mut().query::<&TickedInterpolation>();
    let simulated = q
        .single(app.world())
        .unwrap()
        .current()
        .expect("two ticks have run")
        .translation
        .x;
    assert!(
        (simulated - ticks).abs() < 1e-3,
        "{ticks} ticks at one unit per tick left the body at x = {simulated}; the audit's \
         probe measured 75.5 at tick 99 when the blend fed back"
    );
}

#[test]
fn the_true_transform_is_back_before_pre_tick() {
    #[derive(Resource, Default)]
    struct SeenInPreTick(Vec<f32>);

    let mut app = app();
    app.init_resource::<SeenInPreTick>().add_systems(
        TickedLoop,
        (|bodies: Query<(&Transform, &TickedInterpolation)>, mut seen: ResMut<SeenInPreTick>| {
            for (transform, interpolation) in &bodies {
                if let Some(current) = interpolation.current() {
                    seen.0
                        .push(transform.translation.x - current.translation.x);
                }
            }
        })
        .in_set(TickedSystems::PreTick),
    );
    for _ in 0..40 {
        app.update();
    }
    let seen = &app.world().resource::<SeenInPreTick>().0;
    assert!(seen.len() > 10, "PreTick never saw a body with two states");
    assert!(
        seen.iter().all(|difference| difference.abs() < 1e-6),
        "PreTick read a transform that differed from the simulation's: {seen:?}"
    );
    // And the renderer still gets a blend: after a frame the transform is between two ticks.
    let shown = x(&mut app);
    let mut q = app.world_mut().query::<&TickedInterpolation>();
    let current = q.single(app.world()).unwrap().current().unwrap().translation.x;
    assert!(
        shown < current,
        "the frame ended on a partial tick, so the shown x ({shown}) should lag the simulated \
         x ({current})"
    );
}

/// Every frame clock reads the tick inside the simulation.
#[test]
fn the_frame_clocks_read_tick_values_inside_the_simulation() {
    #[derive(Resource, Default)]
    struct Deltas(Vec<(f32, f32, f32, f32)>);

    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(TickedPlugin {
            source: TickSource::Hz(64.0),
            ..default()
        })
        .insert_resource(TimeUpdateStrategy::ManualDuration(FRAME))
        .init_resource::<Deltas>()
        .add_systems(
            TickedSimulation,
            |time: Res<Time>,
             virt: Res<Time<Virtual>>,
             fixed: Res<Time<Fixed>>,
             real: Res<Time<Real>>,
             mut out: ResMut<Deltas>| {
                out.0.push((
                    time.delta_secs(),
                    virt.delta_secs(),
                    fixed.delta_secs(),
                    real.delta_secs(),
                ));
            },
        );
    for _ in 0..20 {
        app.update();
    }
    let deltas = &app.world().resource::<Deltas>().0;
    assert!(deltas.len() > 10);
    let tick = 1.0 / 64.0;
    for (t, v, f, r) in deltas {
        for (name, value) in [("Time", t), ("Time<Virtual>", v), ("Time<Fixed>", f), ("Time<Real>", r)] {
            assert!(
                (value - tick).abs() < 1e-6,
                "{name}::delta inside a tick was {value}, not one tick ({tick}); a system \
                 reading it would integrate by the frame on the first run and by the tick on \
                 a replay"
            );
        }
    }
    // Outside the tick the frame clock is the frame's again.
    let outer = app.world().resource::<Time<Virtual>>().delta();
    assert_eq!(outer, FRAME);
}

// ── Manual stepping runs the whole loop ──────────────────────────────────────

#[derive(Resource, Default)]
struct Passes(Vec<(&'static str, u64)>);

fn stepping_app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(TickedPlugin {
            source: TickSource::Manual,
            ..default()
        })
        .insert_resource(TimeUpdateStrategy::ManualDuration(FRAME))
        .init_resource::<Passes>()
        .register_ticked_component::<Transform>()
        .add_systems(TickedSimulation, integrate)
        .add_systems(
            TickedLoop,
            (
                (|tick: Res<CurrentTick>, mut p: ResMut<Passes>| p.0.push(("pre", tick.0)))
                    .in_set(TickedSystems::PreTick),
                (|tick: Res<CurrentTick>, mut p: ResMut<Passes>| p.0.push(("post", tick.0)))
                    .in_set(TickedSystems::PostTick),
            ),
        );
    app.world_mut().spawn((TickTrackedEntity(1), Transform::default(), Speed(64.0)));
    app
}

#[test]
fn a_manual_step_runs_pre_and_post_tick() {
    let mut app = stepping_app();
    app.update();
    app.world_mut().resource_mut::<Passes>().0.clear();

    app.world_mut().write_message(StepForward);
    app.update();

    let passes = &app.world().resource::<Passes>().0;
    assert_eq!(
        passes,
        &[("pre", 0), ("post", 1)],
        "a manual step used to advance the tick around the loop, so nothing in PreTick or \
         PostTick ran; saw {passes:?}"
    );
    assert_eq!(app.world().resource::<CurrentTick>().0, 1);
}

#[test]
fn a_manual_rewind_runs_the_loop_without_advancing() {
    let mut app = stepping_app();
    app.update();
    for _ in 0..3 {
        app.world_mut().write_message(StepForward);
        app.update();
    }
    assert_eq!(app.world().resource::<CurrentTick>().0, 3);
    app.world_mut().resource_mut::<Passes>().0.clear();

    app.world_mut().write_message(StepBackward);
    app.update();

    let passes = &app.world().resource::<Passes>().0;
    assert_eq!(
        passes,
        &[("pre", 2), ("post", 2)],
        "after a rewind the loop runs once at the restored tick and the tick does not advance; \
         saw {passes:?}"
    );
    assert_eq!(app.world().resource::<CurrentTick>().0, 2);
    assert!((x(&mut app) - 2.0).abs() < 1e-6, "the world is the restored tick's");
}

#[test]
fn a_manual_step_while_paused_still_advances() {
    let mut app = stepping_app();
    app.insert_resource(TicksPaused);
    app.update();
    app.world_mut().write_message(StepForward);
    app.update();
    assert_eq!(app.world().resource::<CurrentTick>().0, 1);
    assert!(
        !app.world().contains_resource::<StepOnce>(),
        "the step marker is gone once the step has run"
    );
}
