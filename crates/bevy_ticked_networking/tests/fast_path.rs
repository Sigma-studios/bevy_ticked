//! A snapshot that agrees with the prediction costs nothing; one that disagrees costs a
//! bounded amount per frame.

use std::time::Duration;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy_ticked::prelude::*;
use bevy_ticked::registry::TickedComponentRegistry;
use bevy_ticked::tracked_entity::TickTrackedEntity;
use bevy_ticked_networking::client::{AwaitingReplay, ClientTickBuffer, LocalClientPlayer};
use bevy_ticked_networking::diagnostics::{ReplayStats, SnapshotStats};
use bevy_ticked_networking::messages::ReceivedNetworkSnapshot;
use bevy_ticked_networking::prelude::*;
use bevy_ticked_networking::replication::ReplicationMode;
use bevy_ticked_networking::server::LocalServerPlayer;
use bevy_ticked_networking::snapshot::{EntityRecord, SnapshotBody, SnapshotPacket, build_full_body};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
struct Input;

#[derive(Component, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Pos(i32);

/// Written by the simulation every tick, so `Changed` fires once per tick it ran.
#[derive(Component, Clone, Copy, Debug, PartialEq)]
struct Beat(u64);

/// Not registered anywhere: advances once per tick the simulation ran, replays included.
#[derive(Component, Clone, Copy, Debug, PartialEq)]
struct Unregistered(u64);

#[derive(Resource, Default)]
struct Counts {
    sim_runs: u32,
    changed: u32,
    inserted: u32,
}

const LOCAL: u128 = 7;
const TICK: Duration = Duration::from_micros(15_625);

fn beat(mut bodies: Query<(&mut Beat, &mut Unregistered)>, mut counts: ResMut<Counts>) {
    counts.sim_runs += 1;
    for (mut beat, mut unregistered) in &mut bodies {
        beat.0 += 1;
        unregistered.0 += 1;
    }
}

fn count_changed(changed: Query<(), Changed<Beat>>, mut counts: ResMut<Counts>) {
    counts.changed += changed.iter().count() as u32;
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
        .init_resource::<Counts>()
        .register_networked_ticked_component::<Pos>("Pos")
        .register_ticked_component::<Beat>()
        .add_systems(TickedSimulation, beat)
        .add_systems(Update, count_changed)
        .add_observer(|_: On<Insert, Pos>, mut counts: ResMut<Counts>| counts.inserted += 1);
    app.insert_resource(LocalClientPlayer(LOCAL));
    app.update();
    app
}

fn wire_pos(app: &App) -> u16 {
    app.world()
        .resource::<TickedComponentRegistry>()
        .wire_index_of::<Pos>()
        .unwrap()
}

/// Host-shaped: what the client predicted for `tick`, from its own history.
fn matching(app: &mut App, tick: u64) -> SnapshotPacket {
    let mut packet = SnapshotPacket::full(tick, build_full_body(app.world_mut(), tick));
    packet.your_margin = 2;
    packet
}

fn placing(app: &mut App, tick: u64, pos: i32) -> SnapshotPacket {
    let mut packet = matching(app, tick);
    let index = wire_pos(app);
    if let SnapshotBody::Full(body) = &mut packet.body {
        body.put(EntityRecord::new(1).with(index, &Pos(pos)));
    }
    packet
}

fn deliver(app: &mut App, packet: SnapshotPacket) {
    app.world_mut().trigger(ReceivedNetworkSnapshot(packet));
}

/// A predicted body, synced, with a few ticks of history behind it.
fn synced() -> App {
    let mut app = client();
    let index = wire_pos(&app);
    let mut body = bevy_ticked_networking::snapshot::FullBody::default();
    body.put(EntityRecord::new(1).with(index, &Pos(0)));
    deliver(&mut app, SnapshotPacket::full(0, body));
    app.update();
    let entity = {
        let mut q = app.world_mut().query_filtered::<Entity, With<TickTrackedEntity>>();
        q.single(app.world()).unwrap()
    };
    app.world_mut()
        .entity_mut(entity)
        .insert((ReplicationMode::Predicted, Beat(0), Unregistered(0)));
    for _ in 0..12 {
        app.update();
    }
    app
}

