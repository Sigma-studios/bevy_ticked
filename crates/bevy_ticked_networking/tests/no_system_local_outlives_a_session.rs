//! State that used to live in system `Local`s, which outlive every session.
//!
//! The doors reset resources, and a `Local` is not one: it lives as long as the app. Five of them
//! in this stack held session state and leaked it into the next session; they are registered
//! session resources now, and the source guards forbid new ones. Each test runs the same final
//! step on a reused app and on a fresh one and compares.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy_ticked::prelude::*;
use bevy_ticked::registry::TickedComponentRegistry;
use bevy_ticked::tracked_entity::TrackedIdAllocator;
use bevy_ticked_networking::messages::SendNetworkSnapshot;
use bevy_ticked_networking::prelude::*;
use bevy_ticked_networking::server::LocalServerPlayer;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
struct Input;

#[derive(Component, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Pos(i32);

#[derive(Resource, Clone, Default)]
struct Sent(Arc<Mutex<u32>>);

fn peer() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(TickedPlugin {
            source: TickSource::Hz(64.0),
            ..default()
        })
        .add_plugins(TickedServerPlugin::<Input>::new())
        .add_plugins(TickedClientPlugin::<Input>::new())
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            1.0 / 64.0,
        )))
        .register_networked_ticked_component::<Pos>("Pos")
        .init_resource::<Sent>()
        .add_observer(|_: On<SendNetworkSnapshot>, sent: Res<Sent>| {
            *sent.0.lock().unwrap() += 1;
        });
    app
}

fn sent(app: &App) -> u32 {
    *app.world().resource::<Sent>().0.lock().unwrap()
}

fn spawn_tracked(app: &mut App) {
    let id = app
        .world_mut()
        .resource_mut::<TrackedIdAllocator>()
        .next_authority();
    app.world_mut().spawn((id, Pos(1)));
}

/// `broadcast_snapshot`'s `Local<u32> passes_held`: a host that left while its clock was held
/// (a pause menu open — `Manual` is the game's and no door releases it) and hosts again, still
/// held, has its first held snapshot throttled by the count from the last session.
#[test]
fn a_held_hosting_is_not_throttled_by_the_last_one() {
    // Reused: host, hold for a few passes, leave, host again — the menu still open.
    let mut reused = peer();
    reused.insert_resource(LocalServerPlayer(1));
    for _ in 0..5 {
        reused.update();
    }
    reused
        .world_mut()
        .resource_mut::<TickHolds>()
        .hold(TickHoldReason::Manual);
    for _ in 0..5 {
        reused.update();
    }
    reused.world_mut().remove_resource::<LocalServerPlayer>();
    reused.update();
    assert!(
        reused
            .world()
            .resource::<TickHolds>()
            .holds(TickHoldReason::Manual),
        "precondition: the game's own hold survives a leave"
    );
    let before = sent(&reused);
    reused.insert_resource(LocalServerPlayer(1));
    for _ in 0..3 {
        reused.update();
    }
    let reused_sent = sent(&reused) - before;

    // Fresh: hosting starts with the clock already held.
    let mut fresh = peer();
    fresh.update();
    fresh
        .world_mut()
        .resource_mut::<TickHolds>()
        .hold(TickHoldReason::Manual);
    fresh.update();
    let before = sent(&fresh);
    fresh.insert_resource(LocalServerPlayer(1));
    for _ in 0..3 {
        fresh.update();
    }
    let fresh_sent = sent(&fresh) - before;

    println!(
        "first 3 held frames of a new hosting: reused sent {reused_sent}, fresh sent {fresh_sent}"
    );
    assert_eq!(
        reused_sent, fresh_sent,
        "the held-pass count of the last session throttled the first snapshot of this one"
    );
}

/// `ensure_initial_capture`'s `Local<bool> done`: tick 0 was captured once per app, and every
/// door clears the history and puts the clock back to 0, so a later session had no tick 0 — a
/// `ResetToTick(0)` or a step back to the start found nothing to restore.
///
/// A host keeps the world it stands in, so its door captures that world at tick 0; the second
/// hosting of a reused app has to do the same as the first hosting of a fresh one.
#[test]
fn every_session_captures_its_own_tick_zero() {
    let has_zero = |app: &App| {
        let registry = app.world().resource::<TickedComponentRegistry>().clone();
        registry.has_tick_captured(app.world(), 0)
    };

    let mut reused = peer();
    spawn_tracked(&mut reused);
    reused.update();
    assert!(
        has_zero(&reused),
        "precondition: the first world has tick 0"
    );
    reused.insert_resource(LocalServerPlayer(1));
    for _ in 0..10 {
        reused.update();
    }
    reused.world_mut().remove_resource::<LocalServerPlayer>();
    reused.update();
    // A new world, opened to others straight away.
    spawn_tracked(&mut reused);
    reused.insert_resource(LocalServerPlayer(1));
    reused.update();

    let mut fresh = peer();
    fresh.update();
    fresh.update();
    spawn_tracked(&mut fresh);
    fresh.insert_resource(LocalServerPlayer(1));
    fresh.update();

    println!(
        "tick 0 captured by the second hosting: reused {}, fresh {}",
        has_zero(&reused),
        has_zero(&fresh)
    );
    assert!(
        has_zero(&fresh),
        "precondition: a host captures the world it keeps"
    );
    assert!(
        has_zero(&reused),
        "a reused app has no tick-0 capture in its second session"
    );
}

/// `send_local_input`'s `Local<Option<u32>> last_sent_ack`: a client at rest sends a packet only
/// for an acknowledgement it has not sent. The next session's first ack can carry the same `seq`
/// as the last one of the previous session — both count from one — and went unsent.
#[test]
fn the_first_ack_of_a_session_is_sent_even_if_the_last_session_ended_on_it() {
    use bevy_ticked_networking::client::{LastAppliedSeq, LastSentAck, LocalClientPlayer};

    let mut app = peer();
    app.insert_resource(LocalClientPlayer(2));
    app.update();
    app.world_mut().resource_mut::<LastSentAck>().0 = Some(1);
    app.world_mut().remove_resource::<LocalClientPlayer>();
    app.update();
    app.insert_resource(LocalClientPlayer(2));
    app.update();
    assert_eq!(
        app.world().resource::<LastSentAck>().0,
        None,
        "the last session's ack is not this one's"
    );
    assert_eq!(app.world().resource::<LastAppliedSeq>().0, None);
}
