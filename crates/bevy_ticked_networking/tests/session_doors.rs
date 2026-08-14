//! The tracked-entity index, and the three doors into and out of a session.
//!
//! `coop_zombies/docs/upstream-needs.md` T9, T12 and T7. What they have in common is that each was
//! being done by every consumer instead of once here, and each was being done slightly differently.

use std::time::Duration;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy_ticked::prelude::*;
use bevy_ticked::tracked_entity::TickTrackedEntity;
use bevy_ticked_networking::client::LocalClientPlayer;
use bevy_ticked_networking::input::InputQueue;
use bevy_ticked_networking::prelude::*;
use bevy_ticked_networking::server::{LocalServerPlayer, SnapshotRecipients};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
struct Input {
    forward: bool,
}

#[derive(Component, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Pos(i32);

fn peer() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(TickedPlugin {
            source: TickSource::Manual,
            ..default()
        })
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            1.0 / 64.0,
        )))
        .add_plugins(TickedServerPlugin::<Input>::new())
        .add_plugins(TickedClientPlugin::<Input>::new())
        .register_networked_ticked_component_as::<Pos>("Pos");
    app
}

// ── T9: the index ────────────────────────────────────────────────────────────

#[test]
fn a_tracked_entity_can_be_found_by_its_id() {
    let mut app = peer();
    let entity = app.world_mut().spawn((TickTrackedEntity(7), Pos(1))).id();
    app.update();

    assert_eq!(
        app.world().resource::<TrackedEntityIndex>().get(7),
        Some(entity),
        "the id every peer agrees on resolves to the entity this peer holds"
    );
    assert_eq!(app.world().resource::<TrackedEntityIndex>().get(8), None);
}

#[test]
fn despawning_a_tracked_entity_unindexes_it() {
    let mut app = peer();
    let entity = app.world_mut().spawn((TickTrackedEntity(7), Pos(1))).id();
    app.update();
    app.world_mut().despawn(entity);
    app.update();

    assert_eq!(app.world().resource::<TrackedEntityIndex>().get(7), None);
    assert!(app.world().resource::<TrackedEntityIndex>().is_empty());
}

/// The guard that stops the map going wrong in the one case that actually happens.
///
/// Ids are reissued, so the `Add` for the entity claiming an id can land before the `Remove` for
/// the entity giving it up. Removing unconditionally would unindex the live one and leave the map
/// insisting the id does not exist.
#[test]
fn an_id_reclaimed_by_a_new_entity_is_not_unindexed_by_the_old_one() {
    let mut app = peer();
    let old = app.world_mut().spawn((TickTrackedEntity(7), Pos(1))).id();
    app.update();

    let new = app.world_mut().spawn((TickTrackedEntity(7), Pos(2))).id();
    app.update();
    assert_eq!(app.world().resource::<TrackedEntityIndex>().get(7), Some(new));

    app.world_mut().despawn(old);
    app.update();

    assert_eq!(
        app.world().resource::<TrackedEntityIndex>().get(7),
        Some(new),
        "the live entity keeps the id the departing one happened to share"
    );
}

// ── T12: the door out ────────────────────────────────────────────────────────

#[test]
fn leaving_a_session_despawns_what_it_left_behind() {
    let mut app = peer();
    app.world_mut().insert_resource(LocalClientPlayer(1));
    app.update();

    app.world_mut().spawn((TickTrackedEntity(1), Pos(5)));
    app.world_mut().spawn((TickTrackedEntity(2), Pos(6)));
    app.update();

    app.world_mut().remove_resource::<LocalClientPlayer>();
    app.update();

    let mut tracked = app.world_mut().query::<&TickTrackedEntity>();
    assert_eq!(
        tracked.iter(app.world()).count(),
        0,
        "the departed session's entities do not stand around in the next world"
    );
}

#[test]
fn leaving_a_session_clears_the_input_queue_and_the_tick() {
    let mut app = peer();
    app.world_mut().insert_resource(LocalServerPlayer(1));
    app.update();

    app.world_mut()
        .resource_mut::<InputQueue<Input>>()
        .insert(40, 1, Input { forward: true });
    app.world_mut().insert_resource(CurrentTick(40));

    app.world_mut().remove_resource::<LocalServerPlayer>();
    app.update();

    assert!(
        app.world().resource::<InputQueue<Input>>().inputs.is_empty(),
        "inputs from the last session must not be applied to the next one"
    );
    assert_eq!(app.world().resource::<CurrentTick>().0, 0);
}

/// A peer that leaves before its first snapshot would otherwise stay paused for ever, waiting on a
/// session it is no longer in.
#[test]
fn leaving_before_the_first_snapshot_does_not_leave_the_clock_stopped() {
    let mut app = peer();
    app.world_mut().insert_resource(LocalClientPlayer(1));
    app.update();
    assert!(
        app.world().get_resource::<TicksPaused>().is_some(),
        "a joining client is paused until the host's world arrives"
    );

    app.world_mut().remove_resource::<LocalClientPlayer>();
    app.update();

    assert!(
        app.world().get_resource::<TicksPaused>().is_none(),
        "and un-paused when it turns out there is no host coming"
    );
}

// ── T7: not serialising into nowhere ─────────────────────────────────────────

/// Run one whole tick lifecycle, including `PostTick` where `broadcast_snapshot` lives.
///
/// `StepForward` will not do: it calls `advance_one_tick` straight from `PreUpdate` and never
/// enters the `TickedLoop` schedule, so nothing that hangs off `TickedSystems` runs at all.
fn run_tick(app: &mut App) {
    app.world_mut().run_schedule(TickedLoop);
}

#[test]
fn a_host_with_no_recipients_builds_no_snapshot() {
    let mut app = peer();
    let sent = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = sent.clone();
    app.add_observer(
        move |_: On<bevy_ticked_networking::messages::SendNetworkSnapshot>| {
            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        },
    );

    app.world_mut().insert_resource(LocalServerPlayer(1));
    app.world_mut().spawn((TickTrackedEntity(1), Pos(0)));
    app.world_mut().insert_resource(SnapshotRecipients(0));
    for _ in 0..4 {
        run_tick(&mut app);
    }
    assert_eq!(
        sent.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "nobody is listening, so nothing is serialised"
    );

    app.world_mut().insert_resource(SnapshotRecipients(1));
    for _ in 0..4 {
        run_tick(&mut app);
    }
    assert!(
        sent.load(std::sync::atomic::Ordering::Relaxed) > 0,
        "and the moment somebody is, it sends again"
    );
}

/// The compatibility property: a transport that never maintains the count behaves exactly as it
/// did before this existed.
#[test]
fn an_absent_recipient_count_means_send_anyway() {
    let mut app = peer();
    let sent = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = sent.clone();
    app.add_observer(
        move |_: On<bevy_ticked_networking::messages::SendNetworkSnapshot>| {
            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        },
    );

    app.world_mut().insert_resource(LocalServerPlayer(1));
    app.world_mut().spawn((TickTrackedEntity(1), Pos(0)));
    app.world_mut().remove_resource::<SnapshotRecipients>();
    for _ in 0..4 {
        run_tick(&mut app);
    }

    assert!(
        sent.load(std::sync::atomic::Ordering::Relaxed) > 0,
        "unknown is not zero"
    );
}
