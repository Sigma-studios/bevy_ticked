//! A client's correction goes through the same door as every other rewind.
//!
//! `handle_server_snapshot` used to write the authority's networked state into the world and
//! replay from there, and nothing else: rollback-only components kept the client's last
//! predicted value, and the event logs kept events from ticks the authority had just erased.
//! Both presented as "the client is slightly off for a moment" and neither was reproducible
//! without a lossy link.

use std::time::Duration;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy_ticked::prelude::*;
use bevy_ticked::registry::TickedComponentRegistry;
use bevy_ticked::tracked_entity::TickTrackedEntity;
use bevy_ticked_networking::client::{ClientTickBuffer, LocalClientPlayer};
use bevy_ticked_networking::messages::ReceivedNetworkSnapshot;
use bevy_ticked_networking::prelude::*;
use bevy_ticked_networking::snapshot::{SnapshotPacket, build_full_body};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
struct Input;

#[derive(Component, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Pos(i32);

/// Rollback-only: registered without a wire name, never in a snapshot.
#[derive(Component, Clone, Copy, Debug, PartialEq)]
struct TicksSeen(u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Footstep(u64);

const LOCAL: u128 = 7;
const TICK: Duration = Duration::from_micros(15_625);

fn count_ticks(mut bodies: Query<&mut TicksSeen>) {
    for mut seen in &mut bodies {
        seen.0 += 1;
    }
}

/// Whether the simulation is producing footsteps. Flipped off before a correction, so the
/// replay produces none and anything left in the log for the replayed ticks is a ghost.
#[derive(Resource)]
struct Walking(bool);

fn step_each_tick(
    tick: Res<CurrentTick>,
    walking: Res<Walking>,
    mut steps: TickedEventWriter<Footstep>,
) {
    if walking.0 {
        steps.write(tick.0, Footstep(tick.0));
    }
}

#[derive(Resource, Default)]
struct Presented(Vec<u64>);

fn present(mut steps: TickedEventReader<Footstep>, mut out: ResMut<Presented>) {
    for (tick, _) in steps.read() {
        out.0.push(tick);
    }
}

fn client() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(TickedPlugin {
            source: TickSource::Hz(64.0),
            ..default()
        })
        .add_plugins(TickedClientPlugin::<Input>::new())
        .insert_resource(TimeUpdateStrategy::ManualDuration(TICK))
        .register_networked_ticked_component::<Pos>("Pos")
        .register_ticked_component::<TicksSeen>()
        .add_ticked_event::<Footstep>()
        .insert_resource(Walking(true))
        .init_resource::<Presented>()
        .add_systems(TickedSimulation, (count_ticks, step_each_tick))
        .add_systems(Update, present);
    app.insert_resource(LocalClientPlayer(LOCAL));
    app
}

/// A host-shaped snapshot for `tick`, built from what the client itself predicted for that
/// tick, so the correction agrees with the prediction and only the rollback's bookkeeping is
/// under test. From history, not a fresh capture: capturing now would overwrite the tick's
/// history with the current world, which is the very thing the rollback must not see.
fn deliver(app: &mut App, tick: u64) {
    let mut packet = SnapshotPacket::full(tick, build_full_body(app.world_mut(), tick));
    packet.your_margin = 2;
    app.world_mut().trigger(ReceivedNetworkSnapshot(packet));
}

fn sync(app: &mut App) {
    app.update();
    app.world_mut()
        .spawn((TickTrackedEntity(1), Pos(0), TicksSeen(0)));
    let registry = app.world().resource::<TickedComponentRegistry>().clone();
    registry.capture_all(app.world_mut(), 0);
    deliver(app, 0);
    app.update();
}

fn ticks_seen(app: &mut App) -> u64 {
    let mut q = app.world_mut().query::<&TicksSeen>();
    q.single(app.world()).unwrap().0
}

#[test]
fn a_client_rollback_restores_rollback_only_components() {
    let mut app = client();
    sync(&mut app);
    for _ in 0..20 {
        app.update();
    }
    let lead = app.world().resource::<ClientTickBuffer>().target_replay_distance;
    let current = app.world().resource::<CurrentTick>().0;
    let before = ticks_seen(&mut app);
    assert_eq!(before, current, "one increment per tick, so far");

    // A snapshot that agrees with the prediction, so the only thing that can go wrong is the
    // rollback's own bookkeeping.
    deliver(&mut app, current - lead);
    app.update();

    let current = app.world().resource::<CurrentTick>().0;
    assert_eq!(
        ticks_seen(&mut app),
        current,
        "after a rollback the rollback-only counter should still equal the tick: it was \
         not restored to the snapshot tick's value before the replay re-counted {lead} ticks"
    );
}

#[test]
fn a_client_rollback_unpublishes_events_from_ticks_the_authority_erased() {
    let mut app = client();
    sync(&mut app);
    for _ in 0..20 {
        app.update();
    }
    let lead = app.world().resource::<ClientTickBuffer>().target_replay_distance;
    let current = app.world().resource::<CurrentTick>().0;

    let presented_before = app.world().resource::<Presented>().0.len();
    assert!(presented_before > 0);

    // The authority's version of the last `lead` ticks has no footsteps in it.
    app.insert_resource(Walking(false));
    deliver(&mut app, current - lead);
    app.update();

    let log = app.world().resource::<TickedEvents<Footstep>>();
    for tick in (current - lead + 1)..=current {
        assert!(
            log.at_tick(tick).is_empty(),
            "tick {tick} was replayed without a footstep, but the log still holds {:?}: the \
             prediction's events were not truncated before the replay",
            log.at_tick(tick)
        );
    }
    let mut ticks_with_steps = 0;
    for tick in 0..=current {
        ticks_with_steps += usize::from(!log.at_tick(tick).is_empty());
    }
    assert_eq!(
        ticks_with_steps as u64,
        current - lead,
        "the footsteps up to the snapshot's tick are the ones that survive"
    );
    // And nothing new was presented for the replayed range.
    assert_eq!(
        app.world().resource::<Presented>().0.len(),
        presented_before,
        "a ghost footstep from an erased tick reached the presenter"
    );
}

#[test]
fn the_client_plugin_sizes_the_history_window() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(TickedPlugin {
            source: TickSource::Hz(64.0),
            ..default()
        })
        .add_plugins(TickedClientPlugin::<Input>::new());
    app.finish();
    let window = app.world().resource::<HistoryBufferTicks>().0;
    assert!(
        (2 * 64..HISTORY_BUFFER_TICKS).contains(&window),
        "a client keeps what a rollback can reach, not a hundred seconds: {window}"
    );
}

#[test]
#[should_panic(expected = "cannot run on TickSource::FixedUpdate")]
fn the_client_plugin_refuses_fixed_update() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(TickedPlugin::default())
        .add_plugins(TickedClientPlugin::<Input>::new());
    app.finish();
}

#[test]
#[should_panic(expected = "cannot run on TickSource::FixedUpdate")]
fn the_server_plugin_refuses_fixed_update() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(TickedPlugin::default())
        .add_plugins(TickedServerPlugin::<Input>::new());
    app.finish();
}

#[test]
fn the_networking_plugins_accept_manual_and_hz() {
    for source in [TickSource::Manual, TickSource::Hz(64.0)] {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_plugins(TickedPlugin {
                source,
                ..default()
            })
            .add_plugins(TickedClientPlugin::<Input>::new())
            .add_plugins(TickedServerPlugin::<Input>::new());
        app.finish();
    }
}
