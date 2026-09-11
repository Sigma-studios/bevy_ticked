//! Who sizes the history window, and what it costs.

use std::time::Duration;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy_ticked::prelude::*;

#[test]
fn the_core_default_is_a_hundred_seconds_for_scrubbing() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(TickedPlugin::default())
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_micros(
            15_625,
        )));
    app.finish();
    assert_eq!(
        app.world().resource::<HistoryBufferTicks>().0,
        HISTORY_BUFFER_TICKS
    );
}

#[test]
fn a_games_own_window_wins() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins).add_plugins(TickedPlugin {
        history_ticks: Some(32),
        ..default()
    });
    app.finish();
    assert_eq!(app.world().resource::<HistoryBufferTicks>().0, 32);
}

/// A plugin with a better default for its role replaces the core's, and only the core's.
struct Knows;
impl Plugin for Knows {
    fn build(&self, app: &mut App) {
        if !app.world().contains_resource::<HistoryWindowChosen>() {
            app.insert_resource(HistoryBufferTicks(128));
        }
    }
}

#[test]
fn a_plugin_that_knows_better_replaces_the_default_window() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(TickedPlugin::default())
        .add_plugins(Knows);
    assert_eq!(app.world().resource::<HistoryBufferTicks>().0, 128);
}

#[test]
fn a_plugin_that_knows_better_does_not_override_the_games_choice() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(TickedPlugin {
            history_ticks: Some(32),
            ..default()
        })
        .add_plugins(Knows);
    assert_eq!(app.world().resource::<HistoryBufferTicks>().0, 32);
}

#[test]
fn solo_keeps_fixed_update_by_default() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(TickedPlugin::default());
    app.finish();
    assert_eq!(
        app.world().resource::<ConfiguredTickSource>().0,
        TickSource::FixedUpdate,
        "a solo or scrubbing app is not made to change its clock"
    );
    assert!(!TickSource::FixedUpdate.is_steerable());
    assert!(TickSource::Hz(64.0).is_steerable());
    assert!(TickSource::Manual.is_steerable());
}
