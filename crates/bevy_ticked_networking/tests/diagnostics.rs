//! The counters a session shows are counted where the events happen, so a test and an overlay
//! read the same number. Each test here fires one kind of event and reads one counter.

use std::time::Duration;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy_ticked::diagnostics::TickCost;
use bevy_ticked::prelude::*;
use bevy_ticked::registry::TickedComponentRegistry;
use bevy_ticked::tracked_entity::{TickTrackedEntity, TickTrackedEntityCounter};
use bevy_ticked_networking::client::{ClientTickBuffer, LocalClientPlayer};
use bevy_ticked_networking::diagnostics::{HealthWarnings, InputStats, ReplayStats, SnapshotStats};
use bevy_ticked_networking::messages::{ReceivedNetworkInput, ReceivedNetworkSnapshot};
use bevy_ticked_networking::prelude::*;
use bevy_ticked_networking::server::LocalServerPlayer;
use bevy_ticked_networking::snapshot::{EntityRecord, SnapshotBody, SnapshotPacket, build_full_body};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
struct Input {
    forward: bool,
}

#[derive(Component, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Pos(i32);

const LOCAL: u128 = 7;
const HOST: u128 = 1;
const TICK: Duration = Duration::from_micros(15_625);

fn client() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(TickedPlugin {
            source: TickSource::Hz(64.0),
            ..default()
        })
        .add_plugins(TickedClientPlugin::<Input>::new())
        .insert_resource(TimeUpdateStrategy::ManualDuration(TICK))
        .register_networked_ticked_component::<Pos>("Pos");
    app.world_mut().spawn((TickTrackedEntity(1), Pos(0)));
    app.insert_resource(LocalClientPlayer(LOCAL));
    app
}

fn host() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(TickedPlugin {
            source: TickSource::Hz(64.0),
            ..default()
        })
        .add_plugins(TickedServerPlugin::<Input>::new())
        .insert_resource(TimeUpdateStrategy::ManualDuration(TICK))
        .register_networked_ticked_component::<Pos>("Pos");
    app.insert_resource(LocalServerPlayer(HOST));
    app
}

fn snapshot_matching(client: &mut App, tick: u64) -> SnapshotPacket {
    let registry = client.world().resource::<TickedComponentRegistry>().clone();
    registry.capture_all(client.world_mut(), tick);
    let mut packet = SnapshotPacket::full(tick, build_full_body(client.world_mut(), tick));
    packet.your_margin = 2;
    packet
}

fn deliver(app: &mut App, tick: u64) {
    let snapshot = snapshot_matching(app, tick);
    app.world_mut().trigger(ReceivedNetworkSnapshot(snapshot));
}

fn sync(app: &mut App) {
    app.update();
    app.world_mut().spawn((TickTrackedEntity(1), Pos(0)));
    deliver(app, 0);
    app.update();
}

fn replay(app: &App) -> ReplayStats {
    *app.world().resource::<ReplayStats>()
}

// ── ReplayStats ──────────────────────────────────────────────────────────────

#[test]
fn every_applied_snapshot_is_counted_and_its_replay_distance_is_the_lead() {
    let mut app = client();
    sync(&mut app);
    let before = replay(&app);
    assert_eq!(before.snapshots_applied, 1, "the initial sync is a snapshot too");

    let lead = app.world().resource::<ClientTickBuffer>().target_replay_distance;
    let current = app.world().resource::<CurrentTick>().0;
    deliver(&mut app, current - lead);
    app.update();

    let after = replay(&app);
    assert_eq!(after.snapshots_applied, 2);
    assert_eq!(after.rollbacks, before.rollbacks + 1);
    assert_eq!(
        after.last_replay_distance, lead as i64,
        "a client leading by `lead` ticks replays `lead` ticks on a snapshot at current - lead"
    );
    assert_eq!(
        after.ticks_replayed - before.ticks_replayed,
        lead,
        "and the tick counter says the same thing"
    );
}

#[test]
fn a_stale_snapshot_is_counted_as_dropped_not_applied() {
    let mut app = client();
    sync(&mut app);
    let current = app.world().resource::<CurrentTick>().0;

    deliver(&mut app, current - 1);
    app.update();
    let applied = replay(&app).snapshots_applied;

    deliver(&mut app, current - 3);
    app.update();

    let stats = replay(&app);
    assert_eq!(stats.dropped_stale, 1);
    assert_eq!(stats.snapshots_applied, applied, "a stale snapshot never reaches the world");
}

#[test]
fn a_snapshot_before_the_client_role_is_counted_at_the_door() {
    let mut app = client();
    app.world_mut().remove_resource::<LocalClientPlayer>();
    app.update();
    deliver(&mut app, 0);
    app.update();

    assert_eq!(replay(&app).dropped_before_handshake, 1);
    assert_eq!(replay(&app).snapshots_applied, 0);
}

