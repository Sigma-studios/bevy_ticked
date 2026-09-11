//! A joining client, a user's pause, and the first snapshot: three things that used to share
//! one marker resource, so any of them could lift the others.

use std::time::Duration;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use bevy_ticked::prelude::*;
use bevy_ticked::registry::TickedComponentRegistry;
use bevy_ticked::tracked_entity::TickTrackedEntity;
use bevy_ticked_networking::client::LocalClientPlayer;
use bevy_ticked_networking::messages::ReceivedNetworkSnapshot;
use bevy_ticked_networking::prelude::*;
use bevy_ticked_networking::server::LocalServerPlayer;
use bevy_ticked_networking::snapshot::{SnapshotPacket, build_full_body};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
struct Input;

#[derive(Component, Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct Pos(i32);

const LOCAL: u128 = 7;

fn peer() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .add_plugins(TickedPlugin {
            source: TickSource::Hz(64.0),
            ..default()
        })
        .add_plugins(TickedClientPlugin::<Input>::new())
        .add_plugins(TickedServerPlugin::<Input>::new())
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_micros(15_625)))
        .register_networked_ticked_component::<Pos>("Pos");
    app.update();
    app
}

fn deliver_first_snapshot(app: &mut App) {
    app.world_mut().spawn((TickTrackedEntity(1), Pos(0)));
    let registry = app.world().resource::<TickedComponentRegistry>().clone();
    registry.capture_all(app.world_mut(), 0);
    let mut packet = SnapshotPacket::full(0, build_full_body(app.world_mut(), 0));
    packet.your_margin = 2;
    app.world_mut().trigger(ReceivedNetworkSnapshot(packet));
}

fn holds(app: &App) -> &TickHolds {
    app.world().resource::<TickHolds>()
}

#[test]
fn joining_holds_on_awaiting_sync_not_on_a_manual_pause() {
    let mut app = peer();
    app.insert_resource(LocalClientPlayer(LOCAL));
    app.update();
    assert!(holds(&app).held_only_by(TickHoldReason::AwaitingSync));
    assert!(!holds(&app).holds(TickHoldReason::Manual));
}

#[test]
fn a_user_pause_survives_the_first_snapshot() {
    let mut app = peer();
    app.world_mut()
        .resource_mut::<TickHolds>()
        .hold(TickHoldReason::Manual);
    app.insert_resource(LocalClientPlayer(LOCAL));
    app.update();
    assert!(holds(&app).holds(TickHoldReason::AwaitingSync));
    assert!(holds(&app).holds(TickHoldReason::Manual));

    deliver_first_snapshot(&mut app);
    app.update();

    assert!(
        !holds(&app).holds(TickHoldReason::AwaitingSync),
        "the first snapshot is what a joining client was waiting for"
    );
    assert!(
        holds(&app).holds(TickHoldReason::Manual),
        "the user's pause is not the first snapshot's to lift"
    );
    let before = app.world().resource::<CurrentTick>().0;
    for _ in 0..8 {
        app.update();
    }
    assert_eq!(
        app.world().resource::<CurrentTick>().0,
        before,
        "and the clock stays stopped while the pause menu is open"
    );
}

#[test]
fn leaving_releases_the_sessions_reasons_and_keeps_the_games() {
    let mut app = peer();
    app.insert_resource(LocalClientPlayer(LOCAL));
    app.update();
    app.world_mut()
        .resource_mut::<TickHolds>()
        .hold(TickHoldReason::Manual);
    app.world_mut().remove_resource::<LocalClientPlayer>();
    app.update();
    assert!(holds(&app).held_only_by(TickHoldReason::Manual));
}

#[test]
fn hosting_lifts_a_stale_wait_but_not_a_pause() {
    let mut app = peer();
    app.insert_resource(LocalClientPlayer(LOCAL));
    app.update();
    app.world_mut()
        .resource_mut::<TickHolds>()
        .hold(TickHoldReason::Manual);
    app.world_mut().remove_resource::<LocalClientPlayer>();
    app.insert_resource(LocalServerPlayer(LOCAL));
    app.update();
    assert!(holds(&app).held_only_by(TickHoldReason::Manual));
}

#[test]
fn a_paused_host_keeps_broadcasting_at_a_low_rate() {
    #[derive(Resource, Default)]
    struct Sent(u32);
    let mut app = peer();
    app.init_resource::<Sent>().add_observer(
        |_: On<bevy_ticked_networking::messages::SendNetworkSnapshot>, mut sent: ResMut<Sent>| {
            sent.0 += 1;
        },
    );
    app.insert_resource(LocalServerPlayer(LOCAL));
    app.update();
    for _ in 0..8 {
        app.update();
    }
    assert!(app.world().resource::<Sent>().0 >= 8, "one per tick while running");

    app.world_mut().resource_mut::<Sent>().0 = 0;
    app.world_mut()
        .resource_mut::<TickHolds>()
        .hold(TickHoldReason::Manual);
    for _ in 0..64 {
        app.update();
    }
    let while_held = app.world().resource::<Sent>().0;
    assert!(
        (1..=4).contains(&while_held),
        "a paused host still tells joiners about the world, but not sixty-four times a \
         second: sent {while_held} in a second"
    );
}
