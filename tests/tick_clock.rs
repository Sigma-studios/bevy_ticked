//! Driver conformance: a tick must mean the same thing to the simulation no
//! matter what advanced it.
//!
//! Systems inside `TickedSimulation` integrate against the generic `Time`
//! resource — avian reads its whole physics timestep from `Time::delta()`. Bevy
//! only points the generic `Time` at `Time<Fixed>` while `FixedMain` is running,
//! so before the simulation clock existed a manually stepped tick integrated by
//! the *render frame's* delta instead. These tests pin that down.

use std::time::Duration;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy_ticked::prelude::*;
use bevy_ticked::TickedSimulation;

/// What the simulation saw, one entry per tick that actually ran.
#[derive(Resource, Default)]
struct Observed {
    deltas: Vec<Duration>,
    elapsed: Vec<Duration>,
    ticks: Vec<u64>,
}

fn observe(time: Res<Time>, tick: Res<CurrentTick>, mut observed: ResMut<Observed>) {
    observed.deltas.push(time.delta());
    observed.elapsed.push(time.elapsed());
    observed.ticks.push(tick.0);
}

/// `frame_delta` is what each `app.update()` is told has elapsed in real time.
fn app(frame_delta: Duration) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(TickedPlugin::default())
        .init_resource::<Observed>()
        .insert_resource(TimeUpdateStrategy::ManualDuration(frame_delta))
        .add_systems(TickedSimulation, observe);
    app
}

fn timestep(app: &App) -> Duration {
    app.world().resource::<Time<Ticked>>().timestep()
}

fn step_forward(app: &mut App) {
    app.world_mut()
        .resource_mut::<Messages<StepForward>>()
        .write(StepForward);
}

#[test]
fn auto_advance_runs_the_simulation_at_the_tick_timestep() {
    let mut app = app(Duration::from_secs_f64(1.0 / 64.0));
    let timestep = timestep(&app);

    for _ in 0..10 {
        app.update();
    }

    let observed = app.world().resource::<Observed>();
    assert!(!observed.deltas.is_empty(), "no ticks ran");
    assert!(
        observed.deltas.iter().all(|d| *d == timestep),
        "auto-advanced ticks must integrate by exactly one timestep, saw {:?}",
        observed.deltas
    );
}

#[test]
fn manual_step_integrates_by_a_tick_not_by_the_frame() {
    // A deliberately absurd frame delta, and a fixed timestep large enough that
    // FixedUpdate never fires: every tick here comes from StepForward alone. If
    // the simulation read the frame clock, it would see 250ms.
    let mut app = app(Duration::from_millis(250));
    app.world_mut()
        .resource_mut::<Time<Fixed>>()
        .set_timestep(Duration::from_secs(3600));
    let timestep = timestep(&app);

    for _ in 0..5 {
        step_forward(&mut app);
        app.update();
    }

    let observed = app.world().resource::<Observed>();
    assert_eq!(observed.deltas.len(), 5, "expected one tick per StepForward");
    assert!(
        observed.deltas.iter().all(|d| *d == timestep),
        "manually stepped ticks must integrate by one timestep, not the frame \
         delta; saw {:?}",
        observed.deltas
    );
}

#[test]
fn tick_delta_is_independent_of_frame_rate() {
    // The same number of ticks, driven at wildly different frame rates, must
    // integrate identically. This is the property rollback determinism rests on.
    let run = |frame_delta: Duration| {
        let mut app = app(frame_delta);
        app.world_mut()
            .resource_mut::<Time<Fixed>>()
            .set_timestep(Duration::from_secs(3600));
        for _ in 0..8 {
            step_forward(&mut app);
            app.update();
        }
        app.world().resource::<Observed>().deltas.clone()
    };

    assert_eq!(
        run(Duration::from_millis(4)),
        run(Duration::from_millis(33)),
        "tick deltas must not depend on how long the frame took"
    );
}

#[test]
fn simulation_time_is_derived_from_the_tick_counter() {
    let mut app = app(Duration::from_millis(250));
    app.world_mut()
        .resource_mut::<Time<Fixed>>()
        .set_timestep(Duration::from_secs(3600));
    let timestep = timestep(&app);

    for _ in 0..6 {
        step_forward(&mut app);
        app.update();
    }

    let observed = app.world().resource::<Observed>();
    for (elapsed, tick) in observed.elapsed.iter().zip(&observed.ticks) {
        assert_eq!(
            *elapsed,
            timestep * (*tick as u32),
            "elapsed at tick {tick} must be exactly tick * timestep"
        );
    }
}

#[test]
fn the_outer_clock_is_restored_after_a_tick() {
    // Systems outside the simulation must not observe the tick clock leaking.
    let frame_delta = Duration::from_millis(250);
    let mut app = app(frame_delta);
    app.world_mut()
        .resource_mut::<Time<Fixed>>()
        .set_timestep(Duration::from_secs(3600));

    #[derive(Resource, Default)]
    struct OuterDelta(Duration);
    app.init_resource::<OuterDelta>().add_systems(
        Last,
        |time: Res<Time>, mut out: ResMut<OuterDelta>| out.0 = time.delta(),
    );

    // Bevy's first update establishes the time baseline and reports a zero
    // delta, so burn it and take the steady-state frame as the control.
    app.update();
    app.update();
    let without_tick = app.world().resource::<OuterDelta>().0;

    step_forward(&mut app);
    app.update();
    let with_tick = app.world().resource::<OuterDelta>().0;

    assert_eq!(without_tick, frame_delta, "control frame delta");
    assert_eq!(
        with_tick, without_tick,
        "the generic Time must be restored to the frame clock after a tick"
    );
}
