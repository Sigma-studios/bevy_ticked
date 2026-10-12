//! What the tick loop shows a simulation: the tick's clock inside it, and a manual step or rewind
//! that runs the whole loop around it.

use std::time::Duration;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy_ticked::prelude::*;
use bevy_ticked::tracked_entity::TickTrackedEntity;

#[derive(Component, Clone, Copy)]
struct Speed(f32);

/// A simulated position along x.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
struct X(f32);

/// One unit per tick along x, integrated from `Time` so a wrong clock shows too.
fn integrate(time: Res<Time>, mut bodies: Query<(&mut X, &Speed)>) {
    let dt = time.delta_secs();
    for (mut x, speed) in &mut bodies {
        x.0 += speed.0 * dt;
    }
}

fn x(app: &mut App) -> f32 {
    let mut q = app.world_mut().query::<&X>();
    q.single(app.world()).unwrap().0
}

/// A frame that is not a whole number of ticks.
const FRAME: Duration = Duration::from_micros(23_437);

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
        for (name, value) in [
            ("Time", t),
            ("Time<Virtual>", v),
            ("Time<Fixed>", f),
            ("Time<Real>", r),
        ] {
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
        .register_ticked_component::<X>()
        .add_systems(TickedSimulation, integrate)
        .add_systems(
            TickedLoop,
            (
                (|tick: Res<CurrentTick>, mut p: ResMut<Passes>| p.0.push(("pre", tick.0.0)))
                    .in_set(TickedSystems::PreTick),
                (|tick: Res<CurrentTick>, mut p: ResMut<Passes>| p.0.push(("post", tick.0.0)))
                    .in_set(TickedSystems::PostTick),
            ),
        );
    app.world_mut()
        .spawn((TickTrackedEntity(1), X::default(), Speed(64.0)));
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
    assert_eq!(app.world().resource::<CurrentTick>().0, Tick(1));
}

#[test]
fn a_manual_rewind_runs_the_loop_without_advancing() {
    let mut app = stepping_app();
    app.update();
    for _ in 0..3 {
        app.world_mut().write_message(StepForward);
        app.update();
    }
    assert_eq!(app.world().resource::<CurrentTick>().0, Tick(3));
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
    assert_eq!(app.world().resource::<CurrentTick>().0, Tick(2));
    assert!(
        (x(&mut app) - 2.0).abs() < 1e-6,
        "the world is the restored tick's"
    );
}

#[test]
fn a_manual_step_while_paused_still_advances() {
    let mut app = stepping_app();
    app.world_mut()
        .resource_mut::<TickHolds>()
        .hold(TickHoldReason::Manual);
    app.update();
    app.world_mut().write_message(StepForward);
    app.update();
    assert_eq!(app.world().resource::<CurrentTick>().0, Tick(1));
    assert!(
        !app.world().contains_resource::<StepOnce>(),
        "the step marker is gone once the step has run"
    );
}
