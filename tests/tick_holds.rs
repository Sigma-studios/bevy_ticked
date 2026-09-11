//! One clock, many reasons to stop it, and nobody lifts anybody else's.

use std::time::Duration;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy_ticked::prelude::*;

fn app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(TickedPlugin {
            source: TickSource::Hz(64.0),
            ..default()
        })
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_micros(
            15_625,
        )));
    app.update();
    app
}

fn tick(app: &App) -> u64 {
    app.world().resource::<CurrentTick>().0
}

fn holds(app: &mut App) -> Mut<'_, TickHolds> {
    app.world_mut().resource_mut::<TickHolds>()
}

#[test]
fn the_clock_advances_only_when_nobody_holds_it() {
    let mut app = app();
    app.update();
    assert_eq!(tick(&app), 1);

    holds(&mut app).hold(TickHoldReason::Manual);
    holds(&mut app).hold(TickHoldReason::WaitingForPeers);
    app.update();
    assert_eq!(tick(&app), 1, "held twice over");

    holds(&mut app).release(TickHoldReason::WaitingForPeers);
    app.update();
    assert_eq!(tick(&app), 1, "one reason gone, one remains: still held");

    assert!(holds(&mut app).release(TickHoldReason::Manual));
    app.update();
    assert_eq!(tick(&app), 2, "the last reason gone: running");
    assert!(
        !holds(&mut app).release(TickHoldReason::Manual),
        "releasing twice is a no-op"
    );
}

#[test]
fn reasons_are_reported_in_a_fixed_order() {
    let mut app = app();
    holds(&mut app).hold(TickHoldReason::SoftHold);
    holds(&mut app).hold(TickHoldReason::Manual);
    holds(&mut app).hold(TickHoldReason::Custom(3));
    let reasons: Vec<_> = app.world().resource::<TickHolds>().reasons().collect();
    assert_eq!(
        reasons,
        [
            TickHoldReason::Manual,
            TickHoldReason::SoftHold,
            TickHoldReason::Custom(3)
        ]
    );
    assert!(
        !app.world()
            .resource::<TickHolds>()
            .held_only_by(TickHoldReason::Manual)
    );
}

#[test]
fn a_manual_step_advances_through_a_hold() {
    let mut app = app();
    holds(&mut app).hold(TickHoldReason::Manual);
    app.update();
    let before = tick(&app);
    app.world_mut().write_message(StepForward);
    app.update();
    assert_eq!(tick(&app), before + 1);
    assert!(
        app.world()
            .resource::<TickHolds>()
            .holds(TickHoldReason::Manual),
        "a step does not lift the hold"
    );
}
