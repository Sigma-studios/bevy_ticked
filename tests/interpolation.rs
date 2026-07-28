//! `TickInterpolation` now reads the crate's own clock instead of `Time<Fixed>`.
//!
//! Under `TickSource::FixedUpdate` that must be numerically identical to what it
//! returned before, because consumers use it to position rendered visuals every
//! frame and any divergence shows up as stutter. Under other sources `Time<Fixed>`
//! is unrelated to the tick rate, which is the reason for the change.

use std::time::Duration;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy_ticked::prelude::*;

#[derive(Resource, Default)]
struct Samples {
    from_interpolation: Vec<f32>,
    from_fixed: Vec<f32>,
}

fn sample(interp: TickInterpolation, fixed: Res<Time<Fixed>>, mut out: ResMut<Samples>) {
    out.from_interpolation.push(interp.fraction());
    out.from_fixed.push(fixed.overstep_fraction());
}

#[test]
fn fixed_update_source_matches_the_fixed_clock_exactly() {
    // A frame delta that is not a whole number of ticks, so there is a real
    // non-zero overstep to compare rather than a run of zeroes.
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(TickedPlugin::default())
        .init_resource::<Samples>()
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_micros(
            10_000,
        )))
        .add_systems(Update, sample);

    for _ in 0..20 {
        app.update();
    }

    let samples = app.world().resource::<Samples>();
    assert_eq!(
        samples.from_interpolation, samples.from_fixed,
        "under TickSource::FixedUpdate the tick clock must mirror Time<Fixed> \
         exactly, or every interpolated visual shifts"
    );
    assert!(
        samples.from_interpolation.iter().any(|f| *f > 0.0),
        "the test is worthless if the overstep was always zero"
    );
}

#[test]
fn fraction_stays_in_range_and_sawtooths() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(TickedPlugin {
            source: TickSource::Hz(64.0),
            ..default()
        })
        .init_resource::<Samples>()
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_micros(
            10_000,
        )))
        .add_systems(Update, sample);

    for _ in 0..30 {
        app.update();
    }

    let fractions = &app.world().resource::<Samples>().from_interpolation;
    assert!(
        fractions.iter().all(|f| (0.0..=1.0).contains(f)),
        "blend factor must stay in [0,1], saw {fractions:?}"
    );
    assert!(
        fractions.iter().any(|f| *f > 0.0),
        "10ms frames against a 15.625ms tick must leave an overstep"
    );
    // 10ms per frame against a 15.625ms tick: the accumulator fills, spends a
    // tick, and drops back. Without a reset the fraction would climb forever.
    assert!(
        fractions.windows(2).any(|w| w[1] < w[0]),
        "the fraction must fall back after a tick is spent, saw {fractions:?}"
    );
}

#[test]
fn hz_source_does_not_follow_the_fixed_clock() {
    // The bug this change prevents: at a tick rate unrelated to Bevy's fixed
    // timestep, blending against Time<Fixed> means blending against the wrong
    // clock. The two must visibly disagree.
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(TickedPlugin {
            source: TickSource::Hz(7.0),
            ..default()
        })
        .init_resource::<Samples>()
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_micros(
            10_000,
        )))
        .add_systems(Update, sample);

    for _ in 0..30 {
        app.update();
    }

    let samples = app.world().resource::<Samples>();
    assert_ne!(
        samples.from_interpolation, samples.from_fixed,
        "a 7Hz simulation must not inherit the 64Hz fixed clock's overstep"
    );
}