// ── TickCost ─────────────────────────────────────────────────────────────────

#[test]
fn tick_cost_counts_replayed_ticks_as_ticks() {
    let mut app = client();
    sync(&mut app);
    let before = app.world().resource::<TickCost>().ticks;

    let lead = app.world().resource::<ClientTickBuffer>().target_replay_distance;
    let current = app.world().resource::<CurrentTick>().0;
    deliver(&mut app, current - lead);
    app.update();

    let after = app.world().resource::<TickCost>().ticks;
    assert!(
        after - before > 1,
        "one frame that replays the lead runs more than one tick: {before} -> {after}"
    );
    assert!(app.world().resource::<TickCost>().spent > Duration::ZERO);
}

// ── HealthWarnings ───────────────────────────────────────────────────────────

#[test]
fn a_client_that_mints_a_tracked_id_is_caught_once_and_counted_after() {
    let mut app = client();
    sync(&mut app);

    // Something on the client hands out an id. Only the authority may.
    app.world_mut().resource_mut::<TickTrackedEntityCounter>().0 += 1;
    app.update();
    assert_eq!(app.world().resource::<HealthWarnings>().client_minted_tracked_id, 1);

    app.world_mut().resource_mut::<TickTrackedEntityCounter>().0 += 1;
    app.update();
    assert_eq!(app.world().resource::<HealthWarnings>().client_minted_tracked_id, 2);
}

#[test]
fn a_counter_moved_by_a_snapshot_is_not_a_client_minted_id() {
    let mut app = client();
    sync(&mut app);
    let current = app.world().resource::<CurrentTick>().0;

    // The host spawned something with a higher id; applying it raises the counter here.
    let mut snapshot = snapshot_matching(&mut app, current - 1);
    let index = app
        .world()
        .resource::<TickedComponentRegistry>()
        .wire_index_of::<Pos>()
        .unwrap();
    if let SnapshotBody::Full(body) = &mut snapshot.body {
        body.put(EntityRecord::new(41).with(index, &Pos(9)));
    }
    app.world_mut().trigger(ReceivedNetworkSnapshot(snapshot));
    app.update();
    app.update();

    assert_eq!(
        app.world().resource::<HealthWarnings>().client_minted_tracked_id,
        0,
        "the authority moved the counter, which is its job"
    );
}

#[test]
fn a_snapshot_older_than_the_history_window_is_counted() {
    let mut app = client();
    app.insert_resource(HistoryBufferTicks(4));
    sync(&mut app);
    for _ in 0..16 {
        app.update();
    }
    let current = app.world().resource::<CurrentTick>().0;
    let last_applied = 0;
    // Newer than the last applied snapshot, so not stale; older than anything in history.
    let tick = last_applied + 1;
    assert!(tick + 4 < current);
    // Built without capturing into this client's history, which `deliver` does and which would
    // itself extend the history back to `tick`.
    let mut snapshot = SnapshotPacket::full(tick, build_full_body(app.world_mut(), tick));
    snapshot.your_margin = 2;
    app.world_mut().trigger(ReceivedNetworkSnapshot(snapshot));
    app.update();

    assert_eq!(
        app.world().resource::<HealthWarnings>().snapshot_older_than_history,
        1
    );
}

// ── InputStats and SnapshotStats ─────────────────────────────────────────────

#[test]
fn the_host_counts_inputs_and_flags_the_late_ones() {
    let mut app = host();
    for _ in 0..4 {
        app.update();
    }
    let current = app.world().resource::<CurrentTick>().0;
    assert!(current >= 2);

    app.world_mut().trigger(ReceivedNetworkInput {
        sender: LOCAL,
        tick: current + 2,
        input: Input { forward: true },
    });
    app.world_mut().trigger(ReceivedNetworkInput {
        sender: LOCAL,
        tick: current - 1,
        input: Input { forward: true },
    });

    let stats = *app.world().resource::<InputStats>();
    assert_eq!(stats.received, 2);
    assert_eq!(stats.late, 1);
}

#[test]
fn the_host_counts_every_snapshot_it_broadcasts() {
    let mut app = host();
    for _ in 0..8 {
        app.update();
    }
    let stats = *app.world().resource::<SnapshotStats>();
    let ticks = app.world().resource::<CurrentTick>().0;
    assert!(ticks > 0);
    assert_eq!(stats.sent, ticks, "one broadcast per tick until send rates land");
    assert!(stats.bytes > 0, "the server encodes the packet itself and counts it");
    assert_eq!(stats.oversize, 0);
}
