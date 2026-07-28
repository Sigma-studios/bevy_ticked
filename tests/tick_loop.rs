//! The tick lifecycle sets must be ordered wherever the clock comes from.
//!
//! `TickedSystems` used to be configured only when auto-advancing, while the
//! networking plugins registered into those sets unconditionally. Ordering
//! against a set with no members is a silent no-op in Bevy — no error, no panic
//! — so in manual mode the entire networking stack ran unordered and nothing
//! said so. Hosting the sets in `TickedLoop` unconditionally is what fixes that;
//! these tests hold it fixed.

use std::time::Duration;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy_ticked::prelude::*;

#[derive(Resource, Default)]
struct Order(Vec<&'static str>);

fn app(auto_advance: bool) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(TickedPlugin { auto_advance })
        .init_resource::<Order>()
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            1.0 / 64.0,
        )))
        .add_systems(
            TickedLoop,
            (
                (|mut o: ResMut<Order>| o.0.push("pre")).in_set(TickedSystems::PreTick),
                (|mut o: ResMut<Order>| o.0.push("post")).in_set(TickedSystems::PostTick),
            ),
        );
    app
}

#[test]
fn lifecycle_sets_are_ordered_when_auto_advancing() {
    let mut app = app(true);
    for _ in 0..4 {
        app.update();
    }

    let order = &app.world().resource::<Order>().0;
    assert!(!order.is_empty(), "the tick loop never ran");
    assert!(
        order.chunks(2).all(|c| c == ["pre", "post"]),
        "PreTick must precede PostTick every pass, saw {order:?}"
    );
}

#[test]
fn lifecycle_sets_are_configured_even_without_the_fixed_update_driver() {
    // The regression: with no auto-advance the sets used to be unconfigured, so
    // membership silently bought no ordering at all. The driver is gone here, so
    // the loop should not run on its own...
    let mut app = app(false);
    for _ in 0..4 {
        app.update();
    }
    assert!(
        app.world().resource::<Order>().0.is_empty(),
        "nothing should drive the tick loop without a driver"
    );

    // ...but when something does run it, the sets must still be ordered.
    app.world_mut().run_schedule(TickedLoop);
    assert_eq!(app.world().resource::<Order>().0, ["pre", "post"]);
}

#[test]
fn the_tick_advances_between_pre_and_post() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(TickedPlugin::default())
        .init_resource::<Order>()
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            1.0 / 64.0,
        )))
        .add_systems(
            TickedLoop,
            (
                (|t: Res<CurrentTick>, mut o: ResMut<Order>| {
                    o.0.push(if t.0 == 0 { "pre@0" } else { "pre@n" })
                })
                .in_set(TickedSystems::PreTick),
                (|t: Res<CurrentTick>, mut o: ResMut<Order>| {
                    o.0.push(if t.0 == 0 { "post@0" } else { "post@n" })
                })
                .in_set(TickedSystems::PostTick),
            ),
        );

    app.update();
    app.update();

    let order = &app.world().resource::<Order>().0;
    assert_eq!(
        order.first().copied(),
        Some("pre@0"),
        "the first PreTick runs before tick 1 exists"
    );
    assert!(
        order.iter().all(|s| *s != "post@0"),
        "PostTick must always observe an advanced tick, saw {order:?}"
    );
}
