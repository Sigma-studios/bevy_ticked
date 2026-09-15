//! Islands exist to sleep bodies. With sleeping off, the bundle leaves them out, and refuses a
//! game that brought them along anyway.

use bevy::prelude::*;
use bevy_ticked::TickedSimulation;
use bevy_ticked_avian::avian3d::TickedAvianPlugin;
use bevy_ticked_testing::fixtures::avian;
use bevy_ticked_testing::prelude::*;

// The 3d flavour, like the fixture: one plugin per dimension and the logic is the same macro.
use avian3d::dynamics::solver::islands::{IslandPlugin, IslandSleepingPlugin, PhysicsIslands};
use avian3d::prelude::PhysicsPlugins;

/// The documented bundle runs without islands: no plugin, no resource, no node to lose.
#[test]
fn the_bundle_adds_no_islands() {
    let app = peer_app(HOST_UUID, avian::install);
    assert!(!app.is_plugin_added::<IslandPlugin>());
    assert!(!app.is_plugin_added::<IslandSleepingPlugin>());
    assert!(!app.world().contains_resource::<PhysicsIslands>());
}

/// The negative control keeps them: sleeping needs islands, and `allow_sleeping()` says so.
#[test]
fn allowing_sleep_keeps_the_islands() {
    let app = peer_app(HOST_UUID, avian::install_sleepy);
    assert!(app.is_plugin_added::<IslandPlugin>());
    assert!(app.world().contains_resource::<PhysicsIslands>());
}

/// A game that adds `PhysicsPlugins` itself and forgets to leave the islands out is told at
/// startup, not by `merge_islands` a few rounds in.
#[test]
#[should_panic(expected = "IslandPlugin is added and sleeping is off")]
fn a_game_that_brings_islands_with_sleeping_off_is_refused() {
    let mut app = peer_app(HOST_UUID, |app| {
        app.add_plugins(PhysicsPlugins::new(TickedSimulation));
        avian::install_with(
            app,
            TickedAvianPlugin::default().physics_added_by_the_game(),
        );
    });
    app.finish();
}

/// And the same game, with the islands left out as documented, starts.
#[test]
fn a_game_that_leaves_islands_out_is_accepted() {
    let mut app = peer_app(HOST_UUID, |app| {
        app.add_plugins(
            PhysicsPlugins::new(TickedSimulation)
                .build()
                .disable::<IslandPlugin>()
                .disable::<IslandSleepingPlugin>(),
        );
        avian::install_with(
            app,
            TickedAvianPlugin::default().physics_added_by_the_game(),
        );
    });
    app.finish();
    assert!(!app.world().contains_resource::<PhysicsIslands>());
}