fn lead(app: &App) -> u64 {
    app.world().resource::<ClientTickBuffer>().target_replay_distance
}

fn stats(app: &App) -> ReplayStats {
    *app.world().resource::<ReplayStats>()
}

#[test]
fn a_static_world_never_replays() {
    let mut app = synced();
    let before = stats(&app);
    for _ in 0..32 {
        let current = app.world().resource::<CurrentTick>().0;
        let behind = current - lead(&app);
        let packet = matching(&mut app, behind);
        deliver(&mut app, packet);
        app.update();
    }
    let after = stats(&app);
    assert_eq!(after.rollbacks, before.rollbacks);
    assert_eq!(after.skipped_identical - before.skipped_identical, 32);
    assert_eq!(after.snapshots_applied - before.snapshots_applied, 32);
}

#[test]
fn a_flipped_bit_in_a_snapshot_forces_a_replay() {
    let mut app = synced();
    let before = stats(&app);
    let current = app.world().resource::<CurrentTick>().0;
    let behind = current - lead(&app);
    let mut packet = matching(&mut app, behind);
    if let SnapshotBody::Full(body) = &mut packet.body {
        let record = &mut body.entities[0];
        record.bytes[0] ^= 0x01;
    }
    deliver(&mut app, packet);
    app.update();
    let after = stats(&app);
    assert_eq!(after.rollbacks, before.rollbacks + 1, "one bit, one correction");
    assert_eq!(after.skipped_identical, before.skipped_identical);
}

#[test]
fn a_local_misprediction_replays_exactly_once() {
    let mut app = synced();
    let before = stats(&app);
    let current = app.world().resource::<CurrentTick>().0;
    let behind = current - lead(&app);
    let packet = placing(&mut app, behind, 5);
    deliver(&mut app, packet);
    app.update();
    // The correction is in the history now, so the next snapshots agree with it.
    for _ in 0..8 {
        let current = app.world().resource::<CurrentTick>().0;
        let behind = current - lead(&app);
        let packet = placing(&mut app, behind, 5);
        deliver(&mut app, packet);
        app.update();
    }
    let after = stats(&app);
    assert_eq!(after.rollbacks, before.rollbacks + 1);
    assert_eq!(after.skipped_identical - before.skipped_identical, 8);
}

#[test]
fn unregistered_state_advances_once_per_tick_not_once_per_replay() {
    let mut app = synced();
    let ticks_before = app.world().resource::<CurrentTick>().0;
    let count_before = {
        let mut q = app.world_mut().query::<&Unregistered>();
        q.single(app.world()).unwrap().0
    };
    for _ in 0..16 {
        let current = app.world().resource::<CurrentTick>().0;
        let behind = current - lead(&app);
        let packet = matching(&mut app, behind);
        deliver(&mut app, packet);
        app.update();
    }
    let ticks = app.world().resource::<CurrentTick>().0 - ticks_before;
    let mut q = app.world_mut().query::<&Unregistered>();
    let count = q.single(app.world()).unwrap().0 - count_before;
    assert_eq!(
        count, ticks,
        "an unregistered counter used to advance once per replayed tick, seven times per real \
         one; it advances once per tick now"
    );
}

#[test]
fn changed_t_fires_once_per_tick_on_a_static_world() {
    let mut app = synced();
    app.world_mut().resource_mut::<Counts>().changed = 0;
    for _ in 0..16 {
        let current = app.world().resource::<CurrentTick>().0;
        let behind = current - lead(&app);
        let packet = matching(&mut app, behind);
        deliver(&mut app, packet);
        app.update();
    }
    assert_eq!(
        app.world().resource::<Counts>().changed,
        16,
        "Changed<Beat> fires once per frame, since each frame runs one tick and no replay"
    );
}

