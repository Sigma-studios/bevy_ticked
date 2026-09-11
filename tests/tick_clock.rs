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
    source_app(TickSource::FixedUpdate, frame_delta)
}

/// An app where nothing advances the clock, so every tick comes from an explicit
/// `StepForward`.
fn manual_app(frame_delta: Duration) -> App {
    source_app(TickSource::Manual, frame_delta)
}

fn source_app(source: TickSource, frame_delta: Duration) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(TickedPlugin {
            source,
            ..default()
        })
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
    // A deliberately absurd frame delta, and nothing driving the clock: every
    // tick here comes from StepForward alone. If the simulation read the frame
    // clock, it would see 250ms.
    let mut app = manual_app(Duration::from_millis(250));
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
        let mut app = manual_app(frame_delta);
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
    let mut app = manual_app(Duration::from_millis(250));
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

/// An app whose ticks come from the crate's own accumulator, not FixedUpdate.
fn hz_app(hz: f64, frame_delta: Duration, max_ticks_per_frame: u32) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(TickedPlugin {
            source: TickSource::Hz(hz),
            max_ticks_per_frame,
            ..default()
        })
        .init_resource::<Observed>()
        .insert_resource(TimeUpdateStrategy::ManualDuration(frame_delta))
        .add_systems(TickedSimulation, observe);
    app
}

#[test]
fn hz_source_uses_the_requested_timestep() {
    let app = hz_app(30.0, Duration::from_secs_f64(1.0 / 30.0), 16);
    assert_eq!(
        app.world().resource::<Time<Ticked>>().timestep(),
        Duration::from_secs_f64(1.0 / 30.0),
        "TickSource::Hz must set the simulation timestep, not just the rate"
    );
}

#[test]
fn hz_source_runs_one_tick_per_frame_at_matching_rates() {
    let mut app = hz_app(30.0, Duration::from_secs_f64(1.0 / 30.0), 16);
    // Bevy's first frame reports a zero delta, so it produces no tick.
    for _ in 0..11 {
        app.update();
    }

    let observed = app.world().resource::<Observed>();
    assert_eq!(observed.ticks, (1..=10).collect::<Vec<_>>());
    assert!(
        observed
            .deltas
            .iter()
            .all(|d| *d == Duration::from_secs_f64(1.0 / 30.0)),
        "every tick must be worth 1/30s at 30Hz, saw {:?}",
        observed.deltas
    );
}

#[test]
fn hz_source_catches_up_within_one_frame() {
    // Four ticks' worth of time arrives in a single frame; all four must run,
    // rather than three being dropped.
    let mut app = hz_app(64.0, Duration::from_secs_f64(4.0 / 64.0), 16);
    app.update();
    app.update();

    assert_eq!(
        app.world().resource::<Observed>().ticks,
        vec![1, 2, 3, 4],
        "the accumulator must spend all whole ticks it has banked"
    );
}

#[test]
fn hz_source_does_not_run_at_bevys_fixed_rate() {
    // The point of the whole exercise: the simulation rate is independent of
    // Bevy's fixed timestep, which stays at its 64Hz default here.
    let mut app = hz_app(10.0, Duration::from_secs_f64(1.0 / 10.0), 16);
    for _ in 0..6 {
        app.update();
    }

    let observed = app.world().resource::<Observed>();
    assert_eq!(observed.ticks.len(), 5, "10Hz over 5 real frames is 5 ticks");
    assert!(
        observed
            .deltas
            .iter()
            .all(|d| *d == Duration::from_secs_f64(0.1)),
        "ticks must be 100ms, not Bevy's 15.625ms fixed step"
    );
}

#[test]
fn catch_up_is_bounded_by_max_ticks_per_frame() {
    // A long frame must not be allowed to run unbounded ticks; a tick here can
    // drag a rollback resimulation behind it.
    let mut app = hz_app(64.0, Duration::from_millis(100), 2);
    app.update();
    app.update();

    let ran = app.world().resource::<Observed>().ticks.len();
    assert_eq!(
        ran, 2,
        "100ms at 64Hz is 6 ticks of backlog; the ceiling of 2 must hold"
    );
}

#[test]
fn the_outer_clock_is_restored_after_a_tick() {
    // Systems outside the simulation must not observe the tick clock leaking.
    let frame_delta = Duration::from_millis(250);
    let mut app = manual_app(frame_delta);

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