#[test]
fn an_on_insert_observer_fires_once_for_a_confirmed_spawn() {
    let mut app = synced();
    app.world_mut().resource_mut::<Counts>().inserted = 0;
    for _ in 0..8 {
        let current = app.world().resource::<CurrentTick>().0;
        let behind = current - lead(&app);
        let packet = matching(&mut app, behind);
        deliver(&mut app, packet);
        app.update();
    }
    assert_eq!(
        app.world().resource::<Counts>().inserted,
        0,
        "a confirmed body is not re-inserted by the snapshots that agree with it"
    );
    // A correction re-inserts once, for the one snapshot that changed it.
    let current = app.world().resource::<CurrentTick>().0;
    let behind = current - lead(&app);
    let packet = placing(&mut app, behind, 3);
    deliver(&mut app, packet);
    app.update();
    assert_eq!(app.world().resource::<Counts>().inserted, 1);
}

#[test]
fn a_burst_of_stale_snapshots_replays_at_most_max_ticks_per_frame() {
    let mut app = client();
    app.insert_resource(MaxTicksPerFrame(4));
    let index = wire_pos(&app);
    let mut body = bevy_ticked_networking::snapshot::FullBody::default();
    body.put(EntityRecord::new(1).with(index, &Pos(0)));
    deliver(&mut app, SnapshotPacket::full(0, body));
    app.update();
    let entity = {
        let mut q = app.world_mut().query_filtered::<Entity, With<TickTrackedEntity>>();
        q.single(app.world()).unwrap()
    };
    app.world_mut()
        .entity_mut(entity)
        .insert((ReplicationMode::Predicted, Beat(0), Unregistered(0)));
    for _ in 0..40 {
        app.update();
    }
    // A correction from far back: more ticks to replay than one frame may run.
    let current = app.world().resource::<CurrentTick>().0;
    let packet = placing(&mut app, current - 30, 9);
    deliver(&mut app, packet);

    let mut per_frame = Vec::new();
    let mut awaited = false;
    for _ in 0..12 {
        app.world_mut().resource_mut::<Counts>().sim_runs = 0;
        app.update();
        per_frame.push(app.world().resource::<Counts>().sim_runs);
        if app.world().contains_resource::<AwaitingReplay>() {
            awaited = true;
            assert!(
                app.world().resource::<TickHolds>().holds(TickHoldReason::Replaying),
                "the clock waits for the replay to finish"
            );
        }
    }
    assert!(awaited, "the replay spanned more than one frame: {per_frame:?}");
    assert!(
        per_frame.iter().all(|runs| *runs <= 4),
        "no frame ran more than MaxTicksPerFrame ticks: {per_frame:?}"
    );
    assert!(
        !app.world().contains_resource::<AwaitingReplay>(),
        "and it finished: {per_frame:?}"
    );
    assert!(!app.world().resource::<TickHolds>().holds(TickHoldReason::Replaying));
    assert!(
        stats(&app).ticks_replayed >= 30,
        "replayed ticks are counted across the frames: {:?}",
        stats(&app)
    );
}

#[test]
fn the_server_send_rate_divides_the_snapshot_stream() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(TickedPlugin {
            source: TickSource::Hz(64.0),
            ..default()
        })
        .add_plugins(TickedServerPlugin::<Input>::new().send_every(4))
        .insert_resource(TimeUpdateStrategy::ManualDuration(TICK))
        .register_networked_ticked_component::<Pos>("Pos");
    app.insert_resource(LocalServerPlayer(1));
    app.world_mut().spawn((TickTrackedEntity(1), Pos(0)));
    for _ in 0..65 {
        app.update();
    }
    let ticks = app.world().resource::<CurrentTick>().0;
    let sent = app.world().resource::<SnapshotStats>().sent;
    assert!(ticks >= 60);
    assert_eq!(
        sent,
        ticks / 4,
        "one snapshot every fourth tick: {sent} for {ticks} ticks"
    );
}
